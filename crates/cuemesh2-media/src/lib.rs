//! Two-layer GStreamer video pipeline for CueMesh2 clients.
//!
//! Two `uridecodebin` sources feed a `compositor` whose output is rendered to
//! a fullscreen video sink chosen at runtime. Layer alphas are set on the
//! compositor's sink pads, so no separate `alpha` element is needed.
//!
//! See `CLAUDE.md` at the workspace root for the design brief.

pub mod fades;
pub mod pipeline;

pub use pipeline::{MediaEngine, MediaError, MediaEvent};

pub use cuemesh2_shared::protocol::Layer;
