//! MultiPlex client binary.
//!
//! Connects to a controller over WebSocket and drives the local two-layer
//! GStreamer pipeline. Auto-reconnects with exponential backoff; keeps the
//! pipeline running independently of the network task.
//!
//! Env vars:
//!   `MULTIPLEX_CONTROLLER` — controller URL (default `ws://127.0.0.1:9420`)
//!   `MULTIPLEX_NAME`       — human-readable client name (default hostname)
//!   `MULTIPLEX_MEDIA_ROOT` — where this client's media lives
//!                          (default `~/cuemesh_media`)
//!   `MULTIPLEX_CANVAS`     — output canvas as `WxH@FPS`, e.g. `1280x720@30`
//!                          (default 1920x1080@30)
//!   `MULTIPLEX_DRIFT`      — set to `off` to report but never correct drift
//!                          (debugging aid for playback smoothness)
//!
//! Press `F` or `F11` to toggle native OS fullscreen — there's no window
//! chrome to trigger it from otherwise.
//!
//! See `CLAUDE.md` at the workspace root for the design brief.

use multiplex_client::{connection, discovery, state, ui, update};
use multiplex_media::{Canvas, MediaEngine};

/// Parse `WxH@FPS` (e.g. `1280x720@30`); None on any malformed part.
fn parse_canvas(spec: &str) -> Option<Canvas> {
    let (size, fps) = spec.split_once('@')?;
    let (w, h) = size.split_once('x')?;
    Some(Canvas {
        width: w.trim().parse().ok()?,
        height: h.trim().parse().ok()?,
        fps_n: fps.trim().parse().ok()?,
        fps_d: 1,
    })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // A verified update staged by a previous run applies before anything
    // else touches the pipeline or the network; on success this re-execs.
    update::apply_staged_at_startup();

    let controller_url = std::env::var("MULTIPLEX_CONTROLLER")
        .unwrap_or_else(|_| "ws://127.0.0.1:9420".to_string());
    let name = std::env::var("MULTIPLEX_NAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "cuemesh-client".into());
    let client_id = uuid::Uuid::new_v4().to_string();
    let media_root = std::env::var("MULTIPLEX_MEDIA_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("cuemesh_media")
        });

    let engine = match std::env::var("MULTIPLEX_CANVAS").ok().as_deref().map(parse_canvas) {
        Some(Some(canvas)) => {
            tracing::info!(?canvas, "canvas from MULTIPLEX_CANVAS");
            MediaEngine::with_canvas(canvas)?
        }
        Some(None) => anyhow::bail!("MULTIPLEX_CANVAS must look like 1280x720@30"),
        None => MediaEngine::new()?,
    };
    let state = state::shared();
    {
        let mut s = state.lock().unwrap();
        s.client_id = client_id.clone();
        s.name = name.clone();
        s.controller_addr = controller_url.clone();
        s.media_root = media_root.clone();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    discovery::spawn_browser(state.clone());

    let conn_state = state.clone();
    let conn_engine = engine.clone();
    rt.spawn(async move {
        connection::run(
            connection::ConnectionConfig {
                controller_url,
                client_id,
                name,
                media_root,
            },
            conn_state,
            conn_engine,
        )
        .await;
    });

    let ui_state = state.clone();
    let ui_engine = engine.clone();
    let _rt_guard = rt.enter();
    let native_options = eframe::NativeOptions {
        // Chromeless: no OS title bar / min-max-close buttons — the window is
        // just the canvas. Still resizable (drag edges / WM shortcuts).
        viewport: egui::ViewportBuilder::default()
            .with_title("MultiPlex Client")
            .with_decorations(false),
        ..Default::default()
    };
    eframe::run_native(
        "MultiPlex Client",
        native_options,
        Box::new(move |cc| {
            // Repaint exactly when a composited frame lands, so presentation
            // follows the pipeline clock instead of a polling timer.
            let repaint_ctx = cc.egui_ctx.clone();
            ui_engine.set_frame_notify(move || repaint_ctx.request_repaint());
            Ok(Box::new(ui::ClientApp::new(ui_state, ui_engine)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}

