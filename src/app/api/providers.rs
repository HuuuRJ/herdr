//! Provider profile API handlers.
//!
//! CRUD methods run synchronously against the locked registry (a few KB on
//! disk; the locked read-modify-write is microseconds). `provider.test` and
//! `provider.models.fetch` shell out to curl with up to 15s timeouts, so they
//! follow the worktree deferred shape: snapshot the profile, spawn a thread,
//! and deliver the outcome through `AppEvent::ProviderHttpFinished` — which
//! both answers the waiting API caller and drives the TUI toast.

use crate::api::schema::{
    ProviderCreateParams, ProviderDeleteParams, ProviderGetParams, ProviderListParams,
    ProviderModelsFetchResult, ProviderPresetCategory, ProviderPresetInfo, ProviderPresetsParams,
    ProviderProfile, ProviderProfileInfo, ProviderProtocol, ProviderRevealParams,
    ProviderTestParams, ProviderUpdateParams, Request, ResponseResult, SuccessResponse,
};
use crate::events::{AppEvent, ProviderApiRequest, ProviderHttpOutcome};
use crate::provider::http::merge_models;
use crate::provider::{ProviderHttpKind, ProviderHttpResult};

use super::responses::{encode_error, encode_success};
use super::App;

/// Cap on concurrent provider HTTP requests (test + model fetches combined).
/// Mirrors the plugin command in-flight limit; provider requests dial paid
/// APIs, so a runaway fan-out costs real money.
const MAX_PROVIDER_HTTP_IN_FLIGHT: usize = 8;

fn send_provider_response(respond_to: &std::sync::mpsc::Sender<String>, response: String) {
    let _ = respond_to.send(response);
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Masked wire form of a profile — the only shape list/get ever return.
pub(crate) fn masked_info(profile: &ProviderProfile) -> ProviderProfileInfo {
    ProviderProfileInfo {
        id: profile.id.clone(),
        name: profile.name.clone(),
        preset_id: profile.preset_id.clone(),
        protocol: profile.protocol,
        base_url: profile.base_url.clone(),
        api_key_masked: crate::provider::url::mask_secret(&profile.api_key),
        has_api_key: !profile.api_key.is_empty(),
        models: profile.models.clone(),
        weight: profile.weight,
        is_disabled: profile.is_disabled,
        note: profile.note.clone(),
        created_unix: profile.created_unix,
    }
}

fn validate_base_url(base_url: &str) -> Result<(), String> {
    if base_url.trim().is_empty() {
        return Err("base URL is required".to_string());
    }
    if !base_url.starts_with("https://") && !base_url.starts_with("http://") {
        return Err("base URL must start with https:// or http://".to_string());
    }
    Ok(())
}

fn validate_preset_id(preset_id: &str) -> Result<(), String> {
    if preset_id == crate::provider::presets::CUSTOM_PRESET_ID
        || crate::provider::presets::find(preset_id).is_some()
    {
        return Ok(());
    }
    Err(format!("unknown preset: {preset_id}"))
}

fn custom_preset_info() -> ProviderPresetInfo {
    ProviderPresetInfo {
        id: crate::provider::presets::CUSTOM_PRESET_ID.to_string(),
        name: "Custom relay".to_string(),
        category: ProviderPresetCategory::Custom,
        protocol: ProviderProtocol::OpenaiCompat,
        base_url: String::new(),
    }
}

fn load_profile(profile_id: &str) -> Option<ProviderProfile> {
    crate::persist::provider_registry::load()
        .into_iter()
        .find(|profile| profile.id == profile_id)
}

fn provider_not_found(id: String, profile_id: &str) -> String {
    encode_error(
        id,
        "provider_not_found",
        format!("provider profile {profile_id} not found"),
    )
}

impl App {
    pub(super) fn handle_provider_list(
        &mut self,
        id: String,
        _params: ProviderListParams,
    ) -> String {
        let mut profiles: Vec<ProviderProfileInfo> = crate::persist::provider_registry::load()
            .iter()
            .map(masked_info)
            .collect();
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        encode_success(id, ResponseResult::ProviderList { profiles })
    }

    pub(super) fn handle_provider_get(&mut self, id: String, params: ProviderGetParams) -> String {
        let Some(profile) = load_profile(&params.profile_id) else {
            return provider_not_found(id, &params.profile_id);
        };
        encode_success(
            id,
            ResponseResult::ProviderGet {
                profile: masked_info(&profile),
            },
        )
    }

    pub(super) fn handle_provider_presets(
        &mut self,
        id: String,
        _params: ProviderPresetsParams,
    ) -> String {
        let mut presets = crate::provider::presets::preset_infos();
        presets.push(custom_preset_info());
        encode_success(id, ResponseResult::ProviderPresets { presets })
    }

    pub(super) fn handle_provider_create(
        &mut self,
        id: String,
        params: ProviderCreateParams,
    ) -> String {
        if params.name.trim().is_empty() {
            return encode_error(id, "invalid_params", "name is required");
        }
        if let Err(err) = validate_base_url(&params.base_url) {
            return encode_error(id, "invalid_params", err);
        }
        let preset_id = params
            .preset_id
            .unwrap_or_else(|| crate::provider::presets::CUSTOM_PRESET_ID.to_string());
        if let Err(err) = validate_preset_id(&preset_id) {
            return encode_error(id, "invalid_params", err);
        }

        let profile = ProviderProfile {
            id: crate::provider::generate_profile_id(),
            name: params.name.trim().to_string(),
            preset_id,
            protocol: params.protocol,
            base_url: params.base_url.trim().to_string(),
            api_key: params.api_key.unwrap_or_default(),
            models: params.models,
            weight: 1,
            is_disabled: false,
            note: params.note,
            created_unix: now_unix(),
        };
        let inserted = profile.clone();
        match crate::persist::provider_registry::update(|profiles| {
            profiles.push(profile);
        }) {
            Ok(_) => encode_success(
                id,
                ResponseResult::ProviderCreated {
                    profile: masked_info(&inserted),
                },
            ),
            Err(err) => encode_error(id, "provider_registry_error", err.to_string()),
        }
    }

    pub(super) fn handle_provider_update(
        &mut self,
        id: String,
        params: ProviderUpdateParams,
    ) -> String {
        if let Some(name) = params.name.as_deref() {
            if name.trim().is_empty() {
                return encode_error(id, "invalid_params", "name cannot be empty");
            }
        }
        if let Some(base_url) = params.base_url.as_deref() {
            if let Err(err) = validate_base_url(base_url) {
                return encode_error(id, "invalid_params", err);
            }
        }
        if params.api_key.as_deref() == Some("") {
            return encode_error(
                id,
                "invalid_params",
                "api key cannot be cleared; omit the field to keep the current key",
            );
        }

        let mut not_found = false;
        let result = crate::persist::provider_registry::update(|profiles| {
            let Some(profile) = profiles
                .iter_mut()
                .find(|profile| profile.id == params.profile_id)
            else {
                not_found = true;
                return;
            };
            if let Some(name) = params.name.as_deref() {
                profile.name = name.trim().to_string();
            }
            if let Some(protocol) = params.protocol {
                profile.protocol = protocol;
            }
            if let Some(base_url) = params.base_url.as_deref() {
                profile.base_url = base_url.trim().to_string();
            }
            if let Some(api_key) = params.api_key.as_deref() {
                profile.api_key = api_key.to_string();
            }
            if let Some(weight) = params.weight {
                profile.weight = weight;
            }
            if let Some(is_disabled) = params.is_disabled {
                profile.is_disabled = is_disabled;
            }
            if let Some(note) = params.note.as_deref() {
                profile.note = if note.is_empty() {
                    None
                } else {
                    Some(note.to_string())
                };
            }
        });
        if not_found {
            return provider_not_found(id, &params.profile_id);
        }
        match result {
            Ok((_, profiles)) => match profiles
                .iter()
                .find(|profile| profile.id == params.profile_id)
                .cloned()
            {
                Some(profile) => encode_success(
                    id,
                    ResponseResult::ProviderUpdated {
                        profile: masked_info(&profile),
                    },
                ),
                None => provider_not_found(id, &params.profile_id),
            },
            Err(err) => encode_error(id, "provider_registry_error", err.to_string()),
        }
    }

    pub(super) fn handle_provider_delete(
        &mut self,
        id: String,
        params: ProviderDeleteParams,
    ) -> String {
        let mut not_found = false;
        let result = crate::persist::provider_registry::update(|profiles| {
            let before = profiles.len();
            profiles.retain(|profile| profile.id != params.profile_id);
            if profiles.len() == before {
                not_found = true;
            }
        });
        match result {
            Ok(_) if not_found => provider_not_found(id, &params.profile_id),
            Ok(_) => encode_success(id, ResponseResult::ProviderDeleted {}),
            Err(err) => encode_error(id, "provider_registry_error", err.to_string()),
        }
    }

    pub(super) fn handle_provider_reveal(
        &mut self,
        id: String,
        params: ProviderRevealParams,
    ) -> String {
        let Some(profile) = load_profile(&params.profile_id) else {
            return provider_not_found(id, &params.profile_id);
        };
        encode_success(
            id,
            ResponseResult::ProviderReveal {
                api_key: profile.api_key,
            },
        )
    }

    /// Deferred-route entry for the two slow provider methods. Returns
    /// `true` when the request was consumed (the response arrives later via
    /// `AppEvent::ProviderHttpFinished`).
    pub(crate) fn handle_deferred_provider_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        match request.method {
            crate::api::schema::Method::ProviderTest(params) => {
                self.start_provider_http_request(
                    request.id,
                    params.profile_id,
                    ProviderHttpKind::Test,
                    respond_to,
                );
                true
            }
            crate::api::schema::Method::ProviderModelsFetch(params) => {
                self.start_provider_http_request(
                    request.id,
                    params.profile_id,
                    ProviderHttpKind::ModelsFetch,
                    respond_to,
                );
                true
            }
            _ => false,
        }
    }

    fn start_provider_http_request(
        &mut self,
        id: String,
        profile_id: String,
        kind: ProviderHttpKind,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        if self.provider_http_in_flight >= MAX_PROVIDER_HTTP_IN_FLIGHT {
            send_provider_response(
                &respond_to,
                encode_error(id, "provider_busy", "too many concurrent provider requests"),
            );
            return;
        }
        let Some(profile) = load_profile(&profile_id) else {
            send_provider_response(&respond_to, provider_not_found(id, &profile_id));
            return;
        };
        if self.pending_provider_requests.contains_key(&profile_id) {
            send_provider_response(
                &respond_to,
                encode_error(
                    id,
                    "provider_request_in_progress",
                    "a request is already in progress for this profile",
                ),
            );
            return;
        }

        let operation_id = self.next_provider_operation_id;
        self.next_provider_operation_id = self.next_provider_operation_id.saturating_add(1);
        self.pending_provider_requests
            .insert(profile_id.clone(), operation_id);
        self.provider_http_in_flight += 1;

        let event_tx = self.event_tx.clone();
        let api_request = ProviderApiRequest {
            id,
            operation_id,
            respond_to,
        };
        std::thread::spawn(move || {
            let result = match kind {
                ProviderHttpKind::Test => {
                    ProviderHttpResult::Test(crate::provider::http::test_connectivity(&profile))
                }
                ProviderHttpKind::ModelsFetch => {
                    match crate::provider::http::fetch_model_ids(&profile) {
                        Ok(model_ids) => {
                            let (models, truncated, warning) =
                                merge_models(&profile.models, model_ids);
                            ProviderHttpResult::Models {
                                models,
                                truncated,
                                warning,
                            }
                        }
                        Err(err) => ProviderHttpResult::Failed(err),
                    }
                }
            };
            let _ = event_tx.blocking_send(AppEvent::ProviderHttpFinished(Box::new(
                ProviderHttpOutcome {
                    profile_id,
                    result,
                    api_request: Some(api_request),
                },
            )));
        });
    }

    /// Event-handler side of the deferred requests: stale-check the
    /// operation, persist model fetches, answer the waiting caller, and
    /// surface a toast for the TUI.
    pub(crate) fn handle_provider_http_finished(&mut self, outcome: ProviderHttpOutcome) {
        self.provider_http_in_flight = self.provider_http_in_flight.saturating_sub(1);

        let operation_id = outcome
            .api_request
            .as_ref()
            .map(|request| request.operation_id);
        let stale = self
            .pending_provider_requests
            .get(&outcome.profile_id)
            .is_none_or(|pending| Some(*pending) != operation_id);
        if !stale {
            self.pending_provider_requests.remove(&outcome.profile_id);
        }

        // Persist a successful model fetch (unless a newer operation owns
        // the profile now — stale outcomes must not clobber fresh state).
        if !stale {
            if let ProviderHttpResult::Models { models, .. } = &outcome.result {
                let merged = models.clone();
                let profile_id = outcome.profile_id.clone();
                let _ = crate::persist::provider_registry::update(|profiles| {
                    if let Some(profile) =
                        profiles.iter_mut().find(|profile| profile.id == profile_id)
                    {
                        profile.models = merged;
                    }
                });
            }
        }

        // Answer the waiting API caller (CLI blocks on this channel).
        if !stale {
            if let Some(api_request) = outcome.api_request.as_ref() {
                let response = match &outcome.result {
                    ProviderHttpResult::Test(result) => serde_json::to_string(&SuccessResponse {
                        id: api_request.id.clone(),
                        result: ResponseResult::ProviderTest {
                            result: result.clone(),
                        },
                    })
                    .unwrap_or_else(|_| {
                        encode_error(
                            api_request.id.clone(),
                            "provider_response_error",
                            "failed to encode test result",
                        )
                    }),
                    ProviderHttpResult::Models {
                        models,
                        truncated,
                        warning,
                    } => serde_json::to_string(&SuccessResponse {
                        id: api_request.id.clone(),
                        result: ResponseResult::ProviderModelsFetched {
                            result: ProviderModelsFetchResult {
                                models: models.clone(),
                                truncated: *truncated,
                                warning: warning.clone(),
                            },
                        },
                    })
                    .unwrap_or_else(|_| {
                        encode_error(
                            api_request.id.clone(),
                            "provider_response_error",
                            "failed to encode models result",
                        )
                    }),
                    ProviderHttpResult::Failed(err) => {
                        encode_error(api_request.id.clone(), "provider_request_failed", err)
                    }
                };
                send_provider_response(&api_request.respond_to, response);
            }
        }

        // TUI toast (headless forwards toasts to the foreground client).
        let (ok, title, context) = match &outcome.result {
            ProviderHttpResult::Test(result) => {
                let context = if result.ok {
                    format!(
                        "ok · {} ms{}",
                        result.latency_ms,
                        result
                            .model
                            .as_deref()
                            .map(|model| format!(" · {model}"))
                            .unwrap_or_default()
                    )
                } else {
                    format!(
                        "failed{}",
                        result
                            .error
                            .as_deref()
                            .map(|error| format!(": {error}"))
                            .unwrap_or_default()
                    )
                };
                (
                    result.ok,
                    format!("Provider test: {}", outcome.profile_id),
                    context,
                )
            }
            ProviderHttpResult::Models {
                models,
                truncated,
                warning,
            } => {
                let mut context = format!("{} models", models.len());
                if *truncated {
                    context.push_str(" (truncated)");
                }
                if let Some(warning) = warning {
                    context.push_str(" · ");
                    context.push_str(warning);
                }
                (
                    true,
                    format!("Models fetched: {}", outcome.profile_id),
                    context,
                )
            }
            ProviderHttpResult::Failed(err) => (
                false,
                format!("Provider request failed: {}", outcome.profile_id),
                err.clone(),
            ),
        };
        self.state.toast = Some(crate::app::state::ToastNotification {
            kind: if ok {
                crate::app::state::ToastKind::Finished
            } else {
                crate::app::state::ToastKind::NeedsAttention
            },
            title,
            context,
            position: None,
            target: None,
        });
    }
}

// -- settings-tab integration ------------------------------------------------

impl App {
    /// Reload the masked profile list backing the Providers settings tab.
    /// Called when the section opens and after every provider action.
    pub(crate) fn refresh_provider_section(&mut self) {
        let mut profiles: Vec<ProviderProfileInfo> = crate::persist::provider_registry::load()
            .iter()
            .map(masked_info)
            .collect();
        profiles.sort_by(|left, right| left.name.cmp(&right.name));

        let section = self
            .state
            .settings
            .providers
            .get_or_insert_with(Default::default);
        section.profiles = profiles;
        if self.state.settings.list.selected >= section.profiles.len() {
            self.state.settings.list.selected = section.profiles.len().saturating_sub(1);
        }
    }

    pub(crate) fn provider_create_via_settings(
        &mut self,
        name: &str,
        base_url: &str,
        api_key: &str,
    ) {
        let response = self.dispatch_api_request(
            "settings:provider:create",
            crate::api::schema::Method::ProviderCreate(ProviderCreateParams {
                name: name.to_string(),
                preset_id: Some(crate::provider::presets::CUSTOM_PRESET_ID.to_string()),
                protocol: crate::api::schema::ProviderProtocol::OpenaiCompat,
                base_url: base_url.to_string(),
                api_key: if api_key.is_empty() {
                    None
                } else {
                    Some(api_key.to_string())
                },
                models: Vec::new(),
                note: None,
            }),
        );
        if response.contains("\"error\"") {
            tracing::warn!(
                "provider create from settings failed: {}",
                crate::provider::url::redact(api_key, &response)
            );
        }
        self.refresh_provider_section();
    }

    pub(crate) fn provider_update_via_settings(
        &mut self,
        params: crate::api::schema::ProviderUpdateParams,
    ) {
        let response = self.dispatch_api_request(
            "settings:provider:update",
            crate::api::schema::Method::ProviderUpdate(params),
        );
        if response.contains("\"error\"") {
            tracing::warn!("provider update from settings failed: {response}");
        }
        self.refresh_provider_section();
    }

    pub(crate) fn provider_delete_via_settings(&mut self, profile_id: &str) {
        let response = self.dispatch_api_request(
            "settings:provider:delete",
            crate::api::schema::Method::ProviderDelete(ProviderDeleteParams {
                profile_id: profile_id.to_string(),
            }),
        );
        if response.contains("\"error\"") {
            tracing::warn!("provider delete from settings failed: {response}");
        }
        self.refresh_provider_section();
    }

    /// Fire a connectivity test from the settings tab. The result arrives
    /// later as a toast via `AppEvent::ProviderHttpFinished`.
    pub(crate) fn provider_test_via_settings(&mut self, profile_id: &str) {
        let (respond_to, _response_rx) = std::sync::mpsc::channel();
        let consumed = self.handle_deferred_provider_api_request(
            crate::api::schema::Request {
                id: "settings:provider:test".to_string(),
                method: crate::api::schema::Method::ProviderTest(ProviderTestParams {
                    profile_id: profile_id.to_string(),
                }),
            },
            respond_to,
        );
        if consumed {
            if let Some(section) = self.state.settings.providers.as_mut() {
                section.testing.insert(profile_id.to_string());
            }
        }
    }
}
