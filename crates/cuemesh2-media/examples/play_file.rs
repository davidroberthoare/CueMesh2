//! Manual smoke test: play a file on layer A for a few seconds.
//!
//! Usage: cargo run -p cuemesh2-media --example play_file -- <path> [seconds]

use std::time::Duration;

use cuemesh2_media::{Layer, MediaEngine};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: play_file <path> [seconds]"))?;
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(6);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = rt.enter();

    let engine = MediaEngine::new()?;
    engine.set_alpha(Layer::A, 1.0);
    engine.load(Layer::A, std::path::Path::new(&path))?;
    println!("prerolled OK, playing for {secs}s…");
    engine.play()?;

    for _ in 0..(secs * 2) {
        std::thread::sleep(Duration::from_millis(500));
        println!("position: {:?} ms", engine.position_ms());
    }
    engine.stop()?;
    Ok(())
}
