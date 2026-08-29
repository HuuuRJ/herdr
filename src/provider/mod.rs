//! Provider profiles — multi-key, multi-endpoint LLM supplier registry.
//!
//! A profile bundles `{protocol, base_url, api_key, models}` so workflow nodes
//! can bind "which model, through which key, at which address" per node. The
//! persisted registry lives at `providers.json` (0600); API responses always
//! mask the key (`api::schema::ProviderProfileInfo`).

pub(crate) mod http;

pub(crate) use http::{ProviderHttpKind, ProviderHttpResult};
pub(crate) mod presets;
pub(crate) mod url;

/// Generate a stable profile id. Millisecond timestamp plus a process-local
/// counter keeps ids unique across restarts without pulling in a uuid crate
/// (the workspace id / pane id precedents use the same atomic-counter style).
pub(crate) fn generate_profile_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_SUFFIX: AtomicU64 = AtomicU64::new(1);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let suffix = NEXT_SUFFIX.fetch_add(1, Ordering::Relaxed);
    format!("p{millis}x{suffix}")
}
