//! Two-layer video pipeline. Audio is intentionally unsupported: decoded
//! audio streams are discarded into a `fakesink` so files with audio tracks
//! still play, but nothing reaches an audio device.
//!
//! Topology:
//!
//! ```text
//! uridecodebin_a ─► videoconvert ─► videoscale ─┐
//!                                                ├─► compositor ─► capsfilter(I420) ─► videoconvert ─► video sink
//! uridecodebin_b ─► videoconvert ─► videoscale ─┘
//! ```
//!
//! Each `uridecodebin` emits pads at PREROLL; a `pad-added` handler links
//! video pads into the layer chain and audio pads into throwaway fakesinks.
//! Compositor sink pads carry the per-layer `alpha` we drive from Rust.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gstreamer as gst;
use gstreamer::prelude::*;
use tokio::sync::broadcast;

use cuemesh2_shared::protocol::Layer;

/// Errors returned by the media engine.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("gstreamer init failed: {0}")]
    Init(#[from] gst::glib::Error),
    #[error("gstreamer element creation failed: {0}")]
    ElementFactory(String),
    #[error("gstreamer link failed: {0}")]
    Link(#[from] gst::PadLinkError),
    #[error("gstreamer element link failed: {0}")]
    LinkElements(String),
    #[error("gstreamer state change failed: {0}")]
    StateChange(String),
    #[error("gstreamer bus message: {0}")]
    Bus(String),
    #[error("invalid file path: {0}")]
    BadPath(String),
    #[error("gstreamer add-many failed: {0}")]
    AddMany(String),
}

/// Events published on the engine's broadcast channel.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// A layer reached end-of-stream.
    Eos(Layer),
    /// A GStreamer error occurred (usually fatal for one pipeline run).
    Error { source: String, message: String },
    /// State changed on the overall pipeline.
    State(gst::State),
}

fn other(layer: Layer) -> Layer {
    match layer {
        Layer::A => Layer::B,
        Layer::B => Layer::A,
    }
}

fn make(factory: &str, name: Option<&str>) -> Result<gst::Element, MediaError> {
    let mut b = gst::ElementFactory::make(factory);
    if let Some(n) = name {
        b = b.name(n);
    }
    b.build()
        .map_err(|_| MediaError::ElementFactory(factory.to_string()))
}

static GST_INIT: OnceLock<()> = OnceLock::new();

fn ensure_init() -> Result<(), MediaError> {
    if GST_INIT.get().is_some() {
        return Ok(());
    }
    gst::init()?;
    let _ = GST_INIT.set(());
    Ok(())
}

/// Per-layer state that the engine actively touches after construction.
/// The `uridecodebin`, converters, mixer pads, etc. are owned by the
/// pipeline via refcount — no need to hold Rust references to them here.
struct LayerParts {
    uridecodebin: gst::Element,
    compositor_pad: gst::Pad,
    /// Head of the video chain (`videoconvert`). Kept so we can push EOS into
    /// the chain while the layer has no media — see [`MediaEngine::load`].
    video_convert: gst::Element,
    /// True once a URI has been loaded on this layer.
    has_media: AtomicBool,
    /// Handle to the currently running fade task, if any. Aborted on new fade.
    fade: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct Inner {
    pipeline: gst::Pipeline,
    layer_a: LayerParts,
    layer_b: LayerParts,
    events_tx: broadcast::Sender<MediaEvent>,
}

/// Two-layer video+audio pipeline. Clone is cheap (Arc-shared).
#[derive(Clone)]
pub struct MediaEngine {
    inner: Arc<Inner>,
}

impl MediaEngine {
    /// Build a fresh pipeline. Does not start it.
    pub fn new() -> Result<Self, MediaError> {
        ensure_init()?;

        let pipeline = gst::Pipeline::with_name("cuemesh2-pipeline");

        // Compositor + video sink chain.
        let compositor = make("compositor", Some("comp"))?;
        compositor.set_property_from_str("background", "black");
        // Pin the blending format. Left to negotiate freely, compositor can
        // settle on A444_16LE (16-bit 4:4:4 + alpha) and software-convert
        // every frame, which drops the frame rate to a crawl. I420 blends
        // cheaply and every sink path accepts it.
        let comp_caps = make("capsfilter", Some("comp_caps"))?;
        comp_caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .build(),
        );
        let video_out_convert = make("videoconvert", Some("vout_convert"))?;
        let video_sink = Self::make_video_sink()?;

        pipeline
            .add_many([&compositor, &comp_caps, &video_out_convert, &video_sink])
            .map_err(|e| MediaError::AddMany(e.to_string()))?;
        gst::Element::link_many([&compositor, &comp_caps, &video_out_convert, &video_sink])
            .map_err(|e| MediaError::LinkElements(e.to_string()))?;

        let layer_a = Self::build_layer(&pipeline, &compositor, Layer::A, 0)?;
        let layer_b = Self::build_layer(&pipeline, &compositor, Layer::B, 1)?;

        // Default: A visible at full alpha, B hidden.
        layer_a.compositor_pad.set_property("alpha", 1.0f64);
        layer_b.compositor_pad.set_property("alpha", 0.0f64);

        let (events_tx, _rx) = broadcast::channel(64);
        let engine = MediaEngine {
            inner: Arc::new(Inner {
                pipeline,
                layer_a,
                layer_b,
                events_tx,
            }),
        };

        engine.spawn_bus_watch();
        Ok(engine)
    }

    /// Pick the video sink. `glimagesink` scales/converts on the GPU;
    /// `autovideosink` (which usually resolves to `xvimagesink` on X11) is
    /// the fallback where GL isn't available.
    fn make_video_sink() -> Result<gst::Element, MediaError> {
        for factory in ["glimagesink", "autovideosink"] {
            if let Ok(sink) = gst::ElementFactory::make(factory).name("vsink").build() {
                tracing::info!(%factory, "video sink selected");
                return Ok(sink);
            }
        }
        Err(MediaError::ElementFactory("no usable video sink".into()))
    }

    fn build_layer(
        pipeline: &gst::Pipeline,
        compositor: &gst::Element,
        layer: Layer,
        zorder: u32,
    ) -> Result<LayerParts, MediaError> {
        let suffix = match layer {
            Layer::A => "a",
            Layer::B => "b",
        };
        let uridecodebin = make("uridecodebin", Some(&format!("src_{suffix}")))?;
        let video_convert = make("videoconvert", Some(&format!("vconv_{suffix}")))?;
        let video_scale = make("videoscale", Some(&format!("vscale_{suffix}")))?;

        pipeline
            .add_many([&uridecodebin, &video_convert, &video_scale])
            .map_err(|e| MediaError::AddMany(e.to_string()))?;

        gst::Element::link_many([&video_convert, &video_scale])
            .map_err(|e| MediaError::LinkElements(e.to_string()))?;

        // Request a compositor sink pad and link the chain tail to it.
        let compositor_pad = compositor
            .request_pad_simple("sink_%u")
            .ok_or_else(|| MediaError::LinkElements("compositor sink pad request failed".into()))?;
        compositor_pad.set_property("zorder", zorder);
        compositor_pad.set_property("alpha", 0.0f64);

        let video_scale_src = video_scale
            .static_pad("src")
            .ok_or_else(|| MediaError::LinkElements("videoscale src pad missing".into()))?;
        video_scale_src.link(&compositor_pad)?;

        // Route uridecodebin's dynamic pads: video into the layer chain,
        // everything else (audio) into a throwaway fakesink — leaving a
        // decoder pad unlinked would error the pipeline.
        let video_convert_weak = video_convert.downgrade();
        let pipeline_weak = pipeline.downgrade();
        uridecodebin.connect_pad_added(move |_src, pad| {
            let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
            if caps.is_empty() {
                return;
            }
            let structure = match caps.structure(0) {
                Some(s) => s,
                None => return,
            };
            if structure.name().starts_with("video/") {
                if let Some(vc) = video_convert_weak.upgrade() {
                    if let Some(sink) = vc.static_pad("sink") {
                        if !sink.is_linked() {
                            if let Err(e) = pad.link(&sink) {
                                tracing::warn!(?e, "failed to link video pad");
                            }
                            return;
                        }
                    }
                }
            }
            // Non-video stream, or a second video stream we don't want:
            // drain it silently so the demuxer doesn't hit not-linked.
            if let Some(pl) = pipeline_weak.upgrade() {
                let Ok(fakesink) = gst::ElementFactory::make("fakesink")
                    .property("sync", false)
                    .property("async", false)
                    .build()
                else {
                    return;
                };
                if pl.add(&fakesink).is_ok() {
                    let _ = fakesink.sync_state_with_parent();
                    if let Some(sink) = fakesink.static_pad("sink") {
                        if let Err(e) = pad.link(&sink) {
                            tracing::warn!(?e, "failed to link discard sink");
                        }
                    }
                }
            }
        });

        // compositor is an aggregator: it waits for a buffer or EOS on EVERY
        // sink pad before emitting anything. A file with no video track would
        // leave this chain starved and stall the whole pipeline, so once the
        // decoder has revealed all its pads, push EOS if we're still unlinked.
        let vconv_weak = video_convert.downgrade();
        uridecodebin.connect_no_more_pads(move |_| {
            if let Some(vc) = vconv_weak.upgrade() {
                if let Some(sink) = vc.static_pad("sink") {
                    if !sink.is_linked() {
                        tracing::debug!(element = %vc.name(), "no video stream; sending EOS");
                        let _ = sink.send_event(gst::event::Eos::new());
                    }
                }
            }
        });

        // Lock this uridecodebin so it stays in NULL until we actually load a
        // URI. Otherwise pipeline-wide state changes try to preroll an unset
        // source and fail with "No URI specified to play from".
        uridecodebin.set_locked_state(true);
        let _ = uridecodebin.set_state(gst::State::Null);

        Ok(LayerParts {
            uridecodebin,
            compositor_pad,
            video_convert,
            has_media: AtomicBool::new(false),
            fade: Mutex::new(None),
        })
    }

    fn layer(&self, layer: Layer) -> &LayerParts {
        match layer {
            Layer::A => &self.inner.layer_a,
            Layer::B => &self.inner.layer_b,
        }
    }

    fn spawn_bus_watch(&self) {
        let bus = match self.inner.pipeline.bus() {
            Some(b) => b,
            None => return,
        };
        let tx = self.inner.events_tx.clone();
        let pipeline = self.inner.pipeline.clone();
        // Use the glib main-context-free variant so we don't need a running loop.
        std::thread::Builder::new()
            .name("cuemesh2-media-bus".into())
            .spawn(move || {
                for msg in bus.iter_timed(gst::ClockTime::NONE) {
                    use gst::MessageView as M;
                    match msg.view() {
                        M::Eos(_) => {
                            tracing::info!("bus: EOS");
                            let _ = tx.send(MediaEvent::Eos(Layer::A));
                        }
                        M::Error(err) => {
                            let src = err
                                .src()
                                .map(|s| s.path_string().to_string())
                                .unwrap_or_else(|| "unknown".into());
                            let dbg = err.debug().map(|d| d.to_string()).unwrap_or_default();
                            tracing::error!(
                                source = %src,
                                error = %err.error(),
                                debug = %dbg,
                                "bus: ERROR"
                            );
                            let _ = tx.send(MediaEvent::Error {
                                source: src,
                                message: format!("{} — {}", err.error(), dbg),
                            });
                        }
                        M::Warning(w) => {
                            let src = w
                                .src()
                                .map(|s| s.path_string().to_string())
                                .unwrap_or_else(|| "unknown".into());
                            let dbg = w.debug().map(|d| d.to_string()).unwrap_or_default();
                            tracing::warn!(source = %src, warning = %w.error(), debug = %dbg, "bus: WARNING");
                        }
                        M::Info(i) => {
                            let src = i
                                .src()
                                .map(|s| s.path_string().to_string())
                                .unwrap_or_else(|| "unknown".into());
                            tracing::info!(source = %src, info = %i.error(), "bus: INFO");
                        }
                        M::StateChanged(sc) => {
                            if sc
                                .src()
                                .map(|s| s == pipeline.upcast_ref::<gst::Object>())
                                .unwrap_or(false)
                            {
                                tracing::debug!(
                                    old = ?sc.old(),
                                    new = ?sc.current(),
                                    pending = ?sc.pending(),
                                    "bus: pipeline state changed"
                                );
                                let _ = tx.send(MediaEvent::State(sc.current()));
                            }
                        }
                        M::StreamStatus(s) => {
                            tracing::trace!(kind = ?s.type_(), "bus: stream status");
                        }
                        M::AsyncDone(_) => {
                            tracing::debug!("bus: async done");
                        }
                        M::Buffering(b) => {
                            tracing::debug!(percent = b.percent(), "bus: buffering");
                        }
                        other => {
                            tracing::trace!(?other, "bus: other");
                        }
                    }
                }
            })
            .expect("spawn bus watch thread");
    }

    /// Subscribe to engine events (EOS, error, state).
    pub fn subscribe(&self) -> broadcast::Receiver<MediaEvent> {
        self.inner.events_tx.subscribe()
    }

    /// Point a layer at a media file. Pipeline is set to PAUSED to preroll.
    pub fn load(&self, layer: Layer, path: &Path) -> Result<(), MediaError> {
        if !path.exists() {
            tracing::error!(path = %path.display(), ?layer, "load: file does not exist");
            return Err(MediaError::BadPath(format!(
                "file not found: {}",
                path.display()
            )));
        }
        let abs = path
            .canonicalize()
            .map_err(|e| MediaError::BadPath(format!("{}: {e}", path.display())))?;
        let uri = gst::glib::filename_to_uri(&abs, None)
            .map_err(|e| MediaError::BadPath(e.to_string()))?;
        tracing::info!(?layer, %uri, "load: setting uridecodebin uri");
        let parts = self.layer(layer);
        parts.uridecodebin.set_property("uri", uri.as_str());
        parts.has_media.store(true, Ordering::SeqCst);
        // Release the initial NULL lock and let this layer follow the pipeline
        // state. sync_state_with_parent brings the layer up to whatever the
        // pipeline is currently at (typically READY or PAUSED).
        parts.uridecodebin.set_locked_state(false);
        if let Err(e) = parts.uridecodebin.sync_state_with_parent() {
            tracing::warn!(?e, ?layer, "sync_state_with_parent failed");
        }
        match self.inner.pipeline.set_state(gst::State::Paused) {
            Ok(sc) => {
                tracing::debug!(result = ?sc, "load: set_state(Paused) accepted");
                // The other layer's chains are linked into compositor/audiomixer
                // even when it has no media, and those aggregators wait for a
                // buffer or EOS on every sink pad before emitting anything.
                // Mark an idle layer's chains EOS so preroll can complete.
                // (Pads only accept events once activated, hence after the
                // Paused transition starts, not at construction time.)
                self.mark_layer_eos_if_idle(other(layer));
                // Wait briefly for preroll so we surface async failures inline.
                let (result, current, pending) =
                    self.inner.pipeline.state(gst::ClockTime::from_seconds(3));
                tracing::info!(
                    ?result,
                    ?current,
                    ?pending,
                    "load: preroll wait finished"
                );
                if result.is_err() {
                    return Err(MediaError::StateChange(format!(
                        "preroll failed (state={current:?}, pending={pending:?}) — see bus errors above"
                    )));
                }
                Ok(())
            }
            Err(e) => Err(MediaError::StateChange(format!(
                "set_state(Paused) rejected: {e} — see bus errors above"
            ))),
        }
    }

    /// Set the whole pipeline to PLAYING.
    pub fn play(&self) -> Result<(), MediaError> {
        tracing::info!("play: set_state(Playing)");
        match self.inner.pipeline.set_state(gst::State::Playing) {
            Ok(sc) => {
                tracing::debug!(result = ?sc, "play: accepted");
                Ok(())
            }
            Err(e) => Err(MediaError::StateChange(format!(
                "set_state(Playing) rejected: {e} — see bus errors above"
            ))),
        }
    }

    /// Freeze all playback in place.
    pub fn pause(&self) -> Result<(), MediaError> {
        self.inner
            .pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| MediaError::StateChange(e.to_string()))?;
        Ok(())
    }

    /// Cut everything to black and stop the pipeline.
    pub fn stop(&self) -> Result<(), MediaError> {
        self.abort_fade(Layer::A);
        self.abort_fade(Layer::B);
        self.inner.layer_a.compositor_pad.set_property("alpha", 0.0f64);
        self.inner.layer_b.compositor_pad.set_property("alpha", 0.0f64);
        self.inner
            .pipeline
            .set_state(gst::State::Ready)
            .map_err(|e| MediaError::StateChange(e.to_string()))?;
        Ok(())
    }

    /// Set a compositor sink pad's alpha directly (no ramp).
    pub fn set_alpha(&self, layer: Layer, alpha: f64) {
        self.abort_fade(layer);
        self.layer(layer)
            .compositor_pad
            .set_property("alpha", alpha.clamp(0.0, 1.0));
    }

    /// Read the current compositor alpha for a layer.
    pub fn alpha(&self, layer: Layer) -> f64 {
        self.layer(layer).compositor_pad.property::<f64>("alpha")
    }

    /// No-op: CueMesh2 is video-only; audio streams are decoded to a fakesink.
    /// Kept so the protocol surface (`SET_VOLUME`, per-cue volume) stays valid.
    pub fn set_volume(&self, _layer: Layer, _volume: u8) {}

    /// Rate is applied via a pipeline seek. Called sparingly during drift correction.
    pub fn set_rate(&self, rate: f64) -> Result<(), MediaError> {
        let pos = self
            .inner
            .pipeline
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);
        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
        if rate >= 0.0 {
            self.inner
                .pipeline
                .seek(rate, flags, gst::SeekType::Set, pos, gst::SeekType::End, gst::ClockTime::ZERO)
                .map_err(|e| MediaError::StateChange(e.to_string()))?;
        }
        Ok(())
    }

    /// Seek the pipeline to a position in ms (both layers).
    pub fn seek_ms(&self, position_ms: u64) -> Result<(), MediaError> {
        let pos = gst::ClockTime::from_mseconds(position_ms);
        self.inner
            .pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, pos)
            .map_err(|e| MediaError::StateChange(e.to_string()))?;
        Ok(())
    }

    /// Current playback position in ms, or None if not queryable yet.
    pub fn position_ms(&self) -> Option<u64> {
        self.inner
            .pipeline
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds())
    }

    /// Push EOS into a layer's video chain head if the layer has never been
    /// given media. The compositor treats an EOS pad as "don't wait", which
    /// is what lets a single-layer show preroll at all. A later `load` on the
    /// layer clears the EOS state via the decoder's STREAM_START.
    fn mark_layer_eos_if_idle(&self, layer: Layer) {
        let parts = self.layer(layer);
        if parts.has_media.load(Ordering::SeqCst) {
            return;
        }
        if let Some(sink) = parts.video_convert.static_pad("sink") {
            if !sink.is_linked() {
                tracing::debug!(?layer, "idle layer: sending EOS");
                let _ = sink.send_event(gst::event::Eos::new());
            }
        }
    }

    fn abort_fade(&self, layer: Layer) {
        let parts = self.layer(layer);
        if let Ok(mut guard) = parts.fade.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }

    /// Replace this layer's active fade task with a new one.
    pub(crate) fn install_fade(&self, layer: Layer, handle: tokio::task::JoinHandle<()>) {
        let parts = self.layer(layer);
        if let Ok(mut guard) = parts.fade.lock() {
            if let Some(prev) = guard.replace(handle) {
                prev.abort();
            }
        }
    }

    /// Direct access to the compositor pad for the fade animator.
    pub(crate) fn compositor_pad(&self, layer: Layer) -> gst::Pad {
        self.layer(layer).compositor_pad.clone()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_pipeline_without_media() {
        let engine = MediaEngine::new().expect("build");
        // Alphas start at A=1.0, B=0.0.
        assert!((engine.alpha(Layer::A) - 1.0).abs() < 1e-6);
        assert!((engine.alpha(Layer::B) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn set_alpha_direct() {
        let engine = MediaEngine::new().expect("build");
        engine.set_alpha(Layer::B, 0.5);
        assert!((engine.alpha(Layer::B) - 0.5).abs() < 1e-6);
    }
}
