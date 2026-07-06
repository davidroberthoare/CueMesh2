//! Central controller state shared between the network tasks and the egui thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use cuemesh2_shared::protocol::{ClientState, ControllerMsg};
use cuemesh2_shared::show::ShowFile;

/// A connected client from the controller's point of view.
#[derive(Debug, Clone)]
pub struct ClientRow {
    pub client_id: String,
    pub name: String,
    pub addr: String,
    pub state: ClientState,
    pub current_cue: Option<String>,
    pub position_ms: u64,
    pub last_drift_ms: Option<i64>,
    pub last_heartbeat_ms: u64,
    /// Outbound queue to the WebSocket task for this client.
    pub outbound: mpsc::Sender<ControllerMsg>,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub show: Option<ShowFile>,
    pub show_path: Option<PathBuf>,
    pub selected_cue_idx: Option<usize>,
    pub clients: HashMap<String, ClientRow>,
    pub blacklist: Vec<String>,
    /// Log lines shown in the UI. Bounded — oldest entries drop.
    pub log_lines: Vec<String>,
}

impl AppState {
    pub fn push_log(&mut self, line: impl Into<String>) {
        const CAP: usize = 500;
        self.log_lines.push(line.into());
        if self.log_lines.len() > CAP {
            let drop = self.log_lines.len() - CAP;
            self.log_lines.drain(..drop);
        }
    }
}

pub type SharedState = Arc<Mutex<AppState>>;

pub fn shared() -> SharedState {
    Arc::new(Mutex::new(AppState::default()))
}
