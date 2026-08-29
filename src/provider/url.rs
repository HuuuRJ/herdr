//! URL joining and secret hygiene helpers.
//!
//! `join_url` implements the AgentFlow-proven semantics: the base URL only
//! ever goes to the root (e.g. `/v1`), endpoints are appended here; an
//! identical version segment (`/v1` + `/v1/models`) is deduplicated exactly
//! once, while different segments (`/v4` vs `/v1`) are never collapsed.

/// Join a provider base URL with an endpoint path.
///
/// - Trailing `/` on the base and leading `/` on the path are normalized.
/// - If the base already ends with the path's first segment (e.g. base
///   `.../v1`, path `/v1/models`), that segment is emitted once.
pub(crate) fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if base.is_empty() || path.is_empty() {
        return format!("{base}/{path}");
    }
    // Compare whole path segments so `av1` never matches `v1`.
    let first_segment = path.split('/').next().unwrap_or(path);
    let base_last_segment = base.rsplit('/').next().unwrap_or("");
    if !base_last_segment.is_empty() && base_last_segment == first_segment {
        let stripped = &base[..base.len() - first_segment.len()];
        return format!("{}/{}", stripped.trim_end_matches('/'), path);
    }
    format!("{base}/{path}")
}

/// Mask an API key for list/detail surfaces.
///
/// Empty or very short secrets collapse to `***`; anything longer keeps the
/// first 3 and last 4 characters so a user can recognize "which key is this"
/// without exposing it.
pub(crate) fn mask_secret(secret: &str) -> String {
    let char_count = secret.chars().count();
    if char_count <= 8 {
        return "***".to_string();
    }
    let head: String = secret.chars().take(3).collect();
    let tail: String = secret.chars().skip(char_count - 4).collect();
    format!("{head}***{tail}")
}

/// Replace every occurrence of `secret` in `text` with `***`.
///
/// Applied to anything derived from a subprocess (stderr, response bodies)
/// before it reaches logs or API error fields. An empty secret leaves the
/// text untouched — nothing to redact.
pub(crate) fn redact(secret: &str, text: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "***")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_base_and_path() {
        assert_eq!(
            join_url("https://api.example.com/v1", "/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://api.example.com", "/v1/chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn normalizes_trailing_and_leading_slashes() {
        assert_eq!(
            join_url("https://api.example.com/v1/", "chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://api.example.com//", "//v1/models"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn deduplicates_identical_version_segment_once() {
        assert_eq!(
            join_url("https://api.example.com/v1", "/v1/models"),
            "https://api.example.com/v1/models"
        );
        assert_eq!(
            join_url("https://api.example.com/api/anthropic", "/v1/messages"),
            "https://api.example.com/api/anthropic/v1/messages"
        );
    }

    #[test]
    fn different_version_segments_are_not_collapsed() {
        // `/v4` on the base and `/v1` in the path must both survive.
        assert_eq!(
            join_url("https://api.example.com/v4", "/v1/models"),
            "https://api.example.com/v4/v1/models"
        );
        // Dedup is exact-segment only: `v1beta` is not `v1`.
        assert_eq!(
            join_url("https://api.example.com/v1beta", "/v1beta/models"),
            "https://api.example.com/v1beta/models"
        );
    }

    #[test]
    fn handles_gemini_style_bases() {
        // Base already carries /v1beta; the models endpoint reuses it.
        assert_eq!(
            join_url(
                "https://generativelanguage.googleapis.com/v1beta",
                "/v1beta/models"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
    }

    #[test]
    fn dedup_matches_whole_segments_only() {
        // `av1` must not be treated as a duplicate of `v1`.
        assert_eq!(
            join_url("https://api.example.com/av1", "/v1/models"),
            "https://api.example.com/av1/v1/models"
        );
        // Single-segment path with a matching base segment still dedups.
        assert_eq!(
            join_url("https://api.example.com/v1", "/v1"),
            "https://api.example.com/v1"
        );
    }

    #[test]
    fn masks_short_and_empty_secrets_completely() {
        assert_eq!(mask_secret(""), "***");
        assert_eq!(mask_secret("abc"), "***");
        assert_eq!(mask_secret("12345678"), "***");
    }

    #[test]
    fn masks_long_secrets_with_head_and_tail() {
        assert_eq!(mask_secret("sk-1234567890abcdef"), "sk-***cdef");
        // Char-count based, not byte-count, so multibyte keys stay valid.
        // "密钥密钥密钥密钥密钥" is 10 chars: head 3 + tail 4.
        assert_eq!(mask_secret("密钥密钥密钥密钥密钥"), "密钥密***密钥密钥");
    }

    #[test]
    fn redacts_secret_occurrences() {
        let secret = "sk-abcdef123456";
        assert_eq!(
            redact(secret, "failed with key sk-abcdef123456 in body"),
            "failed with key *** in body"
        );
        assert_eq!(redact("", "unchanged"), "unchanged");
        assert_eq!(redact(secret, "no match here"), "no match here");
    }
}
