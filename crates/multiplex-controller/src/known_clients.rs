//! A durable id → name directory for clients, persisted next to the
//! controller binary.
//!
//! `AppState.clients` only holds currently-connected rows, but the cue
//! editor's client picker needs to offer every client the operator has ever
//! seen or named — including ones that are offline right now. This module is
//! the first bit of persisted controller state (everything else lives only
//! in memory) for exactly that reason.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use multiplex_shared::protocol::{AssignName, ControllerMsg};

use crate::server::send_to;
use crate::state::SharedState;

/// What the controller knows about one client, independent of whether it's
/// currently connected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownClient {
    pub name: String,
    /// True once an operator has explicitly renamed this client via the
    /// roster. Protects the name from being overwritten by the client's own
    /// self-reported HELLO name, and is the signal to (re)send ASSIGN_NAME
    /// the next time this client connects.
    #[serde(default)]
    pub assigned: bool,
    #[serde(default)]
    pub last_seen_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownClientsFile {
    #[serde(default)]
    clients: HashMap<String, KnownClient>,
}

/// `<dir>/known_clients.toml`, where `<dir>` is `MULTIPLEX_DATA_DIR` if set,
/// else the directory containing the controller binary — mirrors
/// `update::bundle_dir()`'s precedent for controller-side files.
pub fn default_path() -> PathBuf {
    let dir = std::env::var("MULTIPLEX_DATA_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    });
    dir.join("known_clients.toml")
}

/// Load the known-clients directory from `path`. An absent or corrupt file
/// just means an empty directory — every currently-connecting client will
/// populate it fresh via HELLO.
pub fn load(path: &Path) -> HashMap<String, KnownClient> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str::<KnownClientsFile>(&raw).ok())
        .map(|f| f.clients)
        .unwrap_or_default()
}

/// Persist the known-clients directory to `path`.
pub fn save(path: &Path, clients: &HashMap<String, KnownClient>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = KnownClientsFile {
        clients: clients.clone(),
    };
    if let Ok(toml) = toml::to_string_pretty(&file) {
        let _ = std::fs::write(path, toml);
    }
}

/// Rename a client (by persistent id) from the operator roster: marks it
/// `assigned` so a later HELLO from that client can't silently overwrite the
/// name, updates the live roster row if it's connected, persists to disk,
/// and pushes ASSIGN_NAME immediately if reachable.
pub fn rename(state: &SharedState, client_id: &str, name: &str) {
    let (path, snapshot) = {
        let mut s = state.lock().unwrap();
        let entry = s.known_clients.entry(client_id.to_string()).or_insert_with(|| KnownClient {
            name: name.to_string(),
            assigned: false,
            last_seen_ms: 0,
        });
        entry.name = name.to_string();
        entry.assigned = true;
        if let Some(row) = s.clients.get_mut(client_id) {
            row.name = name.to_string();
        }
        (s.known_clients_path.clone(), s.known_clients.clone())
    };
    save(&path, &snapshot);
    send_to(
        state,
        &[client_id.to_string()],
        ControllerMsg::AssignName(AssignName {
            client_id: client_id.to_string(),
            name: name.to_string(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "multiplex_known_clients_test_{}_{n}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_loads_empty() {
        let path = temp_path();
        assert!(load(&path).is_empty());
    }

    #[test]
    fn save_load_roundtrip() {
        let path = temp_path();
        let mut clients = HashMap::new();
        clients.insert(
            "client-a".to_string(),
            KnownClient {
                name: "center-top".into(),
                assigned: true,
                last_seen_ms: 12345,
            },
        );
        save(&path, &clients);
        let back = load(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back["client-a"].name, "center-top");
        assert!(back["client-a"].assigned);
        assert_eq!(back["client-a"].last_seen_ms, 12345);
        let _ = std::fs::remove_file(&path);
    }
}
