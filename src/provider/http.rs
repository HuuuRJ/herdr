//! Outbound provider HTTP over the `curl` subprocess.
//!
//! herdr deliberately ships no HTTP client crate; every outbound request goes
//! through curl (the same channel `update.rs` and `manifest_update.rs` use).
//! Two provider-specific rules shape this module:
//!
//! 1. **API keys never appear in argv.** Headers are fed via `curl -H @-`
//!    (one `Header: value` line each) from stdin, so a local process listing
//!    cannot observe keys. Bodies are secret-free JSON and stay on argv.
//! 2. **Status codes must survive.** We do NOT use `-f` (it would collapse
//!    401 vs 404 into a silent exit 22); instead `-w '%{http_code}'` appends
//!    the status to stdout for explicit classification.
//!
//! Gemini auth uses the `x-goog-api-key` header — never the `?key=` query
//! parameter, which would land the key in argv and error messages.

use std::io::Read;
use std::process::Stdio;
use std::time::Instant;

use crate::api::schema::{
    ProviderModelEntry, ProviderModelSource, ProviderProfile, ProviderProtocol, ProviderTestResult,
};

use super::url::join_url;

/// Hard ceiling for a models-list response body (4 MiB). Larger bodies are
/// truncated and flagged; the 500-model merge cap does the real limiting.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Models beyond this count are dropped with a warning (FR-3.4).
const MAX_MODELS: usize = 500;

pub(crate) const ANTHROPIC_VERSION_HEADER: &str = "2023-06-01";
/// Anthropic-protocol bases on these hosts have no `/v1/models` endpoint;
/// the model list is fetched from the same host's OpenAI-compatible side.
const ANTHROPIC_MODELS_FALLBACK_HOSTS: &[&str] =
    &["api.deepseek.com", "api.moonshot.cn", "api.moonshot.ai"];

pub(crate) struct ProviderHttpResponse {
    /// HTTP status when the server answered; `None` on transport failure.
    pub(crate) status: Option<u16>,
    pub(crate) body: String,
    /// Transport-level error (curl stderr / spawn failure), un-redacted —
    /// callers must pass it through `redact` before surfacing.
    pub(crate) transport_error: Option<String>,
}

fn run_curl(
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    max_time_secs: u64,
) -> ProviderHttpResponse {
    let mut command = crate::noninteractive_process::curl_command();
    command
        .arg("-sS")
        .arg("--connect-timeout")
        .arg("5")
        .arg("--max-time")
        .arg(max_time_secs.to_string())
        .arg("-w")
        .arg("\n%{http_code}")
        // Headers (including secrets) come from stdin, never argv.
        .arg("-H")
        .arg("@-")
        .arg(url);
    if let Some(body) = body {
        command.arg("--data").arg(body);
    }

    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return ProviderHttpResponse {
                status: None,
                body: String::new(),
                transport_error: Some(format!("failed to start curl: {err}")),
            };
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let mut header_bytes = String::new();
        for (name, value) in headers {
            header_bytes.push_str(name);
            header_bytes.push_str(": ");
            header_bytes.push_str(value);
            header_bytes.push('\n');
        }
        if let Err(err) = stdin.write_all(header_bytes.as_bytes()) {
            tracing::warn!(err = %err, "failed to write curl headers");
        }
        // Dropping stdin signals EOF so curl starts the transfer immediately.
    }

    let mut stdout = Vec::new();
    let mut stderr = String::new();
    let mut has_more = true;
    if let Some(mut pipe) = child.stdout.take() {
        let mut limited = (&mut pipe).take((MAX_RESPONSE_BYTES + 1) as u64);
        // Read to end; the size cap above bounds memory.
        while has_more {
            match limited.read_to_end(&mut stdout) {
                Ok(0) | Err(_) => {
                    has_more = false;
                }
                Ok(_) if stdout.len() > MAX_RESPONSE_BYTES => {
                    has_more = false;
                }
                Ok(_) => {}
            }
        }
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let _ = child.wait();

    let truncated = stdout.len() > MAX_RESPONSE_BYTES;
    let mut body = String::from_utf8_lossy(&stdout).to_string();
    if truncated {
        body.truncate(MAX_RESPONSE_BYTES);
    }

    // curl appends "\n{status}" on a completed HTTP exchange; missing tail
    // means the transfer itself failed.
    if let Some((payload, status_line)) = body.rsplit_once('\n') {
        if let Ok(status) = status_line.trim().parse::<u16>() {
            return ProviderHttpResponse {
                status: Some(status),
                body: payload.to_string(),
                transport_error: None,
            };
        }
    }
    ProviderHttpResponse {
        status: None,
        body,
        transport_error: Some(if stderr.trim().is_empty() {
            "curl transfer failed".to_string()
        } else {
            stderr.trim().to_string()
        }),
    }
}

/// JSON curl request for sibling subsystems (workflow image generation).
/// Same conventions as `run_curl`; returns the parsed status with the body.
pub(crate) fn provider_curl_json(
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    max_time_secs: u64,
) -> ProviderHttpResponse {
    run_curl(url, headers, body, max_time_secs)
}

/// Binary curl download for sibling subsystems (workflow image artifacts).
/// The payload stays raw bytes; the `-w` status tail is split off the end.
pub(crate) struct CurlBinaryResponse {
    pub status: Option<u16>,
    pub bytes: Vec<u8>,
    pub transport_error: Option<String>,
}

pub(crate) fn provider_curl_binary(
    url: &str,
    headers: &[(String, String)],
    max_time_secs: u64,
) -> CurlBinaryResponse {
    let mut command = crate::noninteractive_process::curl_command();
    command
        .arg("-sS")
        .arg("--connect-timeout")
        .arg("5")
        .arg("--max-time")
        .arg(max_time_secs.to_string())
        .arg("-w")
        .arg("\n%{http_code}")
        .arg("-H")
        .arg("@-")
        .arg(url);

    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return CurlBinaryResponse {
                status: None,
                bytes: Vec::new(),
                transport_error: Some(format!("failed to start curl: {err}")),
            };
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let mut header_bytes = String::new();
        for (name, value) in headers {
            header_bytes.push_str(name);
            header_bytes.push_str(": ");
            header_bytes.push_str(value);
            header_bytes.push('\n');
        }
        let _ = stdin.write_all(header_bytes.as_bytes());
    }
    let mut stdout = Vec::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = (&mut pipe).take(64 * 1024 * 1024).read_to_end(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let _ = child.wait();

    // The status tail is "\n<code>" at the very end (≤ 4 bytes of digits).
    if let Some(position) = stdout.iter().rposition(|byte| *byte == b'\n') {
        let tail = String::from_utf8_lossy(&stdout[position + 1..])
            .trim()
            .to_string();
        if tail.len() <= 4 {
            if let Ok(status) = tail.parse::<u16>() {
                stdout.truncate(position);
                return CurlBinaryResponse {
                    status: Some(status),
                    bytes: stdout,
                    transport_error: None,
                };
            }
        }
    }
    CurlBinaryResponse {
        status: None,
        bytes: stdout,
        transport_error: Some(if stderr.trim().is_empty() {
            "curl transfer failed".to_string()
        } else {
            stderr.trim().to_string()
        }),
    }
}

fn auth_headers(protocol: ProviderProtocol, api_key: &str) -> Vec<(String, String)> {
    match protocol {
        ProviderProtocol::OpenaiCompat => {
            vec![
                ("Authorization".to_string(), format!("Bearer {api_key}")),
                ("Accept".to_string(), "application/json".to_string()),
            ]
        }
        ProviderProtocol::Anthropic => {
            // Dual auth headers maximize relay compatibility (FR-3.1).
            vec![
                ("x-api-key".to_string(), api_key.to_string()),
                ("Authorization".to_string(), format!("Bearer {api_key}")),
                (
                    "anthropic-version".to_string(),
                    ANTHROPIC_VERSION_HEADER.to_string(),
                ),
                ("Accept".to_string(), "application/json".to_string()),
            ]
        }
        ProviderProtocol::Gemini => {
            vec![
                ("x-goog-api-key".to_string(), api_key.to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ]
        }
    }
}

fn content_type_json(headers: &mut Vec<(String, String)>) {
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
}

// -- connectivity test -------------------------------------------------------

fn chat_endpoint(protocol: ProviderProtocol, base_url: &str, model: &str) -> (String, String) {
    match protocol {
        ProviderProtocol::OpenaiCompat => (
            join_url(base_url, "/chat/completions"),
            serde_json::json!({
                "model": model,
                "max_tokens": 1,
                "messages": [{ "role": "user", "content": "ping" }],
            })
            .to_string(),
        ),
        ProviderProtocol::Anthropic => (
            join_url(base_url, "/v1/messages"),
            serde_json::json!({
                "model": model,
                "max_tokens": 1,
                "messages": [{ "role": "user", "content": "ping" }],
            })
            .to_string(),
        ),
        ProviderProtocol::Gemini => (
            join_url(base_url, &format!("/v1beta/models/{model}:generateContent")),
            serde_json::json!({
                "contents": [{ "parts": [{ "text": "ping" }] }],
                "generationConfig": { "maxOutputTokens": 1 },
            })
            .to_string(),
        ),
    }
}

fn classify_http_status(status: u16) -> Option<&'static str> {
    match status {
        200..=299 => None,
        401 | 403 => Some("authentication failed (check API key)"),
        404 => Some("endpoint not found (check base URL)"),
        429 => Some("rate limited"),
        _ => Some("provider returned an error"),
    }
}

/// Run a one-shot connectivity test. `profile` is a snapshot taken before the
/// request; the result is display-only and never writes health state.
pub(crate) fn test_connectivity(profile: &ProviderProfile) -> ProviderTestResult {
    let started = Instant::now();
    let test_model = profile
        .models
        .iter()
        .find(|model| model.visible)
        .map(|model| model.id.clone());

    let Some(model) = test_model else {
        // No visible model: fall back to a models-endpoint probe.
        let (url, _, _) = models_endpoint(profile);
        let response = run_curl(
            &url,
            &auth_headers(profile.protocol, &profile.api_key),
            None,
            10,
        );
        return finish_test(profile, response, None, started);
    };

    let (url, body) = chat_endpoint(profile.protocol, &profile.base_url, &model);
    let mut headers = auth_headers(profile.protocol, &profile.api_key);
    content_type_json(&mut headers);
    let response = run_curl(&url, &headers, Some(&body), 10);
    finish_test(profile, response, Some(model), started)
}

fn finish_test(
    profile: &ProviderProfile,
    response: ProviderHttpResponse,
    model: Option<String>,
    started: Instant,
) -> ProviderTestResult {
    let latency_ms = started.elapsed().as_millis() as u64;
    let error = match response.status {
        Some(status) => classify_http_status(status).map(|message| {
            let detail = super::url::redact(&profile.api_key, &response.body);
            let detail = detail.trim();
            if detail.is_empty() {
                format!("HTTP {status}: {message}")
            } else {
                format!("HTTP {status}: {message}: {detail}")
            }
        }),
        None => Some(super::url::redact(
            &profile.api_key,
            &response
                .transport_error
                .unwrap_or_else(|| "request failed".to_string()),
        )),
    };
    ProviderTestResult {
        ok: error.is_none(),
        http_status: response.status,
        latency_ms,
        error: error.map(|message| truncate_error(&message)),
        model,
    }
}

fn truncate_error(message: &str) -> String {
    const MAX_ERROR_CHARS: usize = 400;
    if message.chars().count() <= MAX_ERROR_CHARS {
        return message.to_string();
    }
    let truncated: String = message.chars().take(MAX_ERROR_CHARS).collect();
    format!("{truncated}…")
}

// -- models fetch ------------------------------------------------------------

/// Resolve the models endpoint for a profile.
///
/// Returns `(url, effective_protocol, used_fallback)`. Anthropic-protocol
/// bases on known no-models-endpoint hosts cross over to the same host's
/// OpenAI-compatible endpoint (FR-3.2).
pub(crate) fn models_endpoint(profile: &ProviderProfile) -> (String, ProviderProtocol, bool) {
    match profile.protocol {
        ProviderProtocol::OpenaiCompat => (
            join_url(&profile.base_url, "/models"),
            profile.protocol,
            false,
        ),
        ProviderProtocol::Anthropic => {
            if let Some(openai_base) = anthropic_fallback_openai_base(&profile.base_url) {
                return (
                    join_url(&openai_base, "/v1/models"),
                    ProviderProtocol::OpenaiCompat,
                    true,
                );
            }
            (
                join_url(&profile.base_url, "/v1/models"),
                profile.protocol,
                false,
            )
        }
        ProviderProtocol::Gemini => (
            join_url(&profile.base_url, "/v1beta/models"),
            profile.protocol,
            false,
        ),
    }
}

/// `https://api.deepseek.com/anthropic` → `https://api.deepseek.com`.
/// Only hosts in the fallback table are rewritten; anything else keeps the
/// anthropic endpoint.
fn anthropic_fallback_openai_base(base_url: &str) -> Option<String> {
    let (_, host_port) = base_url.split_once("://")?;
    let host = host_port.split('/').next()?;
    if !ANTHROPIC_MODELS_FALLBACK_HOSTS.contains(&host) {
        return None;
    }
    if !base_url.ends_with("/anthropic") {
        return None;
    }
    Some(base_url.trim_end_matches("/anthropic").to_string())
}

/// Parse a models response body into model ids.
///
/// Business-error guard (FR-3.3): some providers answer 200 with an error
/// body (`code != 0` or `success: false`, e.g. zhipu's `code: 1001`) — that
/// must surface as an authentication-style failure, never as "0 models".
pub(crate) fn parse_models_body(
    protocol: ProviderProtocol,
    body: &str,
) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("invalid JSON response: {err}"))?;

    let entries = value
        .get("data")
        .and_then(|data| data.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
                .map(|id| id.to_string())
                .collect::<Vec<String>>()
        })
        .or_else(|| {
            value
                .get("models")
                .and_then(|models| models.as_array())
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|item| {
                            item.get("name")
                                .or_else(|| item.get("model"))
                                .and_then(|name| name.as_str())
                        })
                        .map(|name| name.trim_start_matches("models/").to_string())
                        .collect::<Vec<String>>()
                })
        });

    match entries {
        Some(models) if !models.is_empty() => {
            let _ = protocol;
            Ok(models)
        }
        Some(_) => Ok(Vec::new()),
        None => {
            let code_nonzero = value
                .get("code")
                .and_then(|code| code.as_i64())
                .is_some_and(|code| code != 0);
            let success_false =
                value.get("success").and_then(|success| success.as_bool()) == Some(false);
            if code_nonzero || success_false {
                Err("provider returned a business error (check API key)".to_string())
            } else {
                Err("unexpected models response shape".to_string())
            }
        }
    }
}

/// Fetch the model list for a profile. Errors are redacted against the key.
pub(crate) fn fetch_model_ids(profile: &ProviderProfile) -> Result<Vec<String>, String> {
    let (url, effective_protocol, _) = models_endpoint(profile);
    let headers = auth_headers(effective_protocol, &profile.api_key);
    let response = run_curl(&url, &headers, None, 15);
    let redact_key = super::url::redact(&profile.api_key, &response.body);
    match response.status {
        Some(status) => match classify_http_status(status) {
            Some(message) => Err(format!(
                "HTTP {status}: {message}: {}",
                truncate_error(redact_key.trim())
            )),
            None => parse_models_body(effective_protocol, &redact_key),
        },
        None => Err(super::url::redact(
            &profile.api_key,
            &response
                .transport_error
                .unwrap_or_else(|| "request failed".to_string()),
        )),
    }
}

/// Merge a freshly fetched model list into the profile's current list.
///
/// Rules (FR-3.5): manual entries survive unconditionally; fetched entries
/// are replaced wholesale; an id that still exists inherits its old
/// `visible`; brand-new ids default to visible.
pub(crate) fn merge_models(
    existing: &[ProviderModelEntry],
    fetched_ids: Vec<String>,
) -> (Vec<ProviderModelEntry>, bool, Option<String>) {
    let mut truncated = false;
    let mut warning = None;
    let mut fetched_ids = fetched_ids;
    if fetched_ids.len() > MAX_MODELS {
        fetched_ids.truncate(MAX_MODELS);
        truncated = true;
        warning = Some(format!(
            "provider listed more than {MAX_MODELS} models; list truncated"
        ));
    }

    let mut merged: Vec<ProviderModelEntry> = existing
        .iter()
        .filter(|model| model.source == ProviderModelSource::Manual)
        .cloned()
        .collect();
    let manual_ids: Vec<String> = merged.iter().map(|model| model.id.clone()).collect();

    for id in fetched_ids {
        if manual_ids.contains(&id) {
            continue;
        }
        let visible = existing
            .iter()
            .find(|model| model.id == id)
            .map(|model| model.visible)
            .unwrap_or(true);
        merged.push(ProviderModelEntry {
            id,
            visible,
            source: ProviderModelSource::Fetched,
        });
    }

    merged.sort_by(|left, right| left.id.cmp(&right.id));
    (merged, truncated, warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(protocol: ProviderProtocol, base_url: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p1".to_string(),
            name: "test".to_string(),
            preset_id: "custom".to_string(),
            protocol,
            base_url: base_url.to_string(),
            api_key: "sk-test-1234567890".to_string(),
            models: vec![],
            weight: 1,
            is_disabled: false,
            note: None,
            created_unix: 1,
        }
    }

    #[test]
    fn anthropic_fallback_hits_only_known_hosts() {
        assert_eq!(
            anthropic_fallback_openai_base("https://api.deepseek.com/anthropic"),
            Some("https://api.deepseek.com".to_string())
        );
        assert_eq!(
            anthropic_fallback_openai_base("https://api.moonshot.cn/anthropic"),
            Some("https://api.moonshot.cn".to_string())
        );
        // Unknown host keeps the anthropic endpoint.
        assert_eq!(
            anthropic_fallback_openai_base("https://relay.example.com/anthropic"),
            None
        );
        // Known host but not an /anthropic base is not rewritten.
        assert_eq!(
            anthropic_fallback_openai_base("https://api.deepseek.com/v1"),
            None
        );
    }

    #[test]
    fn models_endpoints_per_protocol() {
        let (_, protocol, fallback) = models_endpoint(&profile(
            ProviderProtocol::OpenaiCompat,
            "https://api.example.com/v1",
        ));
        assert_eq!(protocol, ProviderProtocol::OpenaiCompat);
        assert!(!fallback);

        let (url, protocol, fallback) = models_endpoint(&profile(
            ProviderProtocol::Anthropic,
            "https://api.example.com",
        ));
        assert!(url.ends_with("/v1/models"));
        assert_eq!(protocol, ProviderProtocol::Anthropic);
        assert!(!fallback);

        let (url, protocol, fallback) = models_endpoint(&profile(
            ProviderProtocol::Anthropic,
            "https://api.deepseek.com/anthropic",
        ));
        assert_eq!(url, "https://api.deepseek.com/v1/models");
        assert_eq!(protocol, ProviderProtocol::OpenaiCompat);
        assert!(fallback);

        let (url, _, _) = models_endpoint(&profile(
            ProviderProtocol::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
        ));
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
    }

    #[test]
    fn parse_openai_and_gemini_shapes() {
        let openai = r#"{"data": [{"id": "gpt-4o"}, {"id": "gpt-4o-mini"}]}"#;
        assert_eq!(
            parse_models_body(ProviderProtocol::OpenaiCompat, openai).unwrap(),
            vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]
        );
        let gemini =
            r#"{"models": [{"name": "models/gemini-2.0-flash"}, {"name": "models/gemini-pro"}]}"#;
        assert_eq!(
            parse_models_body(ProviderProtocol::Gemini, gemini).unwrap(),
            vec!["gemini-2.0-flash".to_string(), "gemini-pro".to_string()]
        );
    }

    #[test]
    fn business_error_body_is_auth_failure_not_empty_list() {
        // zhipu answers 200 + {"code":1001,...} without credentials.
        let body = r#"{"code": 1001, "message": "invalid api key"}"#;
        let err = parse_models_body(ProviderProtocol::OpenaiCompat, body).unwrap_err();
        assert!(err.contains("business error"), "{err}");
        let body = r#"{"success": false, "error": "denied"}"#;
        assert!(parse_models_body(ProviderProtocol::OpenaiCompat, body).is_err());
    }

    #[test]
    fn empty_data_array_is_not_an_error() {
        let body = r#"{"data": []}"#;
        assert!(parse_models_body(ProviderProtocol::OpenaiCompat, body)
            .unwrap()
            .is_empty());
    }

    fn entry(id: &str, visible: bool, source: ProviderModelSource) -> ProviderModelEntry {
        ProviderModelEntry {
            id: id.to_string(),
            visible,
            source,
        }
    }

    #[test]
    fn merge_keeps_manual_replaces_fetched_inherits_visible() {
        let existing = vec![
            entry("manual-a", true, ProviderModelSource::Manual),
            entry("old-fetched", false, ProviderModelSource::Fetched),
            entry("shared", false, ProviderModelSource::Fetched),
        ];
        let (merged, truncated, warning) = merge_models(
            &existing,
            vec![
                "shared".to_string(),
                "new-model".to_string(),
                "manual-a".to_string(),
            ],
        );
        assert!(!truncated);
        assert!(warning.is_none());

        let manual = merged.iter().find(|m| m.id == "manual-a").unwrap();
        assert_eq!(manual.source, ProviderModelSource::Manual);
        assert!(manual.visible);
        // old-fetched was replaced wholesale.
        assert!(!merged.iter().any(|m| m.id == "old-fetched"));
        // shared inherited visible=false; new-model defaults to visible.
        let shared = merged.iter().find(|m| m.id == "shared").unwrap();
        assert!(!shared.visible);
        assert_eq!(shared.source, ProviderModelSource::Fetched);
        let new_model = merged.iter().find(|m| m.id == "new-model").unwrap();
        assert!(new_model.visible);
    }

    #[test]
    fn merge_truncates_beyond_cap() {
        let ids: Vec<String> = (0..600).map(|n| format!("model-{n}")).collect();
        let (merged, truncated, warning) = merge_models(&[], ids);
        assert!(truncated);
        assert_eq!(merged.len(), MAX_MODELS);
        assert!(warning.is_some());
    }

    #[test]
    fn chat_endpoints_join_correctly() {
        let (url, body) = chat_endpoint(
            ProviderProtocol::Anthropic,
            "https://api.example.com",
            "claude-x",
        );
        assert_eq!(url, "https://api.example.com/v1/messages");
        assert!(body.contains("\"max_tokens\":1"));

        let (url, _) = chat_endpoint(
            ProviderProtocol::Gemini,
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-x",
        );
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-x:generateContent"
        );
    }
}

// -- deferred outcome types --------------------------------------------------

/// Which background operation a spawned provider request performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderHttpKind {
    Test,
    ModelsFetch,
}

/// What the background thread delivers back to the app event loop.
#[derive(Debug)]
pub(crate) enum ProviderHttpResult {
    Test(ProviderTestResult),
    Models {
        models: Vec<ProviderModelEntry>,
        truncated: bool,
        warning: Option<String>,
    },
    Failed(String),
}
