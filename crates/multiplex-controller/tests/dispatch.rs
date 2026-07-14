//! Integration tests for per-client cue dispatch: a cue's `target`
//! (all/whitelist/blacklist) and `exclude_action` (ignore/poster/color),
//! plus the known-clients rename-while-offline convergence path. Fake
//! clients speak the wire protocol to a real controller server task — no
//! GUI, no GStreamer, same harness style as `handshake.rs`.

use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMsg;

use multiplex_controller::{dispatch, known_clients, server, state};
use multiplex_shared::protocol::{ClientMsg, ControllerMsg, Envelope, Hello, PROTOCOL_VERSION};
use multiplex_shared::show::{Cue, CueKind, CueTarget, ExcludeAction, DEFAULT_FADE_MS};

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

async fn recv_msg(ws: &mut Ws) -> ControllerMsg {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for controller message")
            .expect("stream ended")
            .expect("ws error");
        if let WsMsg::Text(t) = msg {
            let env: Envelope<ControllerMsg> = serde_json::from_str(&t).expect("bad json");
            return env.msg;
        }
    }
}

/// Assert no message arrives within a short window — used to prove an
/// excluded client with `ExcludeAction::Ignore` gets nothing sent to it.
async fn assert_no_message(ws: &mut Ws) {
    let outcome = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    assert!(outcome.is_err(), "expected no message, but got one");
}

async fn connect_and_hello(addr: &std::net::SocketAddr, client_id: &str, name: &str) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.expect("connect");
    let hello = Envelope::new(
        now_ms(),
        ClientMsg::Hello(Hello {
            client_id: client_id.into(),
            name: name.into(),
            protocol_version: PROTOCOL_VERSION,
            app_version: "0.1.0".into(),
            target_triple: "x86_64-unknown-linux-gnu".into(),
        }),
    );
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap())).await.unwrap();
    ws
}

async fn wait_for_client(state: &state::SharedState, client_id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state.lock().unwrap().clients.contains_key(client_id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("client never appeared in roster");
}

async fn spawn_server() -> (state::SharedState, std::net::SocketAddr) {
    let state = state::shared();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_state = state.clone();
    tokio::spawn(async move {
        let _ = server::serve(server_state, listener).await;
    });
    (state, addr)
}

fn base_cue(id: &str) -> Cue {
    Cue {
        id: id.into(),
        name: id.into(),
        kind: CueKind::Video,
        file: PathBuf::from("a.mp4"),
        ..Default::default()
    }
}

#[tokio::test]
async fn whitelist_cue_dispatches_only_to_targeted_client() {
    let (state, addr) = spawn_server().await;
    let mut ws_a = connect_and_hello(&addr, "client-a", "A").await;
    let mut ws_b = connect_and_hello(&addr, "client-b", "B").await;
    assert!(matches!(recv_msg(&mut ws_a).await, ControllerMsg::HelloAck(_)));
    assert!(matches!(recv_msg(&mut ws_b).await, ControllerMsg::HelloAck(_)));
    wait_for_client(&state, "client-a").await;
    wait_for_client(&state, "client-b").await;

    let cue = Cue {
        target: CueTarget::Whitelist {
            clients: vec!["client-a".into()],
        },
        ..base_cue("c1")
    };
    dispatch::go_for_client(&state, &cue, "client-a", 0);
    dispatch::apply_exclude_action(&state, &cue, &["client-b".to_string()], 0);

    match recv_msg(&mut ws_a).await {
        ControllerMsg::LoadCue(lc) => assert_eq!(lc.cue_id, "c1"),
        other => panic!("expected LOAD_CUE for included client, got {other:?}"),
    }
    match recv_msg(&mut ws_a).await {
        ControllerMsg::PlayAt(_) => {}
        other => panic!("expected PLAY_AT for included client, got {other:?}"),
    }
    // Default exclude_action is Ignore: the excluded client gets nothing.
    assert_no_message(&mut ws_b).await;
}

#[tokio::test]
async fn blacklist_cue_dispatches_to_non_blacklisted_client() {
    let (state, addr) = spawn_server().await;
    let mut ws_a = connect_and_hello(&addr, "client-a", "A").await;
    let mut ws_b = connect_and_hello(&addr, "client-b", "B").await;
    assert!(matches!(recv_msg(&mut ws_a).await, ControllerMsg::HelloAck(_)));
    assert!(matches!(recv_msg(&mut ws_b).await, ControllerMsg::HelloAck(_)));
    wait_for_client(&state, "client-a").await;
    wait_for_client(&state, "client-b").await;

    let cue = Cue {
        target: CueTarget::Blacklist {
            clients: vec!["client-b".into()],
        },
        ..base_cue("c1")
    };
    dispatch::go_for_client(&state, &cue, "client-a", 0);
    dispatch::apply_exclude_action(&state, &cue, &["client-b".to_string()], 0);

    match recv_msg(&mut ws_a).await {
        ControllerMsg::LoadCue(lc) => assert_eq!(lc.cue_id, "c1"),
        other => panic!("expected LOAD_CUE for non-blacklisted client, got {other:?}"),
    }
    assert_no_message(&mut ws_b).await;
}

#[tokio::test]
async fn exclude_action_poster_sends_fade_to_excluded_client() {
    let (state, addr) = spawn_server().await;
    let mut ws_b = connect_and_hello(&addr, "client-b", "B").await;
    assert!(matches!(recv_msg(&mut ws_b).await, ControllerMsg::HelloAck(_)));
    wait_for_client(&state, "client-b").await;

    let cue = Cue {
        exclude_action: ExcludeAction::Poster,
        ..base_cue("c1")
    };
    dispatch::apply_exclude_action(&state, &cue, &["client-b".to_string()], 0);

    match recv_msg(&mut ws_b).await {
        ControllerMsg::Fade(f) => assert_eq!(f.duration_ms, DEFAULT_FADE_MS),
        other => panic!("expected FADE for a Poster-excluded client, got {other:?}"),
    }
}

#[tokio::test]
async fn exclude_action_color_sends_synthetic_color_cue() {
    let (state, addr) = spawn_server().await;
    let mut ws_b = connect_and_hello(&addr, "client-b", "B").await;
    assert!(matches!(recv_msg(&mut ws_b).await, ControllerMsg::HelloAck(_)));
    wait_for_client(&state, "client-b").await;

    let cue = Cue {
        exclude_action: ExcludeAction::Color,
        exclude_color: Some("#112233".into()),
        fade_in_ms: 250,
        ..base_cue("c1")
    };
    dispatch::apply_exclude_action(&state, &cue, &["client-b".to_string()], 0);

    match recv_msg(&mut ws_b).await {
        ControllerMsg::LoadCue(lc) => {
            assert_eq!(lc.cue_id, "c1");
            assert_eq!(lc.kind, CueKind::Color);
            assert_eq!(lc.color.as_deref(), Some("#112233"));
        }
        other => panic!("expected a synthetic colour LOAD_CUE, got {other:?}"),
    }
    match recv_msg(&mut ws_b).await {
        ControllerMsg::PlayAt(_) => {}
        other => panic!("expected PLAY_AT to follow the colour LOAD_CUE, got {other:?}"),
    }
}

#[tokio::test]
async fn rename_while_offline_converges_on_next_hello() {
    let (state, addr) = spawn_server().await;
    let tmp = std::env::temp_dir().join(format!("multiplex_known_clients_rename_test_{}.toml", std::process::id()));
    state.lock().unwrap().known_clients_path = tmp.clone();

    // Rename a client that has never connected.
    known_clients::rename(&state, "client-x", "center-top");
    assert!(state.lock().unwrap().known_clients["client-x"].assigned);

    // It connects later, self-reporting a different name — the mismatch
    // between its HELLO name and the assigned, `assigned=true` entry should
    // push ASSIGN_NAME right after HELLO_ACK.
    let mut ws = connect_and_hello(&addr, "client-x", "old-hostname-name").await;
    assert!(matches!(recv_msg(&mut ws).await, ControllerMsg::HelloAck(_)));
    match recv_msg(&mut ws).await {
        ControllerMsg::AssignName(a) => {
            assert_eq!(a.client_id, "client-x");
            assert_eq!(a.name, "center-top");
        }
        other => panic!("expected ASSIGN_NAME on reconnect after an offline rename, got {other:?}"),
    }

    let _ = std::fs::remove_file(&tmp);
}
