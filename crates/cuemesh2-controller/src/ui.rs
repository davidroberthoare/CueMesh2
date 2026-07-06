//! Minimal egui window: show info, cue list, connected clients, log.

use std::path::PathBuf;
use std::time::Duration;

use cuemesh2_shared::protocol::{ControllerMsg, Crossfade, FadeCmd, Layer, LoadCue, PlayAt};
use cuemesh2_shared::show::ShowFile;

use crate::server::{broadcast, now_utc_ms};
use crate::state::SharedState;

pub struct ControllerApp {
    state: SharedState,
}

impl ControllerApp {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }

    fn load_show_from_path(&self, path: PathBuf) {
        match ShowFile::load(&path) {
            Ok(sf) => {
                let mut s = self.state.lock().unwrap();
                s.show = Some(sf);
                s.show_path = Some(path.clone());
                s.selected_cue_idx = Some(0);
                s.push_log(format!("loaded show {}", path.display()));
            }
            Err(e) => {
                self.state.lock().unwrap().push_log(format!("failed to load show: {e}"));
            }
        }
    }

    fn go_selected(&self) {
        let (cue, media_root) = {
            let s = self.state.lock().unwrap();
            let Some(show) = &s.show else { return };
            let Some(idx) = s.selected_cue_idx else { return };
            let Some(cue) = show.cues.get(idx).cloned() else { return };
            (cue, show.show.media_root.clone())
        };
        let full_path = expand(&media_root).join(&cue.file);
        broadcast(
            &self.state,
            ControllerMsg::LoadCue(LoadCue {
                cue_id: cue.id.clone(),
                layer: Layer::A,
                file: full_path,
                start_ms: None,
                end_ms: None,
                volume: cue.volume,
                fade_in_ms: cue.fade_in_ms,
                fade_out_ms: cue.fade_out_ms,
                crossfade_to_next_ms: cue.crossfade_to_next_ms,
            }),
        );
        // Play with ~250ms lead time so slow-arriving clients still make it.
        broadcast(
            &self.state,
            ControllerMsg::PlayAt(PlayAt {
                layer: Layer::A,
                master_start_utc_ms: now_utc_ms() + 250,
            }),
        );
        self.state.lock().unwrap().push_log(format!("GO cue {}", cue.id));
    }
}

fn expand(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

impl eframe::App for ControllerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 30fps refresh; cheap for our data volume.
        ctx.request_repaint_after(Duration::from_millis(33));

        let (show_summary, cues, clients, log_tail): (
            String,
            Vec<(String, String)>,
            Vec<crate::state::ClientRow>,
            Vec<String>,
        ) = {
            let s = self.state.lock().unwrap();
            let show_summary = match &s.show {
                Some(sf) => format!("{}  ({} cues)", sf.show.title, sf.cues.len()),
                None => "(no show loaded)".into(),
            };
            let cues = match &s.show {
                Some(sf) => sf.cues.iter().map(|c| (c.id.clone(), c.name.clone())).collect(),
                None => vec![],
            };
            let clients: Vec<_> = s.clients.values().cloned().collect();
            let tail: Vec<_> = s.log_lines.iter().rev().take(80).cloned().collect();
            (show_summary, cues, clients, tail)
        };

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("CueMesh2 Controller");
                ui.separator();
                ui.label(show_summary);
                ui.separator();
                if ui.button("Open show…").clicked() {
                    // Simple modal-less prompt: read a path from an env var or hardcoded example.
                    // (A real file dialog can be added with `rfd` when we care about polish.)
                    let example = std::env::var("CUEMESH_SHOW").ok().or_else(|| {
                        Some(
                            std::env::current_dir()
                                .unwrap_or_default()
                                .join("examples/example_show.cuemesh.toml")
                                .to_string_lossy()
                                .into_owned(),
                        )
                    });
                    if let Some(p) = example {
                        self.load_show_from_path(PathBuf::from(p));
                    }
                }
            });
        });

        egui::SidePanel::left("cues").min_width(280.0).show(ctx, |ui| {
            ui.heading("Cues");
            ui.separator();
            let selected = self.state.lock().unwrap().selected_cue_idx;
            for (i, (id, name)) in cues.iter().enumerate() {
                let is_sel = selected == Some(i);
                if ui.selectable_label(is_sel, format!("{i:>3}  {id}  —  {name}")).clicked() {
                    self.state.lock().unwrap().selected_cue_idx = Some(i);
                }
            }
        });

        egui::SidePanel::right("clients").min_width(280.0).show(ctx, |ui| {
            ui.heading("Clients");
            ui.separator();
            if clients.is_empty() {
                ui.label("(no clients connected)");
            }
            for c in &clients {
                ui.group(|ui| {
                    ui.label(format!("{}  ({})", c.name, c.client_id));
                    ui.label(format!("addr: {}", c.addr));
                    ui.label(format!("state: {:?}", c.state));
                    if let Some(cue) = &c.current_cue {
                        ui.label(format!("cue: {cue}  @ {}ms", c.position_ms));
                    }
                    if let Some(d) = c.last_drift_ms {
                        ui.label(format!("offset: {d} ms"));
                    }
                });
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("GO").clicked() {
                    self.go_selected();
                }
                if ui.button("PAUSE").clicked() {
                    broadcast(&self.state, ControllerMsg::Pause);
                }
                if ui.button("FADE").clicked() {
                    let duration_ms = self
                        .state
                        .lock()
                        .unwrap()
                        .show
                        .as_ref()
                        .map(|s| s.show.settings.default_fade_ms)
                        .unwrap_or(1500);
                    broadcast(&self.state, ControllerMsg::Fade(FadeCmd { duration_ms }));
                }
                if ui.button("STOP").clicked() {
                    broadcast(&self.state, ControllerMsg::Stop);
                }
                ui.separator();
                if ui.button("Manual crossfade to selected").clicked() {
                    let (id, duration) = {
                        let s = self.state.lock().unwrap();
                        let Some(show) = &s.show else { return };
                        let Some(idx) = s.selected_cue_idx else { return };
                        let Some(cue) = show.cues.get(idx) else { return };
                        (cue.id.clone(), 1500u32)
                    };
                    broadcast(
                        &self.state,
                        ControllerMsg::Crossfade(Crossfade {
                            to_cue_id: id,
                            duration_ms: duration,
                        }),
                    );
                }
            });
            ui.separator();
            ui.heading("Log");
            egui::ScrollArea::vertical().show(ui, |ui| {
                for line in log_tail.iter().rev() {
                    ui.monospace(line);
                }
            });
        });
    }
}
