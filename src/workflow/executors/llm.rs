//! `llm_chat` node: direct one-shot LLM call over curl, no agent CLI.
//!
//! Non-streaming by decision (P3d): the full response text becomes the node
//! output. The provider profile decides the wire protocol — openai-compat
//! `/chat/completions`, anthropic `/v1/messages`, relays included — reusing
//! the curl conventions from `provider::http` (stdin headers, status via
//! `-w`). Gemini is not a chat wire (P3d scope); `wire_for` filters it out
//! before any request is built.

use crate::api::schema::{ProviderProfile, ProviderProtocol};
use crate::workflow::runs::NodeMeta;

/// The chat wire protocols `llm_chat` speaks, derived from a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatWire {
    OpenaiCompat,
    Anthropic,
}

pub(crate) fn wire_for(protocol: ProviderProtocol) -> Option<ChatWire> {
    match protocol {
        ProviderProtocol::OpenaiCompat => Some(ChatWire::OpenaiCompat),
        ProviderProtocol::Anthropic => Some(ChatWire::Anthropic),
        ProviderProtocol::Gemini => None,
    }
}

/// anthropic rejects requests without `max_tokens`; when the node leaves it
/// unset we substitute this ceiling instead of omitting the key (FR-5.6
/// omit-when-unset still holds for openai-compat).
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 4096;

/// Fallback curl ceiling when the node sets no timeout (0 = no timeout,
/// W6): a hung curl would otherwise wedge the run's drain forever.
const LLM_CURL_MAX_TIME_SECS: u64 = 600;

/// Map the node's `timeout_ms` (0 = unset) onto curl's finite `--max-time`.
pub(crate) fn max_time_secs(timeout_ms: u64) -> u64 {
    match timeout_ms {
        0 => LLM_CURL_MAX_TIME_SECS,
        ms => ms.div_ceil(1000).max(1),
    }
}

/// Build the chat request `(url, headers, body)` for a profile.
pub(crate) fn chat_request(
    profile: &ProviderProfile,
    wire: ChatWire,
    system: Option<&str>,
    prompt: &str,
    model: &str,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
) -> (String, Vec<(String, String)>, String) {
    let mut headers =
        crate::provider::http::auth_headers(profile.protocol, &profile.api_key);
    headers.push(("Content-Type".to_string(), "application/json".to_string()));
    match wire {
        ChatWire::OpenaiCompat => {
            let url = crate::provider::url::join_url(&profile.base_url, "/chat/completions");
            let mut messages = Vec::new();
            if let Some(system) = system {
                messages.push(serde_json::json!({ "role": "system", "content": system }));
            }
            messages.push(serde_json::json!({ "role": "user", "content": prompt }));
            let mut body = serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
            });
            // Unset params are omitted entirely (FR-5.6).
            if let Some(max_tokens) = max_tokens {
                body["max_tokens"] = max_tokens.into();
            }
            if let Some(temperature) = temperature {
                body["temperature"] = temperature.into();
            }
            (url, headers, body.to_string())
        }
        ChatWire::Anthropic => {
            let url = crate::provider::url::join_url(&profile.base_url, "/v1/messages");
            let mut body = serde_json::json!({
                "model": model,
                "max_tokens": max_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS),
                "messages": [{ "role": "user", "content": prompt }],
            });
            if let Some(system) = system {
                body["system"] = serde_json::Value::String(system.to_string());
            }
            if let Some(temperature) = temperature {
                body["temperature"] = temperature.into();
            }
            (url, headers, body.to_string())
        }
    }
}

/// Parse a 2xx chat body into `(text, total_tokens)`. Token usage is optional
/// — some relays omit it.
pub(crate) fn parse_chat_response(
    wire: ChatWire,
    body: &str,
) -> Result<(String, Option<u64>), String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("invalid JSON response: {err}"))?;
    // Error bodies normally arrive with non-2xx; a 2xx business error also
    // lands here (relay shapes measured in FR-3.3).
    if let Some(error) = value.get("error") {
        let detail = error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or("provider returned an error");
        return Err(format!("provider error: {detail}"));
    }
    match wire {
        ChatWire::OpenaiCompat => {
            let content = value
                .get("choices")
                .and_then(|choices| choices.as_array())
                .and_then(|choices| choices.first())
                .ok_or_else(|| "response has no choices".to_string())?
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_str())
                .ok_or_else(|| "response choice has no message content".to_string())?;
            let usage = value.get("usage");
            let tokens = sum_usage(
                usage
                    .and_then(|usage| usage.get("prompt_tokens"))
                    .and_then(|tokens| tokens.as_u64()),
                usage
                    .and_then(|usage| usage.get("completion_tokens"))
                    .and_then(|tokens| tokens.as_u64()),
            );
            Ok((content.to_string(), tokens))
        }
        ChatWire::Anthropic => {
            let blocks = value
                .get("content")
                .and_then(|content| content.as_array())
                .ok_or_else(|| "response has no content blocks".to_string())?;
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(|kind| kind.as_str()) == Some("text") {
                    if let Some(chunk) = block.get("text").and_then(|text| text.as_str()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(chunk);
                    }
                }
            }
            if text.is_empty() {
                return Err("response has no text content".to_string());
            }
            let usage = value.get("usage");
            let tokens = sum_usage(
                usage
                    .and_then(|usage| usage.get("input_tokens"))
                    .and_then(|tokens| tokens.as_u64()),
                usage
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(|tokens| tokens.as_u64()),
            );
            Ok((text, tokens))
        }
    }
}

fn sum_usage(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a + b),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// A failed chat call: the surfaced message plus the HTTP status when one
/// was received. Transport failures and 2xx parse failures carry `None` —
/// the request already went through, so the pool must NOT fail over on
/// them (a retry would double-bill).
pub(crate) struct LlmChatError {
    pub message: String,
    pub status: Option<u16>,
}

/// Run one chat completion. Returns the response text plus a `NodeMeta`
/// carrying the model and token usage.
pub(crate) fn run_llm_chat(
    profile: &ProviderProfile,
    wire: ChatWire,
    system: Option<&str>,
    prompt: &str,
    model: &str,
    max_tokens: Option<u32>,
    temperature: Option<f64>,
    max_time_secs: u64,
) -> Result<(String, NodeMeta), LlmChatError> {
    let (url, headers, body) = chat_request(
        profile, wire, system, prompt, model, max_tokens, temperature,
    );
    // The JSON body rides a temp file instead of argv: llm prompts embed
    // upstream outputs and routinely exceed the Windows 32K command-line
    // cap (os error 206). "@path" makes curl stream the body from disk.
    static BODY_FILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = BODY_FILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let body_file = std::env::temp_dir().join(format!(
        "herdr-llm-body-{}-{seq}.json",
        std::process::id()
    ));
    if let Err(err) = std::fs::write(&body_file, body.as_bytes()) {
        return Err(LlmChatError {
            message: format!("failed to stage request body: {err}"),
            status: None,
        });
    }
    let data = format!("@{}", body_file.display());
    let response = crate::provider::http::provider_curl_json(
        &url,
        &headers,
        Some(&data),
        &[],
        max_time_secs,
    );
    let _ = std::fs::remove_file(&body_file);
    let redacted = crate::provider::url::redact(&profile.api_key, &response.body);
    let Some(status) = response.status else {
        return Err(LlmChatError {
            message: crate::provider::url::redact(
                &profile.api_key,
                &response
                    .transport_error
                    .unwrap_or_else(|| "request failed".to_string()),
            ),
            status: None,
        });
    };
    if !(200..300).contains(&status) {
        let detail = redacted.trim();
        Err(LlmChatError {
            // "HTTP <status>:" prefix feeds the pool outcome labels.
            message: format!(
                "HTTP {status}: {}",
                if detail.is_empty() {
                    "llm request failed"
                } else {
                    detail
                }
            ),
            status: Some(status),
        })
    } else {
        let (text, tokens) = parse_chat_response(wire, &redacted).map_err(|message| LlmChatError {
            message,
            status: None,
        })?;
        let meta = NodeMeta {
            model: Some(model.to_string()),
            tokens,
            ..Default::default()
        };
        Ok((text, meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(protocol: ProviderProtocol, base_url: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p1".to_string(),
            name: "chat".to_string(),
            preset_id: "custom".to_string(),
            protocol,
            base_url: base_url.to_string(),
            api_key: "sk-chat-1234567890".to_string(),
            models: vec![],
            weight: 1,
            is_disabled: false,
            note: None,
            created_unix: 0,
        }
    }

    #[test]
    fn openai_request_joins_url_and_omits_unset_params() {
        let (url, headers, body) = chat_request(
            &profile(ProviderProtocol::OpenaiCompat, "https://api.example.com/v1"),
            ChatWire::OpenaiCompat,
            Some("be brief"),
            "a cat",
            "gpt-test",
            None,
            None,
        );
        assert_eq!(url, "https://api.example.com/v1/chat/completions");
        assert!(headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value.starts_with("Bearer ")));
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["model"], "gpt-test");
        assert_eq!(value["stream"], false);
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][0]["content"], "be brief");
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][1]["content"], "a cat");
        // FR-5.6: unset params are omitted, not null.
        assert!(value.get("max_tokens").is_none());
        assert!(value.get("temperature").is_none());
    }

    #[test]
    fn openai_request_carries_set_params() {
        let (_, _, body) = chat_request(
            &profile(ProviderProtocol::OpenaiCompat, "https://api.example.com/v1"),
            ChatWire::OpenaiCompat,
            None,
            "p",
            "m",
            Some(128),
            Some(0.7),
        );
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["max_tokens"], 128);
        assert_eq!(value["temperature"], 0.7);
        assert_eq!(value["messages"][0]["role"], "user");
    }

    #[test]
    fn anthropic_request_defaults_max_tokens_and_lifts_system() {
        let (url, headers, body) = chat_request(
            &profile(ProviderProtocol::Anthropic, "https://api.example.com/anthropic"),
            ChatWire::Anthropic,
            Some("be brief"),
            "a cat",
            "claude-test",
            None,
            None,
        );
        assert_eq!(url, "https://api.example.com/anthropic/v1/messages");
        // Dual auth headers (FR-3.1) come from the shared auth_headers.
        assert!(headers.iter().any(|(name, _)| name == "x-api-key"));
        assert!(headers
            .iter()
            .any(|(name, _)| name == "anthropic-version"));
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["system"], "be brief");
        assert_eq!(value["max_tokens"], ANTHROPIC_DEFAULT_MAX_TOKENS);
        assert_eq!(value["messages"][0]["content"], "a cat");
        assert!(value.get("temperature").is_none());

        let (_, _, body) = chat_request(
            &profile(ProviderProtocol::Anthropic, "https://api.example.com/anthropic"),
            ChatWire::Anthropic,
            None,
            "p",
            "m",
            Some(64),
            Some(0.2),
        );
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["max_tokens"], 64);
        assert_eq!(value["temperature"], 0.2);
        assert!(value.get("system").is_none());
    }

    #[test]
    fn gemini_is_not_a_chat_wire() {
        assert!(wire_for(ProviderProtocol::Gemini).is_none());
        assert_eq!(wire_for(ProviderProtocol::OpenaiCompat), Some(ChatWire::OpenaiCompat));
        assert_eq!(wire_for(ProviderProtocol::Anthropic), Some(ChatWire::Anthropic));
    }

    #[test]
    fn parses_openai_response_with_usage() {
        let (text, tokens) = parse_chat_response(
            ChatWire::OpenaiCompat,
            r#"{"choices": [{"message": {"role": "assistant", "content": "hello"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 5}}"#,
        )
        .unwrap();
        assert_eq!(text, "hello");
        assert_eq!(tokens, Some(8));
    }

    #[test]
    fn parses_anthropic_blocks_and_concatenates_text() {
        let (text, tokens) = parse_chat_response(
            ChatWire::Anthropic,
            r#"{"content": [{"type": "text", "text": "a"}, {"type": "tool_use"}, {"type": "text", "text": "b"}],
                "usage": {"input_tokens": 2, "output_tokens": 4}}"#,
        )
        .unwrap();
        assert_eq!(text, "a\nb");
        assert_eq!(tokens, Some(6));
    }

    #[test]
    fn usage_is_optional() {
        let (text, tokens) = parse_chat_response(
            ChatWire::OpenaiCompat,
            r#"{"choices": [{"message": {"content": "relay says hi"}}]}"#,
        )
        .unwrap();
        assert_eq!(text, "relay says hi");
        assert_eq!(tokens, None);
    }

    #[test]
    fn error_and_empty_shapes_are_rejected() {
        // 2xx business-error body (relay shape, FR-3.3).
        let err = parse_chat_response(
            ChatWire::OpenaiCompat,
            r#"{"error": {"message": "insufficient balance"}}"#,
        )
        .unwrap_err();
        assert!(err.contains("insufficient balance"), "{err}");
        assert!(parse_chat_response(ChatWire::OpenaiCompat, r#"{"choices": []}"#).is_err());
        assert!(parse_chat_response(ChatWire::Anthropic, r#"{"content": []}"#).is_err());
        assert!(parse_chat_response(ChatWire::Anthropic, "not json").is_err());
    }
}
