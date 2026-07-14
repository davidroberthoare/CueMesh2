//! Persisted client identity: a stable id that survives process restarts,
//! plus a display name the controller may assign.
//!
//! `client_id` used to be a fresh UUID generated on every launch, which meant
//! nothing durable (like a cue's client whitelist) could anchor to it across
//! a restart. This file makes it stable: generated once, then read back on
//! every subsequent launch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Identity {
    pub client_id: String,
    /// Name the controller has explicitly assigned, if any. Overrides
    /// `MULTIPLEX_NAME`/hostname until reassigned.
    #[serde(default)]
    pub assigned_name: Option<String>,
}

/// `MULTIPLEX_IDENTITY_PATH` if set — otherwise `<config_dir>/multiplex-client/
/// identity.toml`, falling back to `~/.multiplex_client/identity.toml` if no
/// config dir is available (mirrors the `media_root` fallback in `main.rs`).
///
/// The env var matters for running multiple clients on one machine (e.g.
/// local dev/testing): they'd otherwise all read `$HOME`'s single identity
/// file and collide on the same `client_id`, which — now that it's the
/// stable key cue targeting anchors to — would make them indistinguishable
/// to the controller. Point each instance at its own path instead.
pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("MULTIPLEX_IDENTITY_PATH") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .map(|d| d.join("multiplex-client").join("identity.toml"))
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".multiplex_client")
                .join("identity.toml")
        })
}

/// Load the identity at `path`, generating and persisting a fresh one (new
/// UUID, no assigned name) if the file is missing, unreadable, or corrupt.
/// Never panics — a bad identity file just means a new persistent id.
pub fn load_or_create(path: &Path) -> Identity {
    if let Ok(raw) = std::fs::read_to_string(path) {
        if let Ok(identity) = toml::from_str::<Identity>(&raw) {
            if !identity.client_id.is_empty() {
                return identity;
            }
        }
    }
    let identity = Identity {
        client_id: uuid::Uuid::new_v4().to_string(),
        assigned_name: None,
    };
    let _ = save(path, &identity);
    identity
}

/// Persist `identity` to `path`, creating parent directories as needed.
pub fn save(path: &Path, identity: &Identity) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(identity)?;
    std::fs::write(path, toml)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("multiplex_identity_test_{}", uuid::Uuid::new_v4()))
            .join("identity.toml")
    }

    #[test]
    fn missing_file_generates_and_persists_a_uuid() {
        let path = temp_path();
        assert!(!path.exists());
        let first = load_or_create(&path);
        assert!(!first.client_id.is_empty());
        assert!(path.exists());
        let second = load_or_create(&path);
        assert_eq!(first.client_id, second.client_id);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_file_does_not_panic_and_regenerates() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not valid toml {{{").unwrap();
        let identity = load_or_create(&path);
        assert!(!identity.client_id.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn assigned_name_roundtrips_through_save_and_reload() {
        let path = temp_path();
        let mut identity = load_or_create(&path);
        identity.assigned_name = Some("center-top".into());
        save(&path, &identity).unwrap();
        let reloaded = load_or_create(&path);
        assert_eq!(reloaded.assigned_name.as_deref(), Some("center-top"));
        assert_eq!(reloaded.client_id, identity.client_id);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
