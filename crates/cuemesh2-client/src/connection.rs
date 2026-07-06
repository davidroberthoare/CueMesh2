//! WebSocket client with reconnect loop, plus the command dispatcher that
//! turns incoming `ControllerMsg` values into media-engine calls.

use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use cuemesh2_media::{fades, MediaEngine, MediaEvent};
use cuemesh2_shared::protocol::{
    ClientMsg, ControllerMsg, Envelope, Hello, Status, SyncReply, PROTOCOL_VERSION,
};

use crate::state::{PlaybackState, SharedState};

pub struct ConnectionConfig {
    pub controller_url: String,
    pub client_id: String,
    pub name: String,
}

pub async fn run(cfg: ConnectionConfig, state: SharedState, engine: MediaEngine) {
    spawn_media_event_pump(engine.clone(), state.clone());
    let mut backoff_ms = 500u64;
    loop {
        match connect_once(&cfg, &state, &engine).await {
            Ok(_) => {
                log(&state, "connection closed cleanly");
                backoff_ms = 500;
            }
            Err(e) => {
                log(&state, format!("connection error: {e}"));
            }
        }
        state.lock().unwrap().connected = false;
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(10_000);
    }
}

async fn connect_once(
    cfg: &ConnectionConfig,
    state: &SharedState,
    engine: &MediaEngine,
) -> Result<()> {
    log(state, format!("connecting to {}", cfg.controller_url));
    let (ws, _resp) = tokio_tungstenite::connect_async(&cfg.controller_url).await?;
    state.lock().unwrap().connected = true;
    log(state, "connected");

    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<ClientMsg>(64);

    // Send HELLO.
    let hello = Envelope::new(
        now_utc_ms(),
        ClientMsg::Hello(Hello {
            client_id: cfg.client_id.clone(),
            name: cfg.name.clone(),
            protocol_version: PROTOCOL_VERSION,
        }),
    );
    sink.send(WsMsg::Text(serde_json::to_string(&hello)?.into())).await?;

    // Writer task.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let env = Envelope::new(now_utc_ms(), msg);
            let text = match serde_json::to_string(&env) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if sink.send(WsMsg::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Periodic status + heartbeat task.
    let status_state = state.clone();
    let status_engine = engine.clone();
    let status_tx = out_tx.clone();
    let status = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1000));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let pb = {
                let mut s = status_state.lock().unwrap();
                s.playback.position_ms = status_engine.position_ms().unwrap_or(0);
                s.playback.layer_a_alpha = status_engine.alpha(cuemesh2_media::Layer::A) as f32;
                s.playback.layer_b_alpha = status_engine.alpha(cuemesh2_media::Layer::B) as f32;
                s.playback.clone()
            };
            let status_msg = ClientMsg::Status(Status {
                state: pb.state.into(),
                current_cue_id: pb.current_cue_id,
                position_ms: pb.position_ms,
                rate: 1.0,
                volume: 100,
                layer_a_alpha: pb.layer_a_alpha,
                layer_b_alpha: pb.layer_b_alpha,
            });
            if status_tx.send(status_msg).await.is_err() {
                break;
            }
            if status_tx.send(ClientMsg::Heartbeat).await.is_err() {
                break;
            }
        }
    });

    // Reader loop.
    let reader_state = state.clone();
    let reader_engine = engine.clone();
    let reader_out = out_tx.clone();
    let reader = tokio::spawn(async move {
        while let Some(next) = source.next().await {
            let msg = match next {
                Ok(m) => m,
                Err(e) => {
                    log(&reader_state, format!("read error: {e}"));
                    break;
                }
            };
            match msg {
                WsMsg::Text(t) => {
                    let env: Envelope<ControllerMsg> = match serde_json::from_str(&t) {
                        Ok(e) => e,
                        Err(e) => {
                            log(&reader_state, format!("bad json: {e}"));
                            continue;
                        }
                    };
                    handle_controller_msg(env, &reader_state, &reader_engine, &reader_out).await;
                }
                WsMsg::Close(_) => break,
                _ => {}
            }
        }
    });

    // Any task finishing means the connection is done.
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
        _ = status => {}
    }
    Ok(())
}

async fn handle_controller_msg(
    env: Envelope<ControllerMsg>,
    state: &SharedState,
    engine: &MediaEngine,
    outbound: &mpsc::Sender<ClientMsg>,
) {
    use cuemesh2_shared::protocol::Layer as WireLayer;
    let media_layer = |l: WireLayer| match l {
        WireLayer::A => cuemesh2_media::Layer::A,
        WireLayer::B => cuemesh2_media::Layer::B,
    };
    match env.msg {
        ControllerMsg::HelloAck(a) => {
            log(state, format!("controller: {} (v{})", a.controller_name, a.protocol_version));
        }
        ControllerMsg::LoadCue(c) => {
            let ml = media_layer(c.layer);
            let exists = c.file.exists();
            log(
                state,
                format!(
                    "LOAD_CUE {} → layer {:?}  file={}  exists={}  vol={}  fade_in={}ms",
                    c.cue_id,
                    c.layer,
                    c.file.display(),
                    exists,
                    c.volume,
                    c.fade_in_ms
                ),
            );
            engine.set_volume(ml, c.volume);
            engine.set_alpha(ml, if c.fade_in_ms > 0 { 0.0 } else { 1.0 });
            match engine.load(ml, &c.file) {
                Ok(_) => {
                    let mut s = state.lock().unwrap();
                    s.playback.current_cue_id = Some(c.cue_id.clone());
                    s.playback.state = PlaybackState::Ready;
                    drop(s);
                    // Remember fade-in intent for PlayAt.
                    PENDING_FADE_IN.store(c.fade_in_ms, std::sync::atomic::Ordering::SeqCst);
                    let _ = outbound.try_send(ClientMsg::Ready(cuemesh2_shared::protocol::Ready {
                        cue_id: c.cue_id,
                        layer: c.layer,
                    }));
                }
                Err(e) => {
                    log(state, format!("load failed: {e}"));
                    state.lock().unwrap().playback.state = PlaybackState::Error;
                }
            }
        }
        ControllerMsg::PlayAt(p) => {
            let ml = media_layer(p.layer);
            let now = now_utc_ms();
            let delay = p.master_start_utc_ms.saturating_sub(now);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            match engine.play() {
                Ok(_) => {
                    state.lock().unwrap().playback.state = PlaybackState::Playing;
                    let fade_in = PENDING_FADE_IN.swap(0, std::sync::atomic::Ordering::SeqCst);
                    if fade_in > 0 {
                        fades::fade(engine, ml, 1.0, Duration::from_millis(fade_in as u64));
                    } else {
                        engine.set_alpha(ml, 1.0);
                    }
                }
                Err(e) => log(state, format!("play failed: {e}")),
            }
        }
        ControllerMsg::SeekTo(s) => {
            if let Err(e) = engine.seek_ms(s.position_ms) {
                log(state, format!("seek failed: {e}"));
            }
        }
        ControllerMsg::SetRate(r) => {
            if let Err(e) = engine.set_rate(r.rate as f64) {
                log(state, format!("set_rate failed: {e}"));
            }
        }
        ControllerMsg::SetVolume(v) => engine.set_volume(media_layer(v.layer), v.volume),
        ControllerMsg::Pause => {
            if let Err(e) = engine.pause() {
                log(state, format!("pause failed: {e}"));
            } else {
                state.lock().unwrap().playback.state = PlaybackState::Paused;
            }
        }
        ControllerMsg::Fade(cmd) => {
            let dur = Duration::from_millis(cmd.duration_ms as u64);
            fades::fade(engine, cuemesh2_media::Layer::A, 0.0, dur);
            fades::fade(engine, cuemesh2_media::Layer::B, 0.0, dur);
            let engine_clone = engine.clone();
            let state_clone = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(dur).await;
                if let Err(e) = engine_clone.stop() {
                    log(&state_clone, format!("post-fade stop failed: {e}"));
                }
                state_clone.lock().unwrap().playback.state = PlaybackState::Black;
            });
        }
        ControllerMsg::Stop => {
            if let Err(e) = engine.stop() {
                log(state, format!("stop failed: {e}"));
            }
            let mut s = state.lock().unwrap();
            s.playback.state = PlaybackState::Black;
            s.playback.current_cue_id = None;
        }
        ControllerMsg::Crossfade(cf) => {
            // For MVP: just log — full manual crossfade requires the client to know
            // where the target cue lives. Wire this up when we add show-file awareness.
            log(state, format!("(unimplemented) manual crossfade to {} in {}ms", cf.to_cue_id, cf.duration_ms));
        }
        ControllerMsg::ShowTestscreen => {
            log(state, "(unimplemented) show testscreen");
        }
        ControllerMsg::RequestStatus | ControllerMsg::ReadyCheck => {
            // Status is sent on our own cadence.
        }
        ControllerMsg::Sync(ping) => {
            let t2 = now_utc_ms();
            let t3 = now_utc_ms();
            let _ = outbound.try_send(ClientMsg::SyncReply(SyncReply {
                token: ping.token,
                t1_utc_ms: ping.t1_utc_ms,
                t2_local_ms: t2,
                t3_local_ms: t3,
            }));
        }
    }
}

/// Cheap single-slot for the fade-in duration that a `LOAD_CUE` set aside for
/// the following `PLAY_AT`. Correct for the MVP one-cue-at-a-time flow.
static PENDING_FADE_IN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Forward pipeline events (errors, EOS, state changes) into the UI log.
fn spawn_media_event_pump(engine: MediaEngine, state: SharedState) {
    let mut rx = engine.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(MediaEvent::Eos(layer)) => log(&state, format!("engine: EOS on layer {layer:?}")),
                Ok(MediaEvent::Error { source, message }) => {
                    log(&state, format!("engine ERROR [{source}]: {message}"))
                }
                Ok(MediaEvent::State(s)) => log(&state, format!("engine state → {s:?}")),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log(&state, format!("engine event stream lagged, dropped {n}"))
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn log(state: &SharedState, line: impl Into<String>) {
    let line = line.into();
    tracing::info!("{line}");
    state.lock().unwrap().push_log(line);
}

fn now_utc_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
