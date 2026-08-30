//! On-disk run store and content-hash cache keys.
//!
//! Layout under `state_dir()/workflows/runs/<run_id>/`:
//! `run.json` (record), `nodes/<node_id>/{output.txt,meta.json,log.*,…}`.
//! Writes use the tmp+rename pattern from `product_announcements.rs`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::model::WorkflowDef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunStatus {
    Running,
    Paused,
    Cancelled,
    Done,
    /// Terminal: some nodes errored (tolerated by `skip_on_error`) or were
    /// blocked, while the rest completed (FR-5.2/FR-9.4).
    PartialFail,
    Error,
}

impl RunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Done => "done",
            Self::PartialFail => "partial_fail",
            Self::Error => "error",
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Done | Self::PartialFail | Self::Error
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodePhase {
    Pending,
    Running,
    Done,
    Error,
    /// Terminal: structurally or failure-derived skip. Free-text
    /// `skip_reason`: "disabled", "blocked: …", "upstream",
    /// "upstream_error", later "when_false: …".
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct NodeRecord {
    pub id: String,
    pub phase: NodePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub cached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub run_id: String,
    pub workflow_name: String,
    pub workflow_path: String,
    pub status: RunStatus,
    pub started_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix: Option<u64>,
    /// The workspace hosting this run's panes, if still alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub nodes: Vec<NodeRecord>,
}

/// Persisted per-node execution metadata (the cache half of the resume key).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub(crate) struct NodeMeta {
    pub config_hash: String,
    pub inputs_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Pool dispatch trace for pool-bound nodes, e.g. `pa(HTTP 401)→pb(ok)`
    /// (P3c). Absent for direct-bound and unbound nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_attempts: Option<String>,
}

pub(crate) fn runs_root() -> PathBuf {
    crate::config::state_dir().join("workflows").join("runs")
}

fn run_root_at(root: &Path, run_id: &str) -> PathBuf {
    root.join(run_id)
}

pub(crate) fn run_root(run_id: &str) -> PathBuf {
    run_root_at(&runs_root(), run_id)
}

fn node_root_at(root: &Path, run_id: &str, node_id: &str) -> PathBuf {
    run_root_at(root, run_id).join("nodes").join(node_id)
}

pub(crate) fn node_root(run_id: &str, node_id: &str) -> PathBuf {
    node_root_at(&runs_root(), run_id, node_id)
}

pub(crate) fn generate_run_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_SUFFIX: AtomicU64 = AtomicU64::new(1);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let suffix = NEXT_SUFFIX.fetch_add(1, Ordering::Relaxed);
    format!("r{millis}x{suffix}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Hash of the node's effectful configuration (presentation fields already
/// stripped by `cache_projection`).
pub(crate) fn node_config_hash(def: &WorkflowDef, node_id: &str) -> Option<String> {
    def.node(node_id).map(|node| {
        sha256_hex(
            serde_json::to_string(&node.cache_projection())
                .unwrap_or_default()
                .as_bytes(),
        )
    })
}

/// Hash of the upstream outputs a node consumes, in `after` order. Nodes
/// without dependencies all share the empty-inputs hash.
pub(crate) fn node_inputs_hash(
    def: &WorkflowDef,
    node_id: &str,
    output_hashes: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let node = def.node(node_id)?;
    let mut combined = String::new();
    for dep in &node.after {
        let hash = output_hashes
            .get(dep)
            .cloned()
            .unwrap_or_else(|| "<pending>".to_string());
        combined.push_str(&hash);
        combined.push('\n');
    }
    Some(sha256_hex(combined.as_bytes()))
}

pub(crate) fn output_hash(output: &str) -> String {
    sha256_hex(output.as_bytes())
}

pub(crate) fn save_json_atomic(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, json)?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn create_run_at(root: &Path, record: &RunRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(run_root_at(root, &record.run_id).join("nodes"))?;
    save_record_at(root, record)
}

pub(crate) fn create_run(record: &RunRecord) -> std::io::Result<()> {
    create_run_at(&runs_root(), record)
}

fn save_record_at(root: &Path, record: &RunRecord) -> std::io::Result<()> {
    save_json_atomic(&run_root_at(root, &record.run_id).join("run.json"), record)
}

pub(crate) fn save_record(record: &RunRecord) -> std::io::Result<()> {
    save_record_at(&runs_root(), record)
}

fn load_record_at(root: &Path, run_id: &str) -> Option<RunRecord> {
    let path = run_root_at(root, run_id).join("run.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn load_record(run_id: &str) -> Option<RunRecord> {
    load_record_at(&runs_root(), run_id)
}

fn load_all_records_at(root: &Path) -> Vec<RunRecord> {
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return records;
    };
    for entry in entries.flatten() {
        if let Some(run_id) = entry.file_name().to_str() {
            if let Some(record) = load_record_at(root, run_id) {
                records.push(record);
            }
        }
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.started_unix));
    records
}

pub(crate) fn load_all_records() -> Vec<RunRecord> {
    load_all_records_at(&runs_root())
}

/// Persist a completed node: final answer text plus cache metadata.
pub(crate) fn save_node_result(
    run_id: &str,
    node_id: &str,
    output: &str,
    meta: &NodeMeta,
) -> std::io::Result<String> {
    save_node_result_at(&runs_root(), run_id, node_id, output, meta)
}

fn save_node_result_at(
    root: &Path,
    run_id: &str,
    node_id: &str,
    output: &str,
    meta: &NodeMeta,
) -> std::io::Result<String> {
    let dir = node_root_at(root, run_id, node_id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("output.txt"), output)?;
    save_json_atomic(&dir.join("meta.json"), meta)?;
    Ok(output_hash(output))
}

/// Load a cached node result for resume: `(output, meta)` when the stored
/// hashes match the expected cache key.
pub(crate) fn load_cached_node(
    run_id: &str,
    node_id: &str,
    expected_config_hash: &str,
    expected_inputs_hash: &str,
) -> Option<(String, NodeMeta)> {
    load_cached_node_at(
        &runs_root(),
        run_id,
        node_id,
        expected_config_hash,
        expected_inputs_hash,
    )
}

fn load_cached_node_at(
    root: &Path,
    run_id: &str,
    node_id: &str,
    expected_config_hash: &str,
    expected_inputs_hash: &str,
) -> Option<(String, NodeMeta)> {
    let dir = node_root_at(root, run_id, node_id);
    let meta: NodeMeta =
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).ok()?).ok()?;
    if meta.config_hash != expected_config_hash || meta.inputs_hash != expected_inputs_hash {
        return None;
    }
    let output = std::fs::read_to_string(dir.join("output.txt")).ok()?;
    // Guard against a truncated write: the recorded artifact (when present)
    // must also exist.
    if let Some(artifact) = &meta.artifact {
        if !dir.join(artifact).exists() {
            return None;
        }
    }
    Some((output, meta))
}

/// Read a node's persisted meta (cache keys + cost/artifact) without the
/// cached output — used by the graph view projection.
pub(crate) fn load_node_meta(run_id: &str, node_id: &str) -> Option<NodeMeta> {
    let dir = node_root(run_id, node_id);
    serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).ok()?).ok()
}

/// Delete runs beyond `keep` (oldest first). Returns removed
/// `(run_id, workspace_id)` pairs — callers close the workspaces of runs
/// whose directories just disappeared.
pub(crate) fn prune_runs(keep: usize) -> Vec<(String, Option<String>)> {
    prune_runs_at(&runs_root(), keep)
}

fn prune_runs_at(root: &Path, keep: usize) -> Vec<(String, Option<String>)> {
    let mut records = load_all_records_at(root);
    if records.len() <= keep {
        return Vec::new();
    }
    // Head of the descending list is the newest; prune everything past it.
    // Non-terminal runs (still live in some server) are never deleted —
    // deleting a live run's directory would orphan its panes and cache.
    records
        .split_off(keep)
        .into_iter()
        .filter(|record| record.status.is_terminal())
        .map(|record| {
            let _ = std::fs::remove_dir_all(run_root_at(root, &record.run_id));
            (record.run_id, record.workspace_id)
        })
        .collect()
}

pub(crate) fn delete_run(run_id: &str) -> std::io::Result<()> {
    std::fs::remove_dir_all(run_root(run_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::WorkflowDef;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-workflow-runs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn record(run_id: &str, started: u64) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            workflow_name: "demo".to_string(),
            workflow_path: "/tmp/demo.aflow.json".to_string(),
            status: RunStatus::Done,
            started_unix: started,
            finished_unix: Some(started + 1),
            workspace_id: None,
            nodes: vec![NodeRecord {
                id: "a".to_string(),
                phase: NodePhase::Done,
                output_hash: Some("h".to_string()),
                error: None,
                cached: false,
                skip_reason: None,
            }],
        }
    }

    fn sample_def() -> WorkflowDef {
        WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "hello"},
                {"id": "b", "type": "agent", "runtime": "claude-code", "prompt": "{{a.output}}", "after": ["a"]}
            ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn config_hash_ignores_presentation_changes() {
        let def = sample_def();
        let base = node_config_hash(&def, "b").unwrap();
        let moved = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "hello"},
                {"id": "b", "type": "agent", "runtime": "claude-code", "prompt": "{{a.output}}", "after": ["a"], "position": {"x": 99, "y": 1}, "title": "moved", "visible": false, "timeout_ms": 60000}
            ]}"#,
        )
        .unwrap();
        assert_eq!(base, node_config_hash(&moved, "b").unwrap());

        let changed = WorkflowDef::parse(
            r#"{"name": "demo", "nodes": [
                {"id": "a", "type": "prompt_template", "template": "hello"},
                {"id": "b", "type": "agent", "runtime": "codex", "prompt": "{{a.output}}", "after": ["a"]}
            ]}"#,
        )
        .unwrap();
        assert_ne!(base, node_config_hash(&changed, "b").unwrap());
    }

    #[test]
    fn inputs_hash_follows_after_order() {
        let def = sample_def();
        let mut outputs = std::collections::HashMap::new();
        outputs.insert("a".to_string(), "aaaa".to_string());
        let with_a = node_inputs_hash(&def, "b", &outputs).unwrap();
        outputs.insert("a".to_string(), "bbbb".to_string());
        let with_b = node_inputs_hash(&def, "b", &outputs).unwrap();
        assert_ne!(with_a, with_b);
        assert_eq!(
            node_inputs_hash(&def, "a", &std::collections::HashMap::new()).unwrap(),
            node_inputs_hash(&def, "a", &outputs).unwrap()
        );
    }

    #[test]
    fn cache_round_trip_hits_and_misses() {
        let root = temp_root("cache");
        let meta = NodeMeta {
            config_hash: "cfg1".to_string(),
            inputs_hash: "in1".to_string(),
            exit_code: Some(0),
            ..Default::default()
        };
        let hash = save_node_result_at(&root, "run1", "n1", "result text", &meta).unwrap();
        assert_eq!(hash, output_hash("result text"));

        let hit = load_cached_node_at(&root, "run1", "n1", "cfg1", "in1").unwrap();
        assert_eq!(hit.0, "result text");
        assert_eq!(hit.1.exit_code, Some(0));

        assert!(load_cached_node_at(&root, "run1", "n1", "cfg2", "in1").is_none());
        assert!(load_cached_node_at(&root, "run1", "n1", "cfg1", "in2").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn record_round_trip() {
        let root = temp_root("record");
        let run_id = generate_run_id();
        create_run_at(&root, &record(&run_id, 1)).unwrap();
        assert_eq!(
            load_record_at(&root, &run_id).as_ref(),
            Some(&record(&run_id, 1))
        );
        assert_eq!(load_all_records_at(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pre_p3_record_without_skip_reason_parses() {
        // Records written before the Skipped phase carry no skip_reason.
        let legacy = r#"{
            "run_id": "r1", "workflow_name": "demo",
            "workflow_path": "/tmp/demo.aflow.json", "status": "error",
            "started_unix": 1, "finished_unix": 2,
            "nodes": [{"id": "a", "phase": "done", "output_hash": "h", "cached": false}]
        }"#;
        let parsed: RunRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.nodes[0].phase, NodePhase::Done);
        assert_eq!(parsed.nodes[0].skip_reason, None);
    }

    #[test]
    fn skipped_record_round_trips_with_reason() {
        let mut rec = record("r2", 5);
        rec.nodes[0].phase = NodePhase::Skipped;
        rec.nodes[0].skip_reason = Some("blocked: provider profile 'x' not found".to_string());
        let json = serde_json::to_string(&rec).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.nodes[0].phase, NodePhase::Skipped);
        assert_eq!(
            parsed.nodes[0].skip_reason.as_deref(),
            Some("blocked: provider profile 'x' not found")
        );
    }

    #[test]
    fn prune_keeps_newest_only() {
        let root = temp_root("prune");
        create_run_at(&root, &record("run-old", 1)).unwrap();
        create_run_at(&root, &record("run-new", 2)).unwrap();

        let removed = prune_runs_at(&root, 1);
        assert!(
            load_record_at(&root, "run-new").is_some(),
            "newest must survive"
        );
        assert!(
            removed.iter().any(|(id, _)| id == "run-old")
                || load_record_at(&root, "run-old").is_none(),
            "older run must be pruned"
        );
        assert_eq!(load_all_records_at(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
