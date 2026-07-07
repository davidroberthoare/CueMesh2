# UI Improvements Plan (2026-07-07)

Driven by operator feedback. Five workstreams; implemented bottom-up so the
tree keeps compiling.

## 1. Client screen: just the canvas
- Remove the egui top bar and the large connect overlay. The window is a plain
  resizable box; the video canvas fills it on a black background.
- **Text only in two states:**
  1. *Startup / disconnected* — a small grey status line at the bottom
     (`CueMesh2 · <name> · <state>`). Hidden once connected and showing media.
  2. *Testscreen* — a large centred block showing the client's name + short id,
     drawn over the SMPTE pattern.
- **Auto-connect**: when offline and no explicit `desired_url`, adopt the first
  mDNS-discovered controller. Manual override stays available via
  `CUEMESH_CONTROLLER`. Removes the need for an in-window connect UI.
- New `AppState.testscreen_on` flag, set by the SHOW/HIDE_TESTSCREEN handlers.

## 2. Controller icons
- egui's default fonts lack the emoji/symbol glyphs currently used. Add
  **`egui-phosphor` 0.7** (pure-Rust icon font, egui 0.29) and install it into
  the fonts of both apps. Replace unicode markers with Phosphor glyphs.

## 3. Cue model simplification (cross-cutting)
- **One fade time per cue**: keep `fade_in_ms`, drop `fade_out_ms` and
  `crossfade_to_next_ms`. A cue's `fade_in_ms` is the fade-from-black time when
  nothing is playing, and the crossfade duration when a cue is already on air
  (the *incoming* cue's fade-in drives the crossfade).
- **New `CueKind::Color`**: a solid-colour cue (black/white/…) with its own
  `color` (hex) and `fade_in_ms`. This is how you "fade to black" now.
- **Remove `[show.settings] default_fade_ms`** and the whole `ShowSettings`
  struct. The operator BLACKOUT command and the client `black` dropout policy
  use a fixed `DEFAULT_FADE_MS = 1500` constant instead.
- Files touched: `shared/show.rs`, `shared/protocol.rs` (LoadCue + ShowSync),
  `media/pipeline.rs` (solid-colour producer), `client/connection.rs`,
  `controller/server.rs`, `controller/ui.rs`, example show, tests.

## 4. Editor as a cue table
- Keep the top section (title, media root, dropout, sync params) minus the
  removed default-fade field.
- Bottom becomes a **data-table**: columns Name | Type | Source | Fade-in |
  actions (up/down/dup/del on the right). `id` is hidden and auto-generated
  (stable per row; unique-ified on build).
- `Source` cell is a file picker for video/image, a colour picker for colour.

## 5. File open/save via egui
- Add **`egui-file-dialog` 0.7** (pure egui, no native GTK/Qt — unlike `rfd`).
- Controller "Open show…" → file-select dialog filtered to `*.cuemesh.toml`.
- Editor "Save" / "Save as…" → save dialog seeded with the current path.

## Order
shared → media → client → controller/server → controller/ui+editor → fonts/deps
→ example show + docs → build + tests.
