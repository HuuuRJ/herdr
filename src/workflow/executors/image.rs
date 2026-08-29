//! `image_gen` node: OpenAI-compatible images endpoint over curl.
//!
//! One protocol covers xAI grok image, OpenAI gpt-image, and any compatible
//! relay — the provider profile decides where the request goes. Requests
//! reuse the curl conventions from `provider::http` (stdin headers, status
//! via `-w`, never `?key=` in argv).

use crate::api::schema::ProviderProfile;

/// Build the images request `(url, body)` for a profile.
pub(crate) fn images_request(
    profile: &ProviderProfile,
    prompt: &str,
    size: Option<&str>,
    model: Option<&str>,
) -> (String, String) {
    let url = crate::provider::url::join_url(&profile.base_url, "/images/generations");
    let mut body = serde_json::json!({ "prompt": prompt });
    if let Some(size) = size {
        body["size"] = serde_json::Value::String(size.to_string());
    }
    if let Some(model) = model {
        body["model"] = serde_json::Value::String(model.to_string());
    }
    (url, body.to_string())
}

/// Response shapes we accept: `{"data":[{"b64_json": "..."}]}` or
/// `{"data":[{"url": "..."}]}`.
pub(crate) enum ImagePayload {
    Base64(String),
    Url(String),
}

pub(crate) fn parse_images_response(body: &str) -> Result<ImagePayload, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("invalid JSON response: {err}"))?;
    let first = value
        .get("data")
        .and_then(|data| data.as_array())
        .and_then(|array| array.first())
        .ok_or_else(|| "response has no data array".to_string())?;
    if let Some(b64) = first.get("b64_json").and_then(|field| field.as_str()) {
        return Ok(ImagePayload::Base64(b64.to_string()));
    }
    if let Some(url) = first.get("url").and_then(|field| field.as_str()) {
        return Ok(ImagePayload::Url(url.to_string()));
    }
    Err("image response has neither b64_json nor url".to_string())
}

/// Generate an image and write it to `output_path`. Returns the artifact
/// file name on success.
pub(crate) fn run_image_gen(
    profile: &ProviderProfile,
    prompt: &str,
    size: Option<&str>,
    model: Option<&str>,
    output_path: &std::path::Path,
) -> Result<String, String> {
    let (url, body) = images_request(profile, prompt, size, model);
    let headers = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", profile.api_key),
        ),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    let response = crate::provider::http::provider_curl_json(&url, &headers, Some(&body), 120);
    let redacted = crate::provider::url::redact(&profile.api_key, &response.body);
    let Some(status) = response.status else {
        return Err(crate::provider::url::redact(
            &profile.api_key,
            &response
                .transport_error
                .unwrap_or_else(|| "request failed".to_string()),
        ));
    };
    if !(200..300).contains(&status) {
        let detail = redacted.trim();
        return Err(format!(
            "HTTP {status}: {}",
            if detail.is_empty() {
                "image request failed"
            } else {
                detail
            }
        ));
    }

    match parse_images_response(&redacted)? {
        ImagePayload::Base64(b64) => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|err| format!("invalid base64 image payload: {err}"))?;
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create output directory: {err}"))?;
            }
            std::fs::write(output_path, bytes)
                .map_err(|err| format!("failed to write image: {err}"))?;
        }
        ImagePayload::Url(url) => {
            // Relay-hosted artifact: download it with the same curl channel.
            let headers = vec![("Accept".to_string(), "image/*".to_string())];
            let download = crate::provider::http::provider_curl_binary(&url, &headers, 240);
            let Some(status) = download.status else {
                return Err(format!(
                    "failed to download image: {}",
                    crate::provider::url::redact(
                        &profile.api_key,
                        &download
                            .transport_error
                            .unwrap_or_else(|| "request failed".to_string())
                    )
                ));
            };
            if !(200..300).contains(&status) {
                return Err(format!("image download returned HTTP {status}"));
            }
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create output directory: {err}"))?;
            }
            std::fs::write(output_path, download.bytes)
                .map_err(|err| format!("failed to write image: {err}"))?;
        }
    }
    output_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| "image output path has no file name".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::ProviderProtocol;

    fn profile(base_url: &str) -> ProviderProfile {
        ProviderProfile {
            id: "p1".to_string(),
            name: "img".to_string(),
            preset_id: "custom".to_string(),
            protocol: ProviderProtocol::OpenaiCompat,
            base_url: base_url.to_string(),
            api_key: "sk-img-1234567890".to_string(),
            models: vec![],
            weight: 1,
            is_disabled: false,
            note: None,
            created_unix: 0,
        }
    }

    #[test]
    fn request_url_joins_and_body_carries_options() {
        let (url, body) = images_request(
            &profile("https://api.example.com/v1"),
            "a cat",
            Some("1024x1024"),
            Some("gpt-image-1"),
        );
        assert_eq!(url, "https://api.example.com/v1/images/generations");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["prompt"], "a cat");
        assert_eq!(value["size"], "1024x1024");
        assert_eq!(value["model"], "gpt-image-1");
    }

    #[test]
    fn parses_b64_and_url_shapes() {
        assert!(matches!(
            parse_images_response(r#"{"data": [{"b64_json": "aGk="}]}"#).unwrap(),
            ImagePayload::Base64(_)
        ));
        assert!(matches!(
            parse_images_response(r#"{"data": [{"url": "https://cdn/x.png"}]}"#).unwrap(),
            ImagePayload::Url(_)
        ));
        assert!(parse_images_response(r#"{"error": "x"}"#).is_err());
        assert!(parse_images_response(r#"{"data": []}"#).is_err());
    }
}
