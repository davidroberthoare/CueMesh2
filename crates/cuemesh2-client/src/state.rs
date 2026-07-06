//! Client-side state shared between the network task and the egui window.

use std::sync::{Arc, Mutex};

use cuemesh2_shared::protocol::ClientState;

#[derive(Debug, Default)]
pub struct AppState {
    pub client_id: String,
    pub name: String,
    pub controller_addr: String,
    pub connected: bool,
    pub playback: ClientPlayback,
    pub log_lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientPlayback {
    pub state: PlaybackState,
    pub current_cue_id: Option<String>,
    pub position_ms: u64,
    pub layer_a_alpha: f32,
    pub layer_b_alpha: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    #[default]
    Idle,
    Loading,
    Ready,
    Playing,
    Paused,
    Error,
    Black,
}

impl From<PlaybackState> for ClientState {
    fn from(v: PlaybackState) -> Self {
        match v {
            PlaybackState::Idle => ClientState::Idle,
            PlaybackState::Loading => ClientState::Loading,
            PlaybackState::Ready => ClientState::Ready,
            PlaybackState::Playing => ClientState::Playing,
            PlaybackState::Paused => ClientState::Paused,
            PlaybackState::Error => ClientState::Error,
            PlaybackState::Black => ClientState::Black,
        }
    }
}

impl AppState {
    pub fn push_log(&mut self, line: impl Into<String>) {
        const CAP: usize = 200;
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
