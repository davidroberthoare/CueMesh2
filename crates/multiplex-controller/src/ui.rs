//! Operator window: cue list with GO/NEXT/PREV, transport, client roster
//! with preflight results and media push, log view.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_file_dialog::FileDialog;
use egui_phosphor::regular as icon;

use multiplex_shared::protocol::{ControllerMsg, FadeCmd, MediaFileStatus};
use multiplex_shared::show::{CueKind, ShowFile};

use crate::editor::{EditorAction, EditorState};
use crate::preflight;
use crate::server::{broadcast, now_utc_ms};
use crate::state::{ClientUpdate, SelfUpdate, SharedState};
use crate::update;

/// A file dialog pre-filtered to MultiPlex show files.
fn show_dialog() -> FileDialog {
    let filter: Arc<dyn Fn(&Path) -> bool + Send + Sync> =
        Arc::new(|p| p.to_string_lossy().to_ascii_lowercase().ends_with(".multiplex.toml"));
    FileDialog::new()
        .add_file_filter("MultiPlex shows (*.multiplex.toml)", filter)
        .default_file_filter("MultiPlex shows (*.multiplex.toml)")
        .default_file_name("show.multiplex.toml")
}

/// How long a cue selection must hold still before we preload it. Keeps
/// arrow-key scrolling from firing a cold decode on every intermediate cue.
const SELECTION_SETTLE_MS: u128 = 150;

pub struct ControllerApp {
    state: SharedState,
    testscreen_on: bool,
    editor: EditorState,
    /// Last selection we observed, and when it last changed — drives the
    /// standby debounce.
    last_selected: Option<usize>,
    selected_since: Instant,
    /// "Open show…" picker.
    open_dialog: FileDialog,
    /// Editor "Save as…" picker.
    save_dialog: FileDialog,
    /// In-progress rename text, keyed by client id, while its roster row's
    /// rename field is open. Pure UI scratch state — not persisted here;
    /// `known_clients::rename` is what actually commits a rename.
    rename_drafts: std::collections::HashMap<String, String>,
}

impl ControllerApp {
    pub fn new(state: SharedState) -> Self {
        let app = Self {
            state,
            testscreen_on: false,
            editor: EditorState::default(),
            last_selected: None,
            selected_since: Instant::now(),
            open_dialog: show_dialog(),
            save_dialog: show_dialog(),
            rename_drafts: std::collections::HashMap::new(),
        };
        // Auto-load the show named by MULTIPLEX_SHOW so headless-ish setups
        // (and operators with a fixed show) skip the open dialog entirely.
        if let Ok(p) = std::env::var("MULTIPLEX_SHOW") {
            app.load_show_from_path(PathBuf::from(p));
        }
        app
    }

    fn load_show_from_path(&self, path: PathBuf) {
        match ShowFile::load(&path) {
            Ok(sf) => {
                {
                    let mut s = self.state.lock().unwrap();
                    s.show = Some(sf);
                    s.show_path = Some(path.clone());
                    s.selected_cue_idx = Some(0);
                    s.run = Default::default();
                    s.reset_client_layers();
                    s.local_media = None;
                    s.push_log(format!("loaded show {}", path.display()));
                }
                // Every connected client needs the new cue list.
                if let Some(msg) = crate::server::show_sync_msg(&self.state) {
                    broadcast(&self.state, msg);
                }
            }
            Err(e) => {
                self.state.lock().unwrap().push_log(format!("failed to load show: {e}"));
            }
        }
    }

    /// Fire the selected cue on every client it targets, with lead time,
    /// crossfading from whatever each of those clients has on air — each
    /// client's own layer/STANDBY state (on its `ClientRow`) decides whether
    /// its LOAD_CUE is skipped in favour of an instant PLAY_AT, same as
    /// before, just resolved per client instead of once for the whole fleet.
    /// Clients this cue excludes get its `exclude_action` instead. Advances
    /// the selection.
    fn go_selected(&self) {
        let plan = {
            let mut s = self.state.lock().unwrap();
            let Some(show) = &s.show else { return };
            let Some(idx) = s.selected_cue_idx else { return };
            let Some(cue) = show.cues.get(idx).cloned() else { return };
            let n = show.cues.len();
            let lead_ms = show.show.sync.start_lead_ms.max(250) as u64;
            let all_ids: Vec<String> = s.clients.keys().cloned().collect();

            s.run.playing_cue_idx = Some(idx);
            s.selected_cue_idx = Some((idx + 1).min(n.saturating_sub(1)));
            s.push_log(format!("GO cue {}", cue.id));
            (cue, lead_ms, all_ids)
        };
        let (cue, lead_ms, all_ids) = plan;
        let (included, excluded) =
            multiplex_shared::show::partition_clients(&cue, all_ids.iter().map(String::as_str));
        for client_id in &included {
            crate::dispatch::go_for_client(&self.state, &cue, client_id, lead_ms);
        }
        crate::dispatch::apply_exclude_action(&self.state, &cue, &excluded, lead_ms);
    }

    /// Preload the selected cue onto the layer the next GO will use, for
    /// every client it targets, so that GO starts instantly. Debounced (so
    /// scrolling doesn't thrash clients); per-client gating (already
    /// preloaded, target layer still mid-crossfade) lives in
    /// `dispatch::standby_for_client`. Clients the cue excludes get no
    /// STANDBY — there's nothing to hide a cold decode for.
    fn maybe_standby(&self) {
        if self.selected_since.elapsed().as_millis() < SELECTION_SETTLE_MS {
            return;
        }
        let plan = {
            let s = self.state.lock().unwrap();
            let Some(show) = &s.show else { return };
            let Some(idx) = s.selected_cue_idx else { return };
            let Some(cue) = show.cues.get(idx).cloned() else { return };
            let all_ids: Vec<String> = s.clients.keys().cloned().collect();
            (cue, all_ids)
        };
        let (cue, all_ids) = plan;
        for client_id in all_ids.iter().filter(|id| cue.targets(id)) {
            crate::dispatch::standby_for_client(&self.state, &cue, client_id);
        }
    }

    fn move_selection(&self, delta: i64) {
        let mut s = self.state.lock().unwrap();
        let Some(show) = &s.show else { return };
        let n = show.cues.len();
        if n == 0 {
            return;
        }
        let cur = s.selected_cue_idx.unwrap_or(0) as i64;
        s.selected_cue_idx = Some((cur + delta).clamp(0, n as i64 - 1) as usize);
    }

    fn blackout(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.run = Default::default();
            s.reset_client_layers();
        }
        broadcast(
            &self.state,
            ControllerMsg::Fade(FadeCmd {
                duration_ms: multiplex_shared::show::DEFAULT_FADE_MS,
            }),
        );
    }

    fn stop_all(&self) {
        {
            let mut s = self.state.lock().unwrap();
            s.run = Default::default();
            s.reset_client_layers();
        }
        broadcast(&self.state, ControllerMsg::Stop);
    }

    /// Enter the editor seeded from the running show (or a blank one).
    fn open_editor(&mut self, blank: bool) {
        let (show, path, known_clients) = {
            let s = self.state.lock().unwrap();
            // Merge the durable known-clients directory (covers offline
            // clients) with the live roster (authoritative name while
            // connected), so the cue editor's client picker can target
            // anyone the operator has ever seen, not just who's online now.
            let mut merged: std::collections::HashMap<String, String> = s
                .known_clients
                .iter()
                .map(|(id, kc)| (id.clone(), kc.name.clone()))
                .collect();
            for (id, row) in &s.clients {
                merged.insert(id.clone(), row.name.clone());
            }
            let mut known_clients: Vec<(String, String)> = merged.into_iter().collect();
            known_clients.sort_by(|a, b| a.1.cmp(&b.1));
            (s.show.clone(), s.show_path.clone(), known_clients)
        };
        if blank {
            self.editor.enter(None, None, known_clients);
        } else {
            self.editor.enter(show.as_ref(), path.as_deref(), known_clients);
        }
    }

    /// Push an edited show into the running state and re-sync every client.
    /// Resets run position (cue indices may have changed) and clears preflight.
    fn apply_show(&self, show: ShowFile) {
        let empty = show.cues.is_empty();
        {
            let mut s = self.state.lock().unwrap();
            s.show = Some(show);
            s.selected_cue_idx = if empty { None } else { Some(0) };
            s.run = Default::default();
            s.reset_client_layers();
            s.local_media = None;
            s.push_log("show updated from editor");
        }
        // Guard dropped above: `show_sync_msg`/`broadcast` re-lock `state`.
        if let Some(msg) = crate::server::show_sync_msg(&self.state) {
            broadcast(&self.state, msg);
        }
    }

    /// Render the editor and act on its result.
    fn editor_panel(&mut self, ctx: &egui::Context) {
        let mut action = EditorAction::None;
        egui::CentralPanel::default().show(ctx, |ui| {
            action = self.editor.ui(ui);
        });
        match action {
            EditorAction::None => {}
            EditorAction::Apply => {
                let show = self.editor.build();
                match show.validate() {
                    Ok(()) => {
                        self.apply_show(show);
                        self.editor.set_status("applied to running show");
                    }
                    Err(e) => self.editor.set_status(format!("invalid: {e}")),
                }
            }
            // Save to the known path, or fall through to Save-as when unset.
            EditorAction::Save => match self.editor.save_path() {
                Some(path) => self.save_editor_to(path),
                None => self.save_dialog.save_file(),
            },
            EditorAction::SaveAs => self.save_dialog.save_file(),
            EditorAction::Close => self.editor.open = false,
        }
    }

    /// Build the draft, write it to `path`, remember the path, and push it live.
    fn save_editor_to(&mut self, path: PathBuf) {
        let show = self.editor.build();
        match show.save(&path) {
            Ok(()) => {
                self.editor.set_path(&path);
                self.state.lock().unwrap().show_path = Some(path.clone());
                self.apply_show(show);
                self.editor.set_status(format!("saved to {}", path.display()));
            }
            Err(e) => self.editor.set_status(format!("save failed: {e}")),
        }
    }

    /// Advance both file dialogs and act on a completed pick. Call last in the
    /// frame so the dialog window renders on top of the panels.
    fn drive_dialogs(&mut self, ctx: &egui::Context) {
        self.open_dialog.update(ctx);
        if let Some(path) = self.open_dialog.take_selected() {
            self.load_show_from_path(path);
        }
        self.save_dialog.update(ctx);
        if let Some(path) = self.save_dialog.take_selected() {
            self.save_editor_to(path);
        }
    }
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

impl eframe::App for ControllerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 30fps refresh; cheap for our data volume.
        ctx.request_repaint_after(Duration::from_millis(33));

        // Keyboard: space = GO, arrows = move selection. Only in run mode and
        // when no widget (e.g. an editor text field) wants the keyboard.
        if !self.editor.open && !ctx.wants_keyboard_input() {
            ctx.input(|i| {
                if i.key_pressed(egui::Key::Space) {
                    self.go_selected();
                }
                if i.key_pressed(egui::Key::ArrowDown) {
                    self.move_selection(1);
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    self.move_selection(-1);
                }
            });
        }

        let (
            show_summary,
            cues,
            playing_idx,
            selected,
            clients,
            preflight_running,
            log_tail,
            update_manifest,
            self_update,
        ) = {
            let s = self.state.lock().unwrap();
            let show_summary = match &s.show {
                Some(sf) => format!("{}  ({} cues)", sf.show.title, sf.cues.len()),
                None => "(no show loaded)".into(),
            };
            let cues: Vec<_> = match &s.show {
                Some(sf) => sf
                    .cues
                    .iter()
                    .map(|c| (c.name.clone(), c.kind, c.fade_in_ms))
                    .collect(),
                None => vec![],
            };
            let clients: Vec<_> = s.clients.values().cloned().collect();
            let tail: Vec<_> = s.log_lines.iter().rev().take(80).cloned().collect();
            (
                show_summary,
                cues,
                s.run.playing_cue_idx,
                s.selected_cue_idx,
                clients,
                s.preflight_running,
                tail,
                s.update_manifest.clone(),
                s.self_update.clone(),
            )
        };

        // Track selection changes to debounce speculative preloading, then
        // (in run mode) preload the settled selection so GO is instant.
        if self.last_selected != selected {
            self.last_selected = selected;
            self.selected_since = Instant::now();
        }
        if !self.editor.open {
            self.maybe_standby();
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("MultiPlex Controller");
                ui.separator();
                ui.label(show_summary);
                ui.separator();
                if ui.button(format!("{}  Open show", icon::FOLDER_OPEN)).clicked() {
                    self.open_dialog.select_file();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        !preflight_running,
                        egui::Button::new(format!("{}  Preflight", icon::CHECK_CIRCLE)),
                    )
                    .clicked()
                {
                    preflight::start_preflight(&self.state);
                }
                if preflight_running {
                    ui.spinner();
                }
                ui.separator();
                let ts_label = if self.testscreen_on {
                    format!("{}  Hide testscreen", icon::EYE_SLASH)
                } else {
                    format!("{}  Testscreen", icon::EYE)
                };
                if ui.button(ts_label).clicked() {
                    self.testscreen_on = !self.testscreen_on;
                    broadcast(
                        &self.state,
                        if self.testscreen_on {
                            ControllerMsg::ShowTestscreen
                        } else {
                            ControllerMsg::HideTestscreen
                        },
                    );
                }
                ui.separator();
                if !self.editor.open {
                    if ui.button(format!("{}  New show", icon::FILE_PLUS)).clicked() {
                        self.open_editor(true);
                    }
                    if ui.button(format!("{}  Edit show", icon::PENCIL_SIMPLE)).clicked() {
                        self.open_editor(false);
                    }
                }
                ui.separator();
                // Two independent operator actions: update the controller
                // itself (needs internet, also refreshes the client bundle),
                // then optionally update the fleet from the local bundle.
                match &self_update {
                    SelfUpdate::Idle | SelfUpdate::Failed(_) => {
                        if ui
                            .button(format!("{}  Update controller", icon::DOWNLOAD_SIMPLE))
                            .on_hover_text(format!(
                                "v{} — check the release server for a newer version",
                                update::APP_VERSION
                            ))
                            .clicked()
                        {
                            update::start_self_update(&self.state);
                        }
                        if let SelfUpdate::Failed(e) = &self_update {
                            ui.colored_label(egui::Color32::from_rgb(220, 70, 70), icon::WARNING)
                                .on_hover_text(e);
                        }
                    }
                    SelfUpdate::Working(what) => {
                        ui.spinner();
                        ui.label(what);
                    }
                    SelfUpdate::ReadyToRestart(v) => {
                        if ui
                            .button(format!("{}  Restart into v{v}", icon::ARROW_CLOCKWISE))
                            .clicked()
                        {
                            update::restart_into_staged(&self.state);
                        }
                    }
                }
                if let Some(m) = &update_manifest {
                    let outdated = clients.iter().any(|c| update::available_for(m, c).is_some());
                    if outdated
                        && ui
                            .button(format!("{}  Update fleet (v{})", icon::UPLOAD_SIMPLE, m.version))
                            .on_hover_text("Stage the new client binary on every out-of-date client")
                            .clicked()
                    {
                        update::update_fleet(&self.state);
                    }
                    let staged = clients
                        .iter()
                        .filter(|c| matches!(c.update, ClientUpdate::Staged(_)))
                        .count();
                    if staged > 0
                        && ui
                            .button(format!("{}  Apply fleet ({staged} staged)", icon::CHECK_FAT))
                            .on_hover_text("Restart every staged client into the new version (idle clients only)")
                            .clicked()
                    {
                        update::apply_fleet(&self.state);
                    }
                }
            });
        });

        // Edit mode takes over the body; run-mode panels are hidden.
        if self.editor.open {
            self.editor_panel(ctx);
            self.drive_dialogs(ctx);
            return;
        }

        egui::SidePanel::left("cues").min_width(300.0).show(ctx, |ui| {
            ui.heading("Cues");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, (name, kind, _fade_in)) in cues.iter().enumerate() {
                    let is_sel = selected == Some(i);
                    let on_air = playing_idx == Some(i);
                    let marker = if on_air { icon::PLAY } else { " " };
                    let kind_icon = match kind {
                        CueKind::Video => icon::FILM_STRIP,
                        CueKind::Image => icon::IMAGE,
                        CueKind::Color => icon::PALETTE,
                    };
                    let label = format!("{marker} {i:>3}  {kind_icon} {name}");
                    if ui.selectable_label(is_sel, label).clicked() {
                        self.state.lock().unwrap().selected_cue_idx = Some(i);
                    }
                }
            });
        });

        egui::SidePanel::right("clients").min_width(320.0).show(ctx, |ui| {
            ui.heading("Clients");
            ui.separator();
            if clients.is_empty() {
                ui.label("(no clients connected)");
            }
            let now = now_utc_ms();
            for c in &clients {
                ui.group(|ui| {
                    let stale = now.saturating_sub(c.last_heartbeat_ms) > 3000;
                    let dot_color = if stale {
                        egui::Color32::from_rgb(220, 70, 70)
                    } else {
                        egui::Color32::from_rgb(80, 200, 110)
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(dot_color, icon::CIRCLE);
                        ui.label(format!("{}  ({})", c.name, &c.client_id[..8.min(c.client_id.len())]));
                        if ui.small_button(icon::PENCIL_SIMPLE).on_hover_text("Rename").clicked() {
                            self.rename_drafts.entry(c.client_id.clone()).or_insert_with(|| c.name.clone());
                        }
                    });
                    // Inline rename: commits via `known_clients::rename`, which
                    // marks the client `assigned` (so its own HELLO can't
                    // silently overwrite this name again) and pushes
                    // ASSIGN_NAME immediately if it's still connected.
                    if let Some(draft) = self.rename_drafts.get(&c.client_id).cloned() {
                        let mut draft = draft;
                        let mut commit = false;
                        let mut cancel = false;
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut draft);
                            commit = ui.small_button(icon::CHECK).clicked();
                            cancel = ui.small_button(icon::X).clicked();
                        });
                        if commit {
                            let name = draft.trim().to_string();
                            if !name.is_empty() {
                                crate::known_clients::rename(&self.state, &c.client_id, &name);
                            }
                            self.rename_drafts.remove(&c.client_id);
                        } else if cancel {
                            self.rename_drafts.remove(&c.client_id);
                        } else {
                            self.rename_drafts.insert(c.client_id.clone(), draft);
                        }
                    }
                    ui.label(format!("addr: {}   state: {:?}", c.addr, c.state));
                    if let Some(cue) = &c.current_cue {
                        ui.label(format!("cue: {cue}  @ {}", fmt_ms(c.position_ms)));
                    }
                    ui.label(format!(
                        "offset: {}   drift: {}",
                        c.offset_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                        c.last_drift_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                    ));
                    {
                        let version = if c.app_version.is_empty() { "?" } else { &c.app_version };
                        ui.label(format!("version: {version}"))
                            .on_hover_text(if c.target_triple.is_empty() {
                                "platform unknown (pre-update client)".to_string()
                            } else {
                                c.target_triple.clone()
                            });
                        let available = update_manifest
                            .as_ref()
                            .and_then(|m| update::available_for(m, c).map(|_| m.version.clone()));
                        match (&c.update, available) {
                            (ClientUpdate::Pushing, _) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("sending update…");
                                });
                            }
                            (ClientUpdate::Applying, _) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("restarting into new version…");
                                });
                            }
                            (ClientUpdate::Staged(v), _) => {
                                if ui
                                    .button(format!("{}  Apply v{v} (restart)", icon::CHECK_FAT))
                                    .on_hover_text("Client refuses unless idle")
                                    .clicked()
                                {
                                    update::send_apply(&self.state, &c.client_id);
                                }
                            }
                            (ClientUpdate::Failed(e), available) => {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 70, 70),
                                    format!("{} update failed", icon::WARNING),
                                )
                                .on_hover_text(e);
                                if let Some(v) = available {
                                    if ui.button(format!("{}  Retry update to v{v}", icon::UPLOAD_SIMPLE)).clicked() {
                                        update::push_update_to(&self.state, c.client_id.clone());
                                    }
                                }
                            }
                            (ClientUpdate::None, Some(v)) => {
                                if ui
                                    .button(format!("{}  Update to v{v}", icon::UPLOAD_SIMPLE))
                                    .on_hover_text("Stage the new binary on this client; apply separately")
                                    .clicked()
                                {
                                    update::push_update_to(&self.state, c.client_id.clone());
                                }
                            }
                            (ClientUpdate::None, None) => {}
                        }
                    }
                    if !c.preflight.is_empty() {
                        let ok = c
                            .preflight
                            .values()
                            .filter(|s| **s == MediaFileStatus::Ok)
                            .count();
                        let total = c.preflight.len();
                        ui.label(format!("media: {ok}/{total} ok"));
                        for (path, status) in &c.preflight {
                            if *status != MediaFileStatus::Ok {
                                let what = match status {
                                    MediaFileStatus::Missing => "missing",
                                    MediaFileStatus::Mismatch { .. } => "mismatch",
                                    MediaFileStatus::Ok => unreachable!(),
                                };
                                ui.label(format!("   {} {} ({what})", icon::WARNING, path.display()));
                            }
                        }
                        if let Some((path, received, total)) = &c.push_progress {
                            let frac = if *total > 0 {
                                *received as f32 / *total as f32
                            } else {
                                0.0
                            };
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .text(format!("{} {:.0}%", path.display(), frac * 100.0)),
                            );
                        } else if ok < total && ui.button("Push missing media").clicked() {
                            preflight::push_missing_to(&self.state, c.client_id.clone());
                        }
                    }
                });
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                let go = egui::Button::new(egui::RichText::new("  GO  ").size(24.0).strong());
                if ui.add(go).clicked() {
                    self.go_selected();
                }
                if ui.button(format!("{}  PREV", icon::CARET_UP)).clicked() {
                    self.move_selection(-1);
                }
                if ui.button(format!("{}  NEXT", icon::CARET_DOWN)).clicked() {
                    self.move_selection(1);
                }
                ui.separator();
                if ui.button(format!("{}  PAUSE", icon::PAUSE)).clicked() {
                    broadcast(&self.state, ControllerMsg::Pause);
                }
                if ui.button(format!("{}  RESUME", icon::PLAY)).clicked() {
                    broadcast(&self.state, ControllerMsg::Resume);
                }
                ui.separator();
                if ui.button(format!("{}  BLACKOUT", icon::MOON)).clicked() {
                    self.blackout();
                }
                if ui.button(format!("{}  STOP", icon::STOP)).clicked() {
                    self.stop_all();
                }
            });
            ui.label("space = GO    up / down arrows = select cue");
            ui.separator();
            ui.heading("Log");
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for line in log_tail.iter().rev() {
                    ui.monospace(line);
                }
            });
        });

        self.drive_dialogs(ctx);
    }
}
