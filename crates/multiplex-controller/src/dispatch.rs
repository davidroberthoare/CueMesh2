//! Per-client cue dispatch.
//!
//! Every cue used to reach every client identically via `server::broadcast`.
//! Now a cue can target only some clients (`Cue::target`), so GO/STANDBY
//! dispatch has to reason about each client individually — including its own
//! two-layer (A/B) crossfade state, which used to live once, globally, on
//! `RunState` because every client always advanced it in lockstep. A client
//! a cue excludes doesn't advance its layer toggle for that GO, so its state
//! can (and will) diverge from the rest of the fleet; that's why the layer
//! bookkeeping this module reads and writes lives on `ClientRow` instead.

use multiplex_shared::protocol::{ControllerMsg, FadeCmd, Layer, PlayAt};
use multiplex_shared::show::{Cue, CueKind, ExcludeAction, DEFAULT_FADE_MS};

use crate::server::{client_queue, load_cue_for, log, now_utc_ms, send_to};
use crate::state::{Outgoing, SharedState};

/// The layer a client's next GO (and thus its next STANDBY) will target: the
/// opposite of whatever's currently on air for it, or A if nothing is.
pub fn idle_layer(active: Option<Layer>) -> Layer {
    match active {
        Some(Layer::A) => Layer::B,
        Some(Layer::B) => Layer::A,
        None => Layer::A,
    }
}

fn send_one(state: &SharedState, client_id: &str, msg: ControllerMsg) {
    if let Some(q) = client_queue(state, client_id) {
        let _ = q.try_send(Outgoing::Msg(msg));
    }
}

/// Fire `cue` for one included client: the per-client body of what a
/// fleet-wide GO used to do for everyone at once. Crossfade timing and
/// STANDBY reuse are decided from this client's own `ClientRow`, not a
/// shared `RunState`.
pub fn go_for_client(state: &SharedState, cue: &Cue, client_id: &str, lead_ms: u64) {
    let plan = {
        let mut s = state.lock().unwrap();
        let Some(row) = s.clients.get_mut(client_id) else {
            return;
        };
        let on_air = row.now_playing.is_some();
        let target_layer = idle_layer(row.active_layer);
        // Crossfade when something is on air for this client; the incoming
        // cue's fade-in doubles as the crossfade duration, floored so it's
        // never a jarring instant swap.
        let crossfade_ms = on_air.then(|| cue.fade_in_ms.max(40));
        let preloaded = row.standby.as_ref() == Some(&(cue.id.clone(), target_layer));

        row.active_layer = Some(target_layer);
        row.standby = None;
        row.now_playing = Some(cue.id.clone());
        row.idle_free_utc_ms = match crossfade_ms {
            Some(ms) => now_utc_ms() + lead_ms + ms as u64 + 300,
            None => 0,
        };
        (target_layer, crossfade_ms, preloaded)
    };
    let (layer, crossfade_ms, preloaded) = plan;

    log(
        state,
        format!(
            "GO cue {} → {client_id} on layer {layer:?}{}",
            cue.id,
            if preloaded { " (preloaded)" } else { " (cold load)" }
        ),
    );
    if !preloaded {
        send_one(state, client_id, ControllerMsg::LoadCue(load_cue_for(cue, layer)));
    }
    send_one(
        state,
        client_id,
        ControllerMsg::PlayAt(PlayAt {
            layer,
            master_start_utc_ms: now_utc_ms() + lead_ms,
            fade_in_ms: cue.fade_in_ms,
            crossfade_ms,
        }),
    );
}

/// Preload `cue` onto the layer this client's next GO will use. Per-client
/// analogue of the old fleet-wide speculative STANDBY; the caller (debounce,
/// "has the selection settled") is unchanged.
pub fn standby_for_client(state: &SharedState, cue: &Cue, client_id: &str) {
    let target = {
        let mut s = state.lock().unwrap();
        let Some(row) = s.clients.get_mut(client_id) else {
            return;
        };
        let target = idle_layer(row.active_layer);
        // Already pre-loaded on the right layer, or that layer is still
        // finishing a crossfade-out.
        if row.standby.as_ref() == Some(&(cue.id.clone(), target)) || now_utc_ms() < row.idle_free_utc_ms {
            return;
        }
        row.standby = Some((cue.id.clone(), target));
        target
    };
    log(state, format!("STANDBY cue {} → {client_id} on layer {target:?}", cue.id));
    send_one(state, client_id, ControllerMsg::Standby(load_cue_for(cue, target)));
}

/// Apply `cue`'s `exclude_action` to every client it doesn't target.
pub fn apply_exclude_action(state: &SharedState, cue: &Cue, excluded_ids: &[String], lead_ms: u64) {
    match cue.exclude_action {
        ExcludeAction::Ignore => {
            // Simply don't send this cue — the excluded client keeps
            // whatever it already had on air.
        }
        ExcludeAction::Poster => {
            send_to(
                state,
                excluded_ids,
                ControllerMsg::Fade(FadeCmd {
                    duration_ms: DEFAULT_FADE_MS,
                }),
            );
            // The client's own FADE handling fully resets both its layers,
            // so nothing is "on air" there anymore — clear this client's
            // bookkeeping to match, and drop any stale STANDBY (its content
            // was just wiped, so treating it as still-preloaded would skip
            // a LOAD_CUE the client actually needs on a future re-inclusion).
            let mut s = state.lock().unwrap();
            for id in excluded_ids {
                if let Some(row) = s.clients.get_mut(id) {
                    row.active_layer = None;
                    row.standby = None;
                    row.idle_free_utc_ms = 0;
                    row.now_playing = None;
                }
            }
        }
        ExcludeAction::Color => {
            // A synthetic colour cue, reusing the same LOAD_CUE/PLAY_AT path
            // (and per-client layer bookkeeping) as a normal cue — no new
            // protocol message needed. Keeps the real cue's id so the roster
            // reports a sensible "current cue" for excluded clients too.
            let color_cue = Cue {
                id: cue.id.clone(),
                kind: CueKind::Color,
                color: Some(cue.exclude_color.clone().unwrap_or_else(|| "#000000".into())),
                fade_in_ms: cue.fade_in_ms,
                ..Default::default()
            };
            for id in excluded_ids {
                go_for_client(state, &color_cue, id, lead_ms);
            }
        }
    }
}
