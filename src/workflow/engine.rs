//! Pure per-run scheduling state machine.
//!
//! The engine tracks node phases and computes dispatch decisions; it never
//! touches panes, processes, or the App. The App-facing glue in
//! `crate::app::workflow` performs the effects and reports completions back.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::model::WorkflowDef;
use super::runs::{NodePhase, NodeRecord, RunRecord, RunStatus};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct EngineNode {
    pub phase: NodePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub cached: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct EngineRun {
    pub def: WorkflowDef,
    pub run_id: String,
    pub workflow_path: String,
    pub status: RunStatus,
    pub workspace_id: Option<String>,
    pub nodes: HashMap<String, EngineNode>,
    /// Output texts keyed by node id — the render source for prompts.
    #[serde(default)]
    pub outputs: HashMap<String, String>,
}

impl EngineRun {
    pub(crate) fn new(def: WorkflowDef, run_id: String, workflow_path: String) -> Self {
        let nodes = def
            .nodes
            .iter()
            .map(|node| {
                (
                    node.id.clone(),
                    EngineNode {
                        phase: NodePhase::Pending,
                        output_hash: None,
                        error: None,
                        cached: false,
                    },
                )
            })
            .collect();
        Self {
            def,
            run_id,
            workflow_path,
            status: RunStatus::Running,
            workspace_id: None,
            nodes,
            outputs: HashMap::new(),
        }
    }

    pub(crate) fn from_record(record: &RunRecord, def: WorkflowDef) -> Self {
        let mut run = Self::new(def, record.run_id.clone(), record.workflow_path.clone());
        run.status = record.status;
        run.workspace_id = record.workspace_id.clone();
        for node_record in &record.nodes {
            if let Some(node) = run.nodes.get_mut(&node_record.id) {
                node.phase = node_record.phase;
                node.output_hash = node_record.output_hash.clone();
                node.error = node_record.error.clone();
                node.cached = node_record.cached;
            }
        }
        run
    }

    /// Nodes whose dependencies are all Done and that may start now:
    /// respects pause and the concurrency budget.
    pub(crate) fn ready_nodes(&self, in_flight: usize, max_concurrent: usize) -> Vec<String> {
        if self.status != RunStatus::Running || in_flight >= max_concurrent {
            return Vec::new();
        }
        let mut ready: Vec<String> = self
            .def
            .nodes
            .iter()
            .filter(|node| {
                self.nodes
                    .get(&node.id)
                    .is_some_and(|state| state.phase == NodePhase::Pending)
                    && node.after.iter().all(|dep| {
                        self.nodes
                            .get(dep)
                            .is_some_and(|state| state.phase == NodePhase::Done)
                    })
            })
            .map(|node| node.id.clone())
            .collect();
        // Deterministic dispatch order (file order).
        ready.sort_by_key(|id| {
            self.def
                .nodes
                .iter()
                .position(|node| &node.id == id)
                .unwrap_or(usize::MAX)
        });
        ready.truncate(max_concurrent.saturating_sub(in_flight));
        ready
    }

    pub(crate) fn mark_running(&mut self, node_id: &str) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Running;
        }
    }

    /// Record a completed node. Returns the run outcome when the run reaches
    /// a terminal state.
    pub(crate) fn mark_done(
        &mut self,
        node_id: &str,
        output: String,
        output_hash: String,
    ) -> Option<RunStatus> {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Done;
            node.output_hash = Some(output_hash);
            node.error = None;
        }
        self.outputs.insert(node_id.to_string(), output);
        self.settle()
    }

    pub(crate) fn mark_error(&mut self, node_id: &str, error: String) -> Option<RunStatus> {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Error;
            node.error = Some(error);
        }
        self.status = RunStatus::Error;
        Some(RunStatus::Error)
    }

    pub(crate) fn mark_cached(&mut self, node_id: &str, output: String, output_hash: String) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Done;
            node.output_hash = Some(output_hash);
            node.cached = true;
        }
        self.outputs.insert(node_id.to_string(), output);
    }

    /// After any transition: all Done → Done; nothing runnable left while
    /// paused → stays Paused; error status is set by `mark_error`.
    pub(crate) fn settle(&mut self) -> Option<RunStatus> {
        if self.status.is_terminal() {
            return Some(self.status);
        }
        if self
            .nodes
            .values()
            .all(|node| node.phase == NodePhase::Done)
        {
            self.status = RunStatus::Done;
            return Some(RunStatus::Done);
        }
        None
    }

    pub(crate) fn pause(&mut self) {
        if self.status == RunStatus::Running {
            self.status = RunStatus::Paused;
        }
    }

    pub(crate) fn resume(&mut self) {
        if self.status == RunStatus::Paused {
            self.status = RunStatus::Running;
        }
    }

    pub(crate) fn cancel(&mut self) {
        if !self.status.is_terminal() {
            self.status = RunStatus::Cancelled;
        }
    }

    pub(crate) fn record(&self, started_unix: u64, finished_unix: Option<u64>) -> RunRecord {
        RunRecord {
            run_id: self.run_id.clone(),
            workflow_name: self.def.name.clone(),
            workflow_path: self.workflow_path.clone(),
            status: self.status,
            started_unix,
            finished_unix,
            workspace_id: self.workspace_id.clone(),
            nodes: self
                .def
                .nodes
                .iter()
                .filter_map(|node| {
                    self.nodes.get(&node.id).map(|state| NodeRecord {
                        id: node.id.clone(),
                        phase: state.phase,
                        output_hash: state.output_hash.clone(),
                        error: state.error.clone(),
                        cached: state.cached,
                    })
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::runs::output_hash;

    fn two_node_run() -> EngineRun {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "hello"},
                {"id": "b", "type": "agent", "runtime": "claude-code", "prompt": "{{a.output}}", "after": ["a"]}
            ]}"#,
        )
        .unwrap();
        EngineRun::new(def, "r1".to_string(), "/tmp/demo.aflow.json".to_string())
    }

    #[test]
    fn dispatch_respects_dependencies_and_budget() {
        let mut run = two_node_run();
        // Only "a" is ready initially.
        assert_eq!(run.ready_nodes(0, 3), vec!["a"]);
        run.mark_running("a");
        assert!(run.ready_nodes(1, 3).is_empty(), "b waits on a");

        run.mark_done("a", "hello".to_string(), output_hash("hello"));
        assert_eq!(run.ready_nodes(0, 3), vec!["b"]);

        // Concurrency budget: one free slot still dispatches b; zero free
        // slots dispatch nothing.
        assert_eq!(run.ready_nodes(2, 3), vec!["b"]);
        assert!(run.ready_nodes(3, 3).is_empty());

        run.mark_running("b");
        assert!(run.ready_nodes(1, 3).is_empty());
    }

    #[test]
    fn all_done_completes_run() {
        let mut run = two_node_run();
        run.mark_running("a");
        assert_eq!(run.mark_done("a", "x".to_string(), output_hash("x")), None);
        run.mark_running("b");
        assert_eq!(
            run.mark_done("b", "y".to_string(), output_hash("y")),
            Some(RunStatus::Done)
        );
        assert_eq!(run.status, RunStatus::Done);
    }

    #[test]
    fn error_fails_run_and_records_message() {
        let mut run = two_node_run();
        run.mark_running("a");
        assert_eq!(
            run.mark_error("a", "exit 2".to_string()),
            Some(RunStatus::Error)
        );
        assert_eq!(run.status, RunStatus::Error);
        // No further dispatch after failure.
        assert!(run.ready_nodes(0, 3).is_empty());
        assert!(run.nodes.get("a").unwrap().error.as_deref() == Some("exit 2"));
    }

    #[test]
    fn pause_blocks_dispatch_resume_restores() {
        let mut run = two_node_run();
        run.pause();
        assert_eq!(run.status, RunStatus::Paused);
        assert!(run.ready_nodes(0, 3).is_empty());
        run.resume();
        assert_eq!(run.ready_nodes(0, 3), vec!["a"]);
    }

    #[test]
    fn cancel_is_terminal_and_idempotent() {
        let mut run = two_node_run();
        run.cancel();
        assert_eq!(run.status, RunStatus::Cancelled);
        run.cancel();
        assert_eq!(run.status, RunStatus::Cancelled);
        assert!(run.ready_nodes(0, 3).is_empty());
    }

    #[test]
    fn cached_nodes_count_as_done() {
        let mut run = two_node_run();
        run.mark_cached("a", "hello".to_string(), output_hash("hello"));
        assert!(run.nodes.get("a").unwrap().cached);
        assert_eq!(run.ready_nodes(0, 3), vec!["b"]);
    }

    #[test]
    fn record_round_trip_preserves_state() {
        let mut run = two_node_run();
        run.mark_running("a");
        run.mark_done("a", "x".to_string(), output_hash("x"));
        let record = run.record(7, None);
        let restored = EngineRun::from_record(&record, run.def.clone());
        assert_eq!(restored.status, RunStatus::Running);
        assert_eq!(restored.nodes.get("a").unwrap().phase, NodePhase::Done);
        assert_eq!(restored.nodes.get("b").unwrap().phase, NodePhase::Pending);
        // Outputs are not persisted in the record (they live per-node on
        // disk); resume reloads them from the cache.
        assert!(restored.outputs.is_empty());
    }
}
