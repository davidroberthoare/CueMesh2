//! Minimal client egui window: connection status, playback readout, log.

use std::time::Duration;

use crate::state::SharedState;

pub struct ClientApp {
    state: SharedState,
}

impl ClientApp {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

impl eframe::App for ClientApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));

        let (name, id, addr, connected, pb, log_tail) = {
            let s = self.state.lock().unwrap();
            (
                s.name.clone(),
                s.client_id.clone(),
                s.controller_addr.clone(),
                s.connected,
                s.playback.clone(),
                s.log_lines.iter().rev().take(80).cloned().collect::<Vec<_>>(),
            )
        };

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("CueMesh2 Client");
                ui.separator();
                ui.label(format!("{name}  ({id})"));
                ui.separator();
                ui.label(format!(
                    "controller: {addr}   {}",
                    if connected { "● online" } else { "○ offline" }
                ));
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Playback");
            ui.label(format!("state:   {:?}", pb.state));
            ui.label(format!(
                "cue:     {}",
                pb.current_cue_id.unwrap_or_else(|| "(none)".into())
            ));
            ui.label(format!("pos:     {} ms", pb.position_ms));
            ui.label(format!(
                "alphas:  A={:.2}   B={:.2}",
                pb.layer_a_alpha, pb.layer_b_alpha
            ));
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
