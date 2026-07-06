//! WebSocket hub. One tokio task per connected client; a broadcast helper
//! iterates the roster and enqueues messages to each.

use std::net::SocketAddr;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use cuemesh2_shared::protocol::{
    ClientMsg, ClientState, ControllerMsg, Envelope, HelloAck, PROTOCOL_VERSION,
};

use crate::state::{ClientRow, SharedState};

const OUTBOUND_QUEUE: usize = 128;

/// Bind and accept WebSocket clients forever.
pub async fn run(state: SharedState, bind: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    log(&state, format!("listening on {bind}"));
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, addr, state.clone()).await {
                        log(&state, format!("client {addr}: {e}"));
                    }
                });
            }
            Err(e) => {
                log(&state, format!("accept error: {e}"));
            }
        }
    }
}

async fn handle_conn(stream: TcpStream, addr: SocketAddr, state: SharedState) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();

    // Wait for HELLO.
    let hello = loop {
        match source.next().await {
            Some(Ok(WsMsg::Text(t))) => {
                let env: Envelope<ClientMsg> = serde_json::from_str(&t)?;
                if let ClientMsg::Hello(h) = env.msg {
                    break h;
                }
            }
            Some(Ok(WsMsg::Ping(p))) => {
                sink.send(WsMsg::Pong(p)).await?;
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(()),
        }
    };

    // Blacklist check.
    {
        let s = state.lock().unwrap();
        if s.blacklist.iter().any(|id| id == &hello.client_id) {
            log(&state, format!("rejecting blacklisted client {}", hello.client_id));
            return Ok(());
        }
    }

    // Register the client.
    let (out_tx, mut out_rx) = mpsc::channel::<ControllerMsg>(OUTBOUND_QUEUE);
    let now_ms = now_utc_ms();
    let client_id = hello.client_id.clone();
    {
        let mut s = state.lock().unwrap();
        s.clients.insert(
            client_id.clone(),
            ClientRow {
                client_id: client_id.clone(),
                name: hello.name.clone(),
                addr: addr.to_string(),
                state: ClientState::Idle,
                current_cue: None,
                position_ms: 0,
                last_drift_ms: None,
                last_heartbeat_ms: now_ms,
                outbound: out_tx.clone(),
            },
        );
        s.push_log(format!("client {} ({}) joined from {addr}", hello.name, client_id));
    }

    // Send HELLO_ACK.
    let ack = Envelope::new(
        now_utc_ms(),
        ControllerMsg::HelloAck(HelloAck {
            controller_name: "cuemesh2-controller".into(),
            protocol_version: PROTOCOL_VERSION,
        }),
    );
    sink.send(WsMsg::Text(serde_json::to_string(&ack)?.into())).await?;

    // Split loops: read → state, write ← channel.
    let state_reader = state.clone();
    let client_id_reader = client_id.clone();
    let reader = tokio::spawn(async move {
        while let Some(next) = source.next().await {
            let msg = match next {
                Ok(m) => m,
                Err(e) => {
                    log(&state_reader, format!("read error {client_id_reader}: {e}"));
                    break;
                }
            };
            match msg {
                WsMsg::Text(t) => {
                    let env: Envelope<ClientMsg> = match serde_json::from_str(&t) {
                        Ok(e) => e,
                        Err(e) => {
                            log(&state_reader, format!("bad json from {client_id_reader}: {e}"));
                            continue;
                        }
                    };
                    handle_client_msg(&state_reader, &client_id_reader, env);
                }
                WsMsg::Close(_) => break,
                _ => {}
            }
        }
    });

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

    let _ = tokio::join!(reader, writer);

    // Deregister.
    {
        let mut s = state.lock().unwrap();
        s.clients.remove(&client_id);
        s.push_log(format!("client {client_id} left"));
    }
    Ok(())
}

fn handle_client_msg(state: &SharedState, client_id: &str, env: Envelope<ClientMsg>) {
    match env.msg {
        ClientMsg::Status(s) => {
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.state = s.state;
                row.current_cue = s.current_cue_id;
                row.position_ms = s.position_ms;
            }
        }
        ClientMsg::Drift(d) => {
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.last_drift_ms = Some(d.drift_ms);
            }
        }
        ClientMsg::Heartbeat => {
            let now = now_utc_ms();
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.last_heartbeat_ms = now;
            }
        }
        ClientMsg::SyncReply(reply) => {
            let t4 = now_utc_ms();
            let offset = cuemesh2_shared::clock_sync::compute_offset(
                reply.t1_utc_ms,
                reply.t2_local_ms,
                reply.t3_local_ms,
                t4,
            );
            let mut st = state.lock().unwrap();
            if let Some(row) = st.clients.get_mut(client_id) {
                row.last_drift_ms = Some(offset);
            }
        }
        ClientMsg::Log(l) => {
            state.lock().unwrap().push_log(format!(
                "[{}][{:?}] {}: {}",
                client_id, l.level, l.source, l.message
            ));
        }
        ClientMsg::Hello(_) | ClientMsg::Ready(_) => {
            // Ignored after initial handshake / not-yet-implemented.
        }
    }
}

/// Enqueue a message to every connected client.
pub fn broadcast(state: &SharedState, msg: ControllerMsg) {
    let queues: Vec<_> = {
        let s = state.lock().unwrap();
        s.clients.values().map(|c| c.outbound.clone()).collect()
    };
    for q in queues {
        let _ = q.try_send(msg.clone());
    }
}

pub fn log(state: &SharedState, line: impl Into<String>) {
    let line = line.into();
    tracing::info!("{line}");
    state.lock().unwrap().push_log(line);
}

pub fn now_utc_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
