//! Workflow API handlers.

use crate::api::schema::{
    ResponseResult, WorkflowCancelParams, WorkflowDeleteParams, WorkflowGetParams,
    WorkflowListParams, WorkflowNodeInfo, WorkflowNodePatch, WorkflowPauseParams,
    WorkflowResumeParams, WorkflowRunInfo, WorkflowRunParams, WorkflowUpdateParams,
};
use crate::app::workflow::node_phase_str;
use crate::workflow::runs::{self, RunRecord};

use super::responses::{encode_error, encode_success};
use super::App;

fn record_not_found(id: String, run_id: &str) -> String {
    encode_error(
        id,
        "workflow_run_not_found",
        format!("workflow run {run_id} not found"),
    )
}

fn run_info(record: &RunRecord) -> WorkflowRunInfo {
    WorkflowRunInfo {
        run_id: record.run_id.clone(),
        workflow_name: record.workflow_name.clone(),
        workflow_path: record.workflow_path.clone(),
        status: record.status.as_str().to_string(),
        started_unix: record.started_unix,
        finished_unix: record.finished_unix,
        workspace_id: record.workspace_id.clone(),
        nodes: record
            .nodes
            .iter()
            .map(|node| WorkflowNodeInfo {
                id: node.id.clone(),
                phase: node_phase_str(node.phase).to_string(),
                error: node.error.clone(),
                skip_reason: node.skip_reason.clone(),
                cached: node.cached,
            })
            .collect(),
    }
}

impl App {
    pub(super) fn handle_workflow_run(&mut self, id: String, params: WorkflowRunParams) -> String {
        match self.start_workflow_run(&params.path) {
            Ok(run_id) => {
                let workspace_id = self
                    .workflow_live_record(&run_id)
                    .and_then(|record| record.workspace_id.clone());
                encode_success(
                    id,
                    ResponseResult::WorkflowStarted {
                        run_id,
                        workspace_id,
                    },
                )
            }
            Err(err) => encode_error(id, "workflow_run_failed", err),
        }
    }

    pub(super) fn handle_workflow_list(
        &mut self,
        id: String,
        _params: WorkflowListParams,
    ) -> String {
        let mut records = runs::load_all_records();
        // Overlay live in-memory state over the persisted snapshot.
        let live_ids: Vec<String> = self.workflow_runs.keys().cloned().collect();
        for run_id in live_ids {
            if let Some(live) = self.workflow_live_record(&run_id) {
                match records.iter_mut().find(|record| record.run_id == run_id) {
                    Some(record) => *record = live,
                    None => records.insert(0, live),
                }
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.started_unix));
        let infos: Vec<WorkflowRunInfo> = records.iter().map(run_info).collect();
        encode_success(id, ResponseResult::WorkflowList { runs: infos })
    }

    pub(super) fn handle_workflow_get(&mut self, id: String, params: WorkflowGetParams) -> String {
        let record = self
            .workflow_live_record(&params.run_id)
            .or_else(|| runs::load_record(&params.run_id));
        match record {
            Some(record) => encode_success(
                id,
                ResponseResult::WorkflowRun {
                    run: run_info(&record),
                },
            ),
            None => record_not_found(id, &params.run_id),
        }
    }

    pub(super) fn handle_workflow_pause(
        &mut self,
        id: String,
        params: WorkflowPauseParams,
    ) -> String {
        match self.pause_workflow_run(&params.run_id) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(err) => encode_error(id, "workflow_pause_failed", err),
        }
    }

    pub(super) fn handle_workflow_resume(
        &mut self,
        id: String,
        params: WorkflowResumeParams,
    ) -> String {
        match self.resume_workflow_run(&params.run_id) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(err) => encode_error(id, "workflow_resume_failed", err),
        }
    }

    pub(super) fn handle_workflow_cancel(
        &mut self,
        id: String,
        params: WorkflowCancelParams,
    ) -> String {
        match self.cancel_workflow_run(&params.run_id) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(err) => encode_error(id, "workflow_cancel_failed", err),
        }
    }

    pub(super) fn handle_workflow_delete(
        &mut self,
        id: String,
        params: WorkflowDeleteParams,
    ) -> String {
        match self.delete_workflow_run(&params.run_id) {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(err) => encode_error(id, "workflow_delete_failed", err),
        }
    }

    pub(super) fn handle_workflow_update(
        &mut self,
        id: String,
        params: WorkflowUpdateParams,
    ) -> String {
        match update_workflow_file(&params.path, &params.patches) {
            Ok(()) => encode_success(id, ResponseResult::WorkflowUpdated { path: params.path }),
            Err(err) => encode_error(id, "workflow_update_failed", err),
        }
    }
}

/// Apply single-value node patches to the parsed workflow JSON tree. An
/// empty string clears an optional binding; unknown nodes are rejected.
/// The tree is patched in place and never round-tripped through the typed
/// model, so unknown JSON fields and key order survive the edit.
fn apply_workflow_patches(
    root: &mut serde_json::Value,
    patches: &[WorkflowNodePatch],
) -> Result<(), String> {
    let nodes = root
        .get_mut("nodes")
        .and_then(|nodes| nodes.as_array_mut())
        .ok_or_else(|| "workflow file has no nodes array".to_string())?;
    for patch in patches {
        let node = nodes
            .iter_mut()
            .find(|node| node.get("id").and_then(|id| id.as_str()) == Some(patch.node_id.as_str()))
            .ok_or_else(|| format!("node '{}' not found", patch.node_id))?;
        let obj = node
            .as_object_mut()
            .ok_or_else(|| format!("node '{}' is not an object", patch.node_id))?;
        let optional_binding = |value: &Option<String>| {
            value.as_ref().map(|value| {
                if value.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(value.clone())
                }
            })
        };
        let assignments: [(&str, Option<serde_json::Value>); 6] = [
            (
                "runtime",
                patch.runtime.clone().map(serde_json::Value::String),
            ),
            (
                "provider_profile_id",
                optional_binding(&patch.provider_profile_id),
            ),
            ("model", optional_binding(&patch.model)),
            (
                "timeout_ms",
                patch
                    .timeout_ms
                    .map(|value| serde_json::Value::Number(value.into())),
            ),
            ("visible", patch.visible.map(serde_json::Value::Bool)),
            ("enabled", patch.enabled.map(serde_json::Value::Bool)),
        ];
        for (field, value) in assignments {
            if let Some(value) = value {
                if value.is_null() {
                    obj.remove(field);
                } else {
                    obj.insert(field.to_string(), value);
                }
            }
        }
    }
    Ok(())
}

fn update_workflow_file(path: &str, patches: &[WorkflowNodePatch]) -> Result<(), String> {
    // Same cwd rule as workflow.run: resolve client-relative paths against
    // the server cwd.
    let owned_path = std::path::absolute(path)
        .map_err(|err| format!("invalid workflow path {path}: {err}"))?
        .to_string_lossy()
        .into_owned();
    let path = owned_path.as_str();
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read workflow file {path}: {err}"))?;
    let mut root: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("invalid workflow JSON: {err}"))?;
    apply_workflow_patches(&mut root, patches)?;
    // Validate a typed copy of the patched tree before writing it back.
    let def_text = serde_json::to_string(&root)
        .map_err(|err| format!("failed to serialize workflow: {err}"))?;
    crate::workflow::model::WorkflowDef::parse(&def_text)?;
    crate::workflow::runs::save_json_atomic(std::path::Path::new(path), &root)
        .map_err(|err| format!("failed to write workflow file: {err}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(node_id: &str) -> WorkflowNodePatch {
        WorkflowNodePatch {
            node_id: node_id.to_string(),
            runtime: None,
            provider_profile_id: None,
            model: None,
            timeout_ms: None,
            visible: None,
            enabled: None,
        }
    }

    #[test]
    fn patches_apply_and_clear_and_preserve_unknown_fields() {
        let mut root: serde_json::Value = serde_json::from_str(
            r#"{"name": "demo", "futureField": {"x": 1}, "nodes": [
                {"id": "a", "type": "agent", "runtime": "claude-code", "prompt": "p",
                 "providerProfileId": "legacy-alias", "skillIds": ["s1"]}
            ]}"#,
        )
        .unwrap();
        let mut p = patch("a");
        p.runtime = Some("grok-build".to_string());
        p.provider_profile_id = Some("p123".to_string());
        p.timeout_ms = Some(5000);
        p.visible = Some(false);
        apply_workflow_patches(&mut root, &[p]).unwrap();
        let node = &root["nodes"][0];
        assert_eq!(node["runtime"], "grok-build");
        assert_eq!(node["provider_profile_id"], "p123");
        assert_eq!(node["timeout_ms"], 5000);
        assert_eq!(node["visible"], false);
        // Unknown fields at every level survive.
        assert_eq!(root["futureField"]["x"], 1);
        assert_eq!(node["skillIds"][0], "s1");

        // Empty string clears the binding.
        let mut clear = patch("a");
        clear.provider_profile_id = Some(String::new());
        apply_workflow_patches(&mut root, &[clear]).unwrap();
        assert!(root["nodes"][0].get("provider_profile_id").is_none());
    }

    #[test]
    fn unknown_node_is_rejected() {
        let mut root: serde_json::Value = serde_json::from_str(
            r#"{"name": "d", "nodes": [{"id": "a", "type": "prompt_template", "template": "t"}]}"#,
        )
        .unwrap();
        let err = apply_workflow_patches(&mut root, &[patch("ghost")]).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn invalid_results_are_rejected_before_write() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-wf-update-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("wf.aflow.json");
        std::fs::write(
            &path,
            r#"{"name": "demo", "nodes": [{"id": "a", "type": "agent", "runtime": "claude-code", "prompt": "p"}]}"#,
        )
        .unwrap();
        // An empty runtime string is not a valid variant: validation rejects
        // the result before anything is written.
        let mut clear = patch("a");
        clear.runtime = Some(String::new());
        let err = update_workflow_file(path.to_str().unwrap(), &[clear]).unwrap_err();
        assert!(err.contains("invalid workflow"), "{err}");
        // File is untouched by the failed update.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("claude-code"));
        // A valid edit round-trips.
        let mut ok = patch("a");
        ok.model = Some("glm-4.7".to_string());
        update_workflow_file(path.to_str().unwrap(), &[ok]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("glm-4.7"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
