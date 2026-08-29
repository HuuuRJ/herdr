//! Workflow API handlers.

use crate::api::schema::{
    ResponseResult, WorkflowCancelParams, WorkflowDeleteParams, WorkflowGetParams,
    WorkflowListParams, WorkflowNodeInfo, WorkflowPauseParams, WorkflowResumeParams,
    WorkflowRunInfo, WorkflowRunParams,
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
}
