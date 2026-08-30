//! Provider key-pool scheduling (P3c): group profiles, order candidates,
//! and classify failures for failover.
//!
//! Pools are implicit groups — profiles sharing a preset form one pool keyed
//! by the preset id; every custom relay is its own single-profile pool (FR-2.2).
//! Scheduling is priority-then-least-used: weight descending, then fewest
//! uses, with in-memory cooldowns (auth 60s / rate-limit 30s) pushing a
//! profile behind its uncooled siblings. Everything here is pure; the App
//! owns the mutable health state.

use std::time::{Duration, Instant};

use crate::api::schema::ProviderProfile;

/// How long an auth-classified failure cools a profile down.
pub(crate) const AUTH_COOLDOWN: Duration = Duration::from_secs(60);
/// How long a rate-limit (429) failure cools a profile down.
pub(crate) const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);
/// Failover tries at most this many profiles per node dispatch, first
/// attempt included.
pub(crate) const MAX_POOL_ATTEMPTS: usize = 2;

/// The implicit pool a profile belongs to: its preset, or itself when it is
/// a from-scratch relay (all of those share the `custom` sentinel preset, so
/// preset-only keys would lump unrelated relays together).
pub(crate) fn pool_group_key(profile: &ProviderProfile) -> &str {
    if profile.preset_id == crate::provider::presets::CUSTOM_PRESET_ID {
        &profile.id
    } else {
        &profile.preset_id
    }
}

/// Enabled profiles of a pool, in scheduler order. `cooldown_remaining` and
/// `used` come from the App's in-memory health state; ordering is:
/// available-before-cooled, then weight descending, then least-used, then a
/// stable id tiebreak. An all-cooled pool still returns members (earliest
/// to unthaw first) — cooldown reorders, it never empties the pool.
pub(crate) fn order_pool_members<'a>(
    pool: &str,
    profiles: &'a [ProviderProfile],
    used: &std::collections::HashMap<String, u32>,
    cooldown_remaining: &dyn Fn(&str) -> Option<Duration>,
) -> Vec<&'a ProviderProfile> {
    let mut members: Vec<&ProviderProfile> = profiles
        .iter()
        .filter(|profile| !profile.is_disabled && pool_group_key(profile) == pool)
        .collect();
    members.sort_by(|a, b| {
        let tier = |profile: &ProviderProfile| cooldown_remaining(&profile.id).is_some();
        tier(a)
            .cmp(&tier(b))
            .then_with(|| {
                cooldown_remaining(&a.id)
                    .unwrap_or_default()
                    .cmp(&cooldown_remaining(&b.id).unwrap_or_default())
            })
            .then_with(|| b.weight.cmp(&a.weight))
            .then_with(|| {
                used.get(&a.id)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&used.get(&b.id).copied().unwrap_or(0))
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    members
}

/// Failure classes that trigger failover to the next pool profile. Anything
/// else (transport errors, 4xx payloads, timeouts) is not a key problem —
/// switching profiles would not help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    Auth,
    RateLimit,
    Server,
}

impl FailureClass {
    /// Cooldown applied to the profile that failed with this class (auth
    /// 60s / 429 30s; server failures fail over without a cooldown).
    pub(crate) fn cooldown(self) -> Option<Duration> {
        match self {
            Self::Auth => Some(AUTH_COOLDOWN),
            Self::RateLimit => Some(RATE_LIMIT_COOLDOWN),
            Self::Server => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::RateLimit => "rate_limit",
            Self::Server => "server",
        }
    }
}

/// HTTP-status classification (401/403 → auth, 429 → rate limit,
/// 5xx → server). Other statuses are execution problems, not key problems.
pub(crate) fn classify_status(status: u16) -> Option<FailureClass> {
    match status {
        401 | 403 => Some(FailureClass::Auth),
        429 => Some(FailureClass::RateLimit),
        500..=599 => Some(FailureClass::Server),
        _ => None,
    }
}

/// Classification scans only the head and tail of a failure message: real
/// provider errors land at the very start (our own `HTTP {status}: …`
/// prefixes, CLI banners) or end (exit lines), while an agent's transcript
/// may legitimately discuss "rate limits" mid-body — classifying the whole
/// log would bench healthy keys on ordinary assistant output.
const CLASSIFY_WINDOW: usize = 512;

fn head_tail_lower(text: &str) -> String {
    if text.len() <= CLASSIFY_WINDOW * 2 {
        return text.to_ascii_lowercase();
    }
    let head: String = text.chars().take(CLASSIFY_WINDOW).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(CLASSIFY_WINDOW)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n{tail}").to_ascii_lowercase()
}

/// First `HTTP <status>` marker in a failure text (the image executor's own
/// `HTTP {status}: …` prefix, or a CLI printing one).
fn http_status_in(text: &str) -> Option<u16> {
    let mut rest: &str = text;
    while let Some(position) = rest.find("http ") {
        rest = &rest[position + 5..];
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if digits.len() == 3 {
            if let Ok(status) = digits.parse::<u16>() {
                return Some(status);
            }
        }
    }
    None
}

/// Textual failure classification for agent nodes: the CLI surfaces provider
/// failures as error text (is_error result lines, "process exited with
/// code N: …"), never as a status number. HTTP markers win when present;
/// otherwise keyword heuristics. Both run over the head/tail window only.
pub(crate) fn classify_error_text(text: &str) -> Option<FailureClass> {
    let lowered = head_tail_lower(text);
    if let Some(status) = http_status_in(&lowered) {
        return classify_status(status);
    }
    let contains_any = |needles: &[&str]| needles.iter().any(|needle| lowered.contains(needle));
    if contains_any(&[
        "unauthorized",
        "invalid api key",
        "invalid_api_key",
        "authentication",
        "api key not valid",
        "incorrect api key",
        // grok's canonical not-logged-in error (agent.rs test fixture).
        "not signed in",
    ]) {
        return Some(FailureClass::Auth);
    }
    if contains_any(&["rate limit", "rate_limit", "too many requests"]) {
        return Some(FailureClass::RateLimit);
    }
    if contains_any(&[
        "server error",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "overloaded",
        "upstream error",
    ]) {
        return Some(FailureClass::Server);
    }
    None
}

/// One tried profile of a node's pool chain (App-side bookkeeping; rendered
/// into `NodeMeta.pool_attempts` on completion).
#[derive(Debug, Clone)]
pub(crate) struct PoolAttempt {
    pub profile_id: String,
    /// Classified failure, or `None` on success — the App records cooldowns
    /// from it when the thread's chain arrives.
    pub class: Option<FailureClass>,
    /// Short outcome for the attempts chain ("pending", "ok", "HTTP 401", …).
    pub outcome: String,
}

/// Short outcome label for the attempt chain: the HTTP marker when present,
/// else the class label, else a bare "error".
pub(crate) fn attempt_outcome(error: &str) -> String {
    let lowered = head_tail_lower(error);
    if let Some(status) = http_status_in(&lowered) {
        return format!("HTTP {status}");
    }
    if let Some(class) = classify_error_text(error) {
        return class.label().to_string();
    }
    "error".to_string()
}

/// In-memory, cross-run pool health: least-used counters and cooldown
/// deadlines (auth 60s / rate-limit 30s). Never persisted — a restart starts
/// every profile fresh by design.
#[derive(Debug, Default)]
pub(crate) struct PoolHealth {
    pub used: std::collections::HashMap<String, u32>,
    pub cooldown_until: std::collections::HashMap<String, Instant>,
}

impl PoolHealth {
    /// Cooldown left for a profile, if it is currently cooling down.
    pub(crate) fn cooldown_remaining(&self, profile_id: &str, now: Instant) -> Option<Duration> {
        self.cooldown_until
            .get(profile_id)
            .filter(|until| **until > now)
            .map(|until| *until - now)
    }

    /// Count a successful dispatch (the least-used signal).
    pub(crate) fn record_use(&mut self, profile_id: &str) {
        *self.used.entry(profile_id.to_string()).or_insert(0) += 1;
    }

    /// Apply a failure's cooldown (auth / rate-limit classes only; server
    /// failures fail over without a cooldown).
    pub(crate) fn penalize(&mut self, profile_id: &str, class: FailureClass, now: Instant) {
        if let Some(duration) = class.cooldown() {
            self.cooldown_until
                .insert(profile_id.to_string(), now + duration);
        }
    }
}

/// Render the attempt chain for the run record, e.g. `pa(HTTP 401)→pb(ok)`.
pub(crate) fn format_attempts(attempts: &[PoolAttempt]) -> String {
    attempts
        .iter()
        .map(|attempt| format!("{}({})", attempt.profile_id, attempt.outcome))
        .collect::<Vec<_>>()
        .join("→")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::ProviderProtocol;

    fn profile(id: &str, preset: &str, weight: u32, disabled: bool) -> ProviderProfile {
        ProviderProfile {
            id: id.to_string(),
            name: id.to_string(),
            preset_id: preset.to_string(),
            protocol: ProviderProtocol::OpenaiCompat,
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-x".to_string(),
            models: vec![],
            weight,
            is_disabled: disabled,
            note: None,
            created_unix: 0,
        }
    }

    fn none(_: &str) -> Option<Duration> {
        None
    }

    #[test]
    fn custom_relays_form_their_own_pools() {
        assert_eq!(pool_group_key(&profile("a", "kimi", 1, false)), "kimi");
        assert_eq!(pool_group_key(&profile("r1", "custom", 1, false)), "r1");
        assert_eq!(pool_group_key(&profile("r2", "custom", 1, false)), "r2");
    }

    #[test]
    fn orders_by_weight_then_least_used_then_id() {
        let profiles = vec![
            profile("low", "kimi", 1, false),
            profile("high2", "kimi", 5, false),
            profile("high1", "kimi", 5, false),
            profile("other", "glm", 9, false),
            profile("off", "kimi", 9, true),
        ];
        let used: std::collections::HashMap<String, u32> =
            [("high2".to_string(), 3u32)].into_iter().collect();
        let order: Vec<&str> = order_pool_members("kimi", &profiles, &used, &|_| None)
            .iter()
            .map(|profile| profile.id.as_str())
            .collect();
        assert_eq!(order, vec!["high1", "high2", "low"]);
        // Disabled members never join.
        assert!(!order.contains(&"off"));
    }

    #[test]
    fn cooled_profiles_sort_behind_but_still_available() {
        let profiles = vec![
            profile("hot", "kimi", 1, false),
            profile("cold_short", "kimi", 9, false),
            profile("cold_long", "kimi", 9, false),
        ];
        let remaining = |id: &str| -> Option<Duration> {
            match id {
                "cold_short" => Some(Duration::from_secs(5)),
                "cold_long" => Some(Duration::from_secs(50)),
                _ => None,
            }
        };
        let order: Vec<&str> = order_pool_members(
            "kimi",
            &profiles,
            &std::collections::HashMap::new(),
            &remaining,
        )
        .iter()
        .map(|profile| profile.id.as_str())
        .collect();
        // Available first (even at lower weight); among cooled, earliest to
        // unthaw wins.
        assert_eq!(order, vec!["hot", "cold_short", "cold_long"]);

        // An entirely cooled pool still returns its members, unthaw-first.
        let all_cold = |_: &str| Some(Duration::from_secs(30));
        let order: Vec<&str> = order_pool_members(
            "kimi",
            &profiles,
            &std::collections::HashMap::new(),
            &all_cold,
        )
        .iter()
        .map(|profile| profile.id.as_str())
        .collect();
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn empty_pool_returns_nothing() {
        let profiles = vec![profile("a", "kimi", 1, false)];
        assert!(
            order_pool_members("glm", &profiles, &std::collections::HashMap::new(), &none)
                .is_empty()
        );
    }

    #[test]
    fn classifies_http_statuses() {
        assert_eq!(classify_status(401), Some(FailureClass::Auth));
        assert_eq!(classify_status(403), Some(FailureClass::Auth));
        assert_eq!(classify_status(429), Some(FailureClass::RateLimit));
        assert_eq!(classify_status(500), Some(FailureClass::Server));
        assert_eq!(classify_status(503), Some(FailureClass::Server));
        assert_eq!(classify_status(404), None);
        assert_eq!(classify_status(200), None);
        assert_eq!(FailureClass::Auth.cooldown(), Some(AUTH_COOLDOWN));
        assert_eq!(
            FailureClass::RateLimit.cooldown(),
            Some(RATE_LIMIT_COOLDOWN)
        );
        assert_eq!(FailureClass::Server.cooldown(), None);
    }

    #[test]
    fn classifies_error_text_by_marker_then_keywords() {
        assert_eq!(
            classify_error_text("HTTP 401: invalid token"),
            Some(FailureClass::Auth)
        );
        assert_eq!(
            classify_error_text("process exited with code 1: HTTP 429: too many requests"),
            Some(FailureClass::RateLimit)
        );
        assert_eq!(
            classify_error_text("HTTP 502: bad gateway"),
            Some(FailureClass::Server)
        );
        assert_eq!(
            classify_error_text("API Error: 401 Invalid API key provided"),
            Some(FailureClass::Auth)
        );
        assert_eq!(
            classify_error_text("error: unauthorized"),
            Some(FailureClass::Auth)
        );
        // grok's canonical not-logged-in line (agent.rs test fixture).
        assert_eq!(
            classify_error_text("Not signed in. Run `grok login` to authenticate."),
            Some(FailureClass::Auth)
        );
        assert_eq!(
            classify_error_text("rate limit exceeded, retry later"),
            Some(FailureClass::RateLimit)
        );
        assert_eq!(
            classify_error_text("the server is overloaded"),
            Some(FailureClass::Server)
        );
        // Not key problems: no failover. The artifact-download error is
        // deliberately phrased so it never classifies (CDN problem, not a
        // key problem — retrying would bill a second generation).
        assert_eq!(classify_error_text("HTTP 404: not found"), None);
        assert_eq!(
            classify_error_text("image download failed with status 403"),
            None
        );
        assert_eq!(classify_error_text("node timed out"), None);
        assert_eq!(classify_error_text("process exited with code 1"), None);
        assert_eq!(classify_error_text(""), None);
    }

    #[test]
    fn mid_transcript_keywords_do_not_classify() {
        // An agent transcript that merely DISCUSSES provider errors must not
        // bench a healthy key: classification scans only the head and tail.
        let discussion = format!(
            "{}\nthe assistant wrote: 'watch out for rate limit errors and \
             HTTP 502 from the relay'\n{}",
            " ".repeat(1200),
            " ".repeat(1200),
        );
        assert_eq!(classify_error_text(&discussion), None);
        // The same keywords at the very end (a real CLI exit line) still hit.
        let tail_error = format!(
            "{}\nprocess exited with code 1: upstream error",
            " ".repeat(1200),
        );
        assert_eq!(classify_error_text(&tail_error), Some(FailureClass::Server));
        // And at the very start (our own error prefixes).
        let head_error = format!("HTTP 429: too many requests\n{}", " ".repeat(1200),);
        assert_eq!(
            classify_error_text(&head_error),
            Some(FailureClass::RateLimit)
        );
    }

    #[test]
    fn attempt_outcomes_prefer_http_markers() {
        assert_eq!(attempt_outcome("HTTP 401: bad token"), "HTTP 401");
        assert_eq!(attempt_outcome("the server is overloaded"), "server");
        assert_eq!(attempt_outcome("node timed out"), "error");
    }

    #[test]
    fn formats_attempt_chains() {
        let attempts = vec![
            PoolAttempt {
                profile_id: "pa".to_string(),
                class: Some(FailureClass::Auth),
                outcome: "HTTP 401".to_string(),
            },
            PoolAttempt {
                profile_id: "pb".to_string(),
                class: None,
                outcome: "ok".to_string(),
            },
        ];
        assert_eq!(format_attempts(&attempts), "pa(HTTP 401)→pb(ok)");
        assert_eq!(format_attempts(&[]), "");
    }
}
