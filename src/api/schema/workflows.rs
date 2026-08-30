//! Workflow wire types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunParams {
    /// Path to the `.aflow.json` file.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct WorkflowListParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowGetParams {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowPauseParams {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowResumeParams {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowCancelParams {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowDeleteParams {
    pub run_id: String,
}

/// One node's single-value binding edit (W12: the TUI inspector edits only
/// these fields; prompt text is edited in the file itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowNodePatch {
    pub node_id: String,
    /// Agent runtime id: claude-code | codex | grok-build | dsh | custom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Bound provider profile id; an empty string clears the binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_profile_id: Option<String>,
    /// Model id; an empty string clears it (falls back to the profile's
    /// first visible model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowUpdateParams {
    /// Path to the `.aflow.json` file.
    pub path: String,
    pub patches: Vec<WorkflowNodePatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowNodeInfo {
    pub id: String,
    /// pending | running | done | error | skipped
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why a skipped node was skipped ("disabled", "blocked: …",
    /// "upstream", "upstream_error").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    #[serde(default)]
    pub cached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunInfo {
    pub run_id: String,
    pub workflow_name: String,
    pub workflow_path: String,
    /// running | paused | cancelled | done | error | partial_fail
    pub status: String,
    pub started_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub nodes: Vec<WorkflowNodeInfo>,
}
