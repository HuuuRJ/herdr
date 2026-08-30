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
    /// Why a Skipped node was skipped ("disabled", "blocked: …",
    /// "upstream", "upstream_error", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
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
                        skip_reason: None,
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
                node.skip_reason = node_record.skip_reason.clone();
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
    /// a terminal state. A late completion for an already-terminal node
    /// (e.g. its pane died after the timeout kill) is ignored.
    pub(crate) fn mark_done(
        &mut self,
        node_id: &str,
        output: String,
        output_hash: String,
    ) -> Option<RunStatus> {
        let fresh = self.nodes.get(node_id).is_some_and(|node| {
            !matches!(
                node.phase,
                NodePhase::Done | NodePhase::Error | NodePhase::Skipped
            )
        });
        if !fresh {
            return None;
        }
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Done;
            node.output_hash = Some(output_hash);
            node.error = None;
        }
        self.outputs.insert(node_id.to_string(), output);
        self.settle()
    }

    /// Record a failed node. Non-tolerated failures fail the run outright
    /// (the App drains still-running siblings before finishing). Tolerated
    /// failures (`skip_on_error`, FR-5.2) leave the run going: downstream
    /// is skipped and the run settles into `partial_fail` once everything
    /// lands.
    pub(crate) fn mark_node_error(
        &mut self,
        node_id: &str,
        error: String,
        tolerated: bool,
    ) -> Option<RunStatus> {
        // Late failures (e.g. a timeout re-firing, a second death event)
        // must not overwrite a node that already reached a terminal phase.
        let fresh = self.nodes.get(node_id).is_some_and(|node| {
            !matches!(
                node.phase,
                NodePhase::Done | NodePhase::Error | NodePhase::Skipped
            )
        });
        if !fresh {
            return None;
        }
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Error;
            node.error = Some(error);
        }
        if !tolerated {
            self.status = RunStatus::Error;
            return Some(RunStatus::Error);
        }
        self.derive_skips();
        self.settle()
    }

    /// Record a structural skip (disabled node, blocked precondition such
    /// as a missing/disabled bound profile — FR-9.4). No spawn, no slot;
    /// the node keeps an empty output port and downstream cascades.
    pub(crate) fn mark_skipped(&mut self, node_id: &str, reason: &str) -> Option<RunStatus> {
        let fresh = self.nodes.get(node_id).is_some_and(|node| {
            !matches!(
                node.phase,
                NodePhase::Done | NodePhase::Error | NodePhase::Skipped
            )
        });
        if !fresh {
            return None;
        }
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Skipped;
            node.skip_reason = Some(reason.to_string());
        }
        self.outputs.insert(node_id.to_string(), String::new());
        self.derive_skips();
        self.settle()
    }

    /// Flip Pending nodes whose fate is sealed upstream: any dependency
    /// Skipped, or any dependency Error that declared `skip_on_error`.
    /// Cascades to a fixpoint; skipped nodes keep an empty output port so
    /// downstream renders never break (FR-9.4 empty-port semantics).
    fn derive_skips(&mut self) {
        loop {
            let mut flipped: Vec<(String, &'static str)> = Vec::new();
            for node in &self.def.nodes {
                let Some(state) = self.nodes.get(&node.id) else {
                    continue;
                };
                if state.phase != NodePhase::Pending {
                    continue;
                }
                for dep in &node.after {
                    let Some(dep_state) = self.nodes.get(dep) else {
                        continue;
                    };
                    let reason = match dep_state.phase {
                        NodePhase::Skipped => Some("upstream"),
                        NodePhase::Error => {
                            let tolerated = self.def.node(dep).is_some_and(|n| n.skip_on_error);
                            tolerated.then_some("upstream_error")
                        }
                        _ => None,
                    };
                    if let Some(reason) = reason {
                        flipped.push((node.id.clone(), reason));
                        break;
                    }
                }
            }
            if flipped.is_empty() {
                break;
            }
            for (id, reason) in flipped {
                if let Some(state) = self.nodes.get_mut(&id) {
                    state.phase = NodePhase::Skipped;
                    state.skip_reason = Some(reason.to_string());
                }
                self.outputs.insert(id, String::new());
            }
        }
    }

    pub(crate) fn mark_cached(&mut self, node_id: &str, output: String, output_hash: String) {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.phase = NodePhase::Done;
            node.output_hash = Some(output_hash);
            node.cached = true;
        }
        self.outputs.insert(node_id.to_string(), output);
    }

    /// After any transition: all nodes terminal → Done, or PartialFail when
    /// anything errored or was blocked (a "blocked: …" skip reason is a
    /// precondition failure, not a user-intended prune). Paused runs stay
    /// Paused; Error is set by `mark_node_error` on non-tolerated failures.
    pub(crate) fn settle(&mut self) -> Option<RunStatus> {
        if self.status.is_terminal() {
            return Some(self.status);
        }
        let all_terminal = self.nodes.values().all(|node| {
            matches!(
                node.phase,
                NodePhase::Done | NodePhase::Error | NodePhase::Skipped
            )
        });
        if !all_terminal {
            return None;
        }
        let any_error = self.nodes.values().any(|node| node.phase == NodePhase::Error);
        let any_blocked = self.nodes.values().any(|node| {
            node.phase == NodePhase::Skipped
                && node
                    .skip_reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("blocked"))
        });
        self.status = if any_error || any_blocked {
            RunStatus::PartialFail
        } else {
            RunStatus::Done
        };
        Some(self.status)
    }

    pub(crate) fn pause(&mut self) {
        if self.status == RunStatus::Running {
            self.status = RunStatus::Paused;
        }
    }

    pub(crate) fn resume(&mut self) {
        if matches!(
            self.status,
            RunStatus::Paused | RunStatus::Error | RunStatus::PartialFail
        ) {
            self.status = RunStatus::Running;
        }
    }

    /// After resume: drop Done nodes whose cache key no longer matches — the
    /// node was edited while the run was paused (W9), or an upstream node
    /// reset just now and the downstream inputs hash moved. Resets cascade
    /// through `<pending>` inputs hashes until stable. `cache_hit` reports
    /// whether `(node_id, config_hash, inputs_hash)` still has a stored
    /// result.
    pub(crate) fn invalidate_stale_done_nodes(
        &mut self,
        cache_hit: impl Fn(&str, &str, &str) -> bool,
    ) {
        loop {
            let mut reset_any = false;
            let done_ids: Vec<String> = self
                .nodes
                .iter()
                .filter(|(_, node)| node.phase == NodePhase::Done)
                .map(|(id, _)| id.clone())
                .collect();
            for node_id in done_ids {
                let config_hash = crate::workflow::runs::node_config_hash(&self.def, &node_id)
                    .unwrap_or_default();
                let output_hashes: HashMap<String, String> = self
                    .nodes
                    .iter()
                    .filter_map(|(id, node)| {
                        node.output_hash.clone().map(|hash| (id.clone(), hash))
                    })
                    .collect();
                let inputs_hash =
                    crate::workflow::runs::node_inputs_hash(&self.def, &node_id, &output_hashes)
                        .unwrap_or_default();
                if !cache_hit(&node_id, &config_hash, &inputs_hash) {
                    if let Some(node) = self.nodes.get_mut(&node_id) {
                        node.phase = NodePhase::Pending;
                        node.output_hash = None;
                        node.cached = false;
                    }
                    reset_any = true;
                }
            }
            if !reset_any {
                break;
            }
        }
    }

    pub(crate) fn cancel(&mut self) {
        if !self.status.is_terminal() {
            self.status = RunStatus::Cancelled;
        }
    }

    /// After an in-place def swap (resume re-read the workflow file), drop
    /// state for nodes that no longer exist — otherwise a reset retryable
    /// node idles as Pending forever, unreachable by dispatch and settle
    /// alike.
    pub(crate) fn retain_def_nodes(&mut self) {
        self.nodes.retain(|id, _| self.def.node(id).is_some());
        self.outputs.retain(|id, _| self.def.node(id).is_some());
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
                        skip_reason: state.skip_reason.clone(),
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
            run.mark_node_error("a", "exit 2".to_string(), false),
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

    #[test]
    fn resume_accepts_error_status() {
        let mut run = two_node_run();
        run.mark_node_error("a", "boom".to_string(), false);
        run.resume();
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn tolerated_error_skips_downstream_and_settles_partial() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "x", "skip_on_error": true},
                {"id": "b", "type": "prompt_template", "template": "{{a.output}}", "after": ["a"]},
                {"id": "c", "type": "prompt_template", "template": "{{b.output}}", "after": ["b"]},
                {"id": "side", "type": "prompt_template", "template": "s"}
            ]}"#,
        )
        .unwrap();
        let mut run = EngineRun::new(def, "r1".to_string(), "/tmp/x".to_string());
        // a fails tolerated; the side branch is untouched.
        assert_eq!(
            run.mark_node_error("a", "flaky".to_string(), true),
            None,
            "run keeps going while the side branch is pending"
        );
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.nodes.get("b").unwrap().phase, NodePhase::Skipped);
        assert_eq!(
            run.nodes.get("b").unwrap().skip_reason.as_deref(),
            Some("upstream_error")
        );
        // Cascade: c is skipped because b is.
        assert_eq!(run.nodes.get("c").unwrap().phase, NodePhase::Skipped);
        assert_eq!(
            run.nodes.get("c").unwrap().skip_reason.as_deref(),
            Some("upstream")
        );
        // Empty output ports keep downstream renders well-defined.
        assert_eq!(run.outputs.get("b").map(String::as_str), Some(""));

        // Side branch completes → all terminal → partial_fail.
        run.mark_done("side", "ok".to_string(), output_hash("ok"));
        assert_eq!(run.status, RunStatus::PartialFail);
    }

    #[test]
    fn blocked_skip_taints_run_as_partial() {
        let mut run = two_node_run();
        // The chain is fully settled by the skip: a blocked, b cascaded.
        assert_eq!(
            run.mark_skipped("a", "blocked: provider profile 'x' not found"),
            Some(RunStatus::PartialFail)
        );
        // b cascades as a plain upstream skip.
        assert_eq!(run.nodes.get("b").unwrap().phase, NodePhase::Skipped);
        assert_eq!(
            run.nodes.get("b").unwrap().skip_reason.as_deref(),
            Some("upstream")
        );
        // All terminal now — the blocked reason (not a user prune) settles
        // the run as partial_fail rather than done.
        assert_eq!(run.status, RunStatus::PartialFail);
    }

    #[test]
    fn disabled_only_skips_settle_done() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "off", "type": "prompt_template", "template": "t", "enabled": false},
                {"id": "mid", "type": "prompt_template", "template": "m", "after": ["off"]}
            ]}"#,
        )
        .unwrap();
        let mut run = EngineRun::new(def, "r1".to_string(), "/tmp/x".to_string());
        run.mark_skipped("off", "disabled");
        assert_eq!(run.nodes.get("mid").unwrap().phase, NodePhase::Skipped);
        // A user-intended prune is a clean done, not a partial failure.
        assert_eq!(run.status, RunStatus::Done);
    }

    #[test]
    fn resume_accepts_partial_fail_status() {
        let mut run = two_node_run();
        run.status = RunStatus::PartialFail;
        run.resume();
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn late_transitions_cannot_resurrect_terminal_nodes() {
        let mut run = two_node_run();
        run.mark_running("a");
        run.mark_node_error("a", "timed out".to_string(), false);
        // The pane dies after the timeout kill: its completion must not
        // flip the errored node back to Done.
        assert_eq!(run.mark_done("a", "late".to_string(), output_hash("late")), None);
        assert_eq!(run.nodes.get("a").unwrap().phase, NodePhase::Error);
        // A re-fired timeout must not overwrite the recorded error either.
        run.mark_node_error("a", "timed out again".to_string(), false);
        assert_eq!(
            run.nodes.get("a").unwrap().error.as_deref(),
            Some("timed out")
        );
        // Nor can a late skip claim a finished node.
        let mut run2 = two_node_run();
        run2.mark_running("a");
        run2.mark_done("a", "ok".to_string(), output_hash("ok"));
        run2.mark_skipped("a", "disabled");
        assert_eq!(run2.nodes.get("a").unwrap().phase, NodePhase::Done);
    }

    #[test]
    fn retain_def_nodes_drops_edited_out_state() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "x"},
                {"id": "leaf", "type": "prompt_template", "template": "l"}
            ]}"#,
        )
        .unwrap();
        let mut run = EngineRun::new(def, "r1".to_string(), "/tmp/x".to_string());
        run.mark_done("a", "ok".to_string(), output_hash("ok"));
        run.mark_node_error("leaf", "boom".to_string(), false);
        // While paused, the user deletes the failed leaf (W9 fix-and-rerun).
        run.def.nodes.retain(|node| node.id != "leaf");
        run.retain_def_nodes();
        assert!(run.nodes.get("leaf").is_none());
        assert!(run.outputs.get("leaf").is_none());
        // Resume: nothing retryable remains, every node is Done — the run
        // must settle instead of wedging live at Running.
        run.resume();
        let idle = run.nodes.values().all(|node| {
            !matches!(node.phase, NodePhase::Pending | NodePhase::Running)
        });
        assert!(idle, "no Pending/Running nodes survive the prune");
        assert_eq!(run.settle(), Some(RunStatus::Done));
    }

    #[test]
    fn invalidation_resets_edited_done_nodes_and_cascades() {
        let def = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "hello"},
                {"id": "b", "type": "prompt_template", "template": "{{a.output}}", "after": ["a"]},
                {"id": "c", "type": "prompt_template", "template": "{{b.output}}", "after": ["b"]}
            ]}"#,
        )
        .unwrap();
        let mut run = EngineRun::new(def, "r1".to_string(), "/tmp/x".to_string());
        run.mark_done("a", "hello".to_string(), output_hash("hello"));
        run.mark_done("b", "HELLO".to_string(), output_hash("HELLO"));
        run.mark_done("c", "done".to_string(), output_hash("done"));

        // Cache keys as persisted at pause time.
        let snapshot = run.clone();
        let keys_at_pause: HashMap<String, (String, String)> = ["a", "b", "c"]
            .into_iter()
            .map(|id| {
                let config =
                    crate::workflow::runs::node_config_hash(&snapshot.def, id).unwrap_or_default();
                let hashes: HashMap<String, String> = snapshot
                    .nodes
                    .iter()
                    .filter_map(|(nid, n)| n.output_hash.clone().map(|h| (nid.clone(), h)))
                    .collect();
                let inputs = crate::workflow::runs::node_inputs_hash(&snapshot.def, id, &hashes)
                    .unwrap_or_default();
                (id.to_string(), (config, inputs))
            })
            .collect();

        // The workflow file was edited while paused: b's template changed.
        run.def.nodes[1].template = Some("{{a.output}} EDITED".to_string());
        run.invalidate_stale_done_nodes(|id, config, inputs| {
            keys_at_pause
                .get(id)
                .is_some_and(|(c, i)| c == config && i == inputs)
        });

        // a untouched, b reset (config changed), c reset (inputs changed).
        assert_eq!(run.nodes.get("a").unwrap().phase, NodePhase::Done);
        assert_eq!(run.nodes.get("b").unwrap().phase, NodePhase::Pending);
        assert_eq!(run.nodes.get("c").unwrap().phase, NodePhase::Pending);
        // With an always-hit cache, nothing further resets.
        run.invalidate_stale_done_nodes(|_, _, _| true);
        assert_eq!(run.nodes.get("b").unwrap().phase, NodePhase::Pending);
        assert_eq!(run.nodes.get("c").unwrap().phase, NodePhase::Pending);
    }
}
