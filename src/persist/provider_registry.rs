//! Provider profile registry — `providers.json` in the config dir.
//!
//! Same shape as the plugin registry (advisory cross-process file lock +
//! tmp/rename atomic writes + locked read-modify-write), with one hard
//! difference: this file contains API keys, so it is created with 0600
//! permissions *before* any secret byte is written (unix; on Windows the
//! per-user config dir already restricts access).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::api::schema::ProviderProfile;

const REGISTRY_LOCK_FILE: &str = ".providers.lock";
#[cfg(unix)]
const REGISTRY_FILE_MODE: u32 = 0o600;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ProviderRegistryFile {
    #[serde(default)]
    profiles: Vec<ProviderProfile>,
}

fn registry_path() -> PathBuf {
    crate::config::config_dir().join("providers.json")
}

fn registry_lock_path() -> PathBuf {
    crate::config::config_dir().join(REGISTRY_LOCK_FILE)
}

fn with_registry_lock<T>(operation: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let lock_path = registry_lock_path();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.lock()?;
    operation()
}

/// Write the registry atomically. The tmp file is created with 0600 (unix)
/// before any content — including secrets — reaches the disk.
fn save_to_path(path: &Path, profiles: &[ProviderProfile]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&ProviderRegistryFile {
        profiles: profiles.to_vec(),
    })?;
    let tmp_path = path.with_extension("json.tmp");
    {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(REGISTRY_FILE_MODE)
                .open(&tmp_path)?
        };
        #[cfg(windows)]
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;

        file.write_all(json.as_bytes())?;
    }
    #[cfg(windows)]
    if path.exists() {
        if let Err(err) = std::fs::remove_file(path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

/// Locked read-modify-write: the mutation always sees the on-disk state, so
/// concurrent herdr processes (server + CLI) cannot lose updates.
pub fn update<T>(
    mutation: impl FnOnce(&mut Vec<ProviderProfile>) -> T,
) -> std::io::Result<(T, Vec<ProviderProfile>)> {
    with_registry_lock(|| {
        let mut profiles = load_from_path_strict(&registry_path())?;
        let result = mutation(&mut profiles);
        save_to_path(&registry_path(), &profiles)?;
        Ok((result, profiles))
    })
}

pub fn try_load() -> std::io::Result<Vec<ProviderProfile>> {
    with_registry_lock(|| load_from_path_strict(&registry_path()))
}

/// Lenient load: a corrupt or missing file yields an empty registry instead
/// of blocking startup. Mutations still go through strict reads, so a broken
/// file is only ever replaced by an explicit write.
pub fn load() -> Vec<ProviderProfile> {
    match try_load() {
        Ok(profiles) => profiles,
        Err(err) => {
            warn!(path = %registry_path().display(), err = %err, "failed to load provider registry");
            Vec::new()
        }
    }
}

fn load_from_path_strict(path: &Path) -> std::io::Result<Vec<ProviderProfile>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str::<ProviderRegistryFile>(&content)
        .map(|file| file.profiles)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
pub(crate) fn load_from_path(path: &Path) -> Vec<ProviderProfile> {
    load_from_path_strict(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ProviderModelEntry, ProviderModelSource, ProviderProtocol};

    fn temp_registry_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "herdr-provider-registry-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn sample_profile(id: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.to_string(),
            name: format!("Profile {id}"),
            preset_id: "custom".to_string(),
            protocol: ProviderProtocol::Anthropic,
            base_url: "https://api.example.com".to_string(),
            api_key: format!("secret-{id}-9876543210"),
            models: vec![ProviderModelEntry {
                id: "model-a".to_string(),
                visible: true,
                source: ProviderModelSource::Manual,
            }],
            weight: 1,
            is_disabled: false,
            note: None,
            created_unix: 42,
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let path = temp_registry_path("roundtrip");
        let profiles = vec![sample_profile("p1"), sample_profile("p2")];
        save_to_path(&path, &profiles).unwrap();

        let loaded = load_from_path(&path);
        assert_eq!(loaded, profiles);
        assert_eq!(loaded[0].api_key, "secret-p1-9876543210");
    }

    #[test]
    fn missing_file_returns_empty() {
        let path = temp_registry_path("missing");
        assert!(load_from_path(&path).is_empty());
    }

    #[test]
    fn corrupt_file_fails_strict_load() {
        let path = temp_registry_path("corrupt");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"not valid json {{{{").unwrap();
        assert!(load_from_path_strict(&path).is_err());
    }

    #[test]
    fn save_replaces_existing_registry_file() {
        let path = temp_registry_path("replace-existing");
        save_to_path(&path, &[sample_profile("first")]).unwrap();
        save_to_path(&path, &[sample_profile("second")]).unwrap();

        let loaded = load_from_path(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "second");
    }

    #[cfg(unix)]
    #[test]
    fn registry_file_is_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_registry_path("permissions");
        save_to_path(&path, &[sample_profile("secret")]).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "provider registry must be 0600");
    }
}
