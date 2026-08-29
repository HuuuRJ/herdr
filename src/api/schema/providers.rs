//! Provider profile wire types.
//!
//! `ProviderProfile` is the persisted registry entry (stored verbatim in the
//! 0600 `providers.json`). API surfaces never return it directly — list/get
//! responses go through `ProviderProfileInfo`, which carries a masked key;
//! `provider.reveal` is the only method that returns the plaintext secret.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderProtocol {
    OpenaiCompat,
    Anthropic,
    Gemini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderPresetCategory {
    Official,
    CnOfficial,
    /// Synthetic category for the always-present "custom relay" pseudo
    /// preset appended to provider.presets responses.
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelSource {
    Fetched,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderModelEntry {
    pub id: String,
    #[serde(default = "super::common::default_true")]
    pub visible: bool,
    pub source: ProviderModelSource,
}

/// A persisted provider profile. `preset_id` is either a built-in preset id
/// or `custom`. The base URL only ever goes to the root (e.g. `/v1`);
/// endpoints are appended by `provider::url::join_url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    pub preset_id: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    /// Plaintext secret. Only ever leaves the process through the masked
    /// `ProviderProfileInfo` or the explicit `provider.reveal` method.
    pub api_key: String,
    #[serde(default)]
    pub models: Vec<ProviderModelEntry>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub is_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
    pub created_unix: u64,
}

fn default_weight() -> u32 {
    1
}

/// Masked profile shape returned by list/get.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderProfileInfo {
    pub id: String,
    pub name: String,
    pub preset_id: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key_masked: String,
    pub has_api_key: bool,
    #[serde(default)]
    pub models: Vec<ProviderModelEntry>,
    pub weight: u32,
    pub is_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderPresetInfo {
    pub id: String,
    pub name: String,
    pub category: ProviderPresetCategory,
    pub protocol: ProviderProtocol,
    pub base_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct ProviderListParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderGetParams {
    pub profile_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderCreateParams {
    pub name: String,
    #[serde(default)]
    pub preset_id: Option<String>,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<ProviderModelEntry>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Field updates are `None` = keep current value; `Some` = replace. An empty
/// `Some("")` for `api_key` is rejected — clearing a key is not an edit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderUpdateParams {
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProviderProtocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderDeleteParams {
    pub profile_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct ProviderPresetsParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderTestParams {
    pub profile_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderModelsFetchParams {
    pub profile_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderRevealParams {
    pub profile_id: String,
}

/// Result of a one-shot connectivity test (display-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderTestResult {
    pub ok: bool,
    /// HTTP status when the server answered, `None` on transport failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Result of a model list fetch, after merge into the profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProviderModelsFetchResult {
    pub models: Vec<ProviderModelEntry>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}
