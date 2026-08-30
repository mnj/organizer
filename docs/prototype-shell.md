# Prototype: maximized split-view shell & preview pane

**Branch:** `prototype/shell-preview`  
**Crate:** `organizer-prototype/` (throwaway, not merged to `main`)  
**Run:** `cargo run -p organizer-prototype` (requires `gtk4-devel` + `libadwaita-devel` on Fedora, or `libgtk-4-dev` + `libadwaita-1-dev` on Ubuntu)

## Question

What does the maximized split-view shell look like in GTK4, and how does preview + action bar behave? Fixed ratio vs resizable? Aspect-fit vs fill? Placeholder, HiDPI, keyboard, overflow?

## Layout

```
┌─────────────────────────────────────────────┐  maximized ApplicationWindow
│ Preview (dark #1e1e2e, Picture Contain)      │  ← Paned top, ~80% height
│  ┌─────────────────────────────────┐        │
│  │  center icon + kind label       │        │  Picture::content_fit = Contain
│  │  (mock Texture / Paintable)     │        │  can_shrink = true, aspect-fit
│  └─────────────────────────────────┘        │
│  or: Unsupported card (filename + badge)     │  Stack: "picture" vs "placeholder"
├─────────────────────────────────────────────┤  ← Paned handle (wide, draggable)
│ Action Bar (~140px min, ~20%)               │
│  [←][→] | 42 / 312 — foo.jpg  [Duplicate]  … [Undo][Redo] [⚙] │
│  [ Keep [1] ]  [ Maybe [2] ]  [ Reject [3] ]                  │
│  hint: Press 1–3 or Ctrl+1–3 …                               │
└─────────────────────────────────────────────┘
```

### Window

- `gtk4::ApplicationWindow` maximized on launch (`window.maximize()`), default 1280×800 fallback.
- `GtkPaned` vertical, `wide-handle`, `shrink_start_child=false`, `shrink_end_child=false`, `resize_start_child=true`.
- Ratio ~80/20 set once on `idle_add_local_once` from actual window height (`h*0.80`). User-draggable handle; **no persistence v1** (per decision: not remembered until validated).
- Preview has `hexpand/vexpand`, dark CSS `#1e1e2e` for letterboxing (Contain leaves bars, not stretch).

### Preview

- `gtk::Picture` with `content_fit = Contain`, `can_shrink = true`, `halign/valign = Fill` — letterboxes with dark background, no crop.
- Real path (not in prototype): `glycin → GdkTexture` for stills/animated, `gst-plugin-gtk4 gtk4paintablesink → Paintable` for video, single `Picture` (`Picture::for_paintable`). HiDPI via `Texture` (device scale handled by GTK).
- States (Stack):
  - **Supported image/animated**: centered emoji icon + kind label (`png/jpg/... glycin → Texture`, `gif animated`, `webm GStreamer Paintable auto-play loop`, `svg vector`). In real: set `picture.set_paintable(texture/paintable)`.
  - **Unsupported**: placeholder card with filename + `Unsupported` badge (peach `#fab387`) + hint “Skippable via Prev/Next — never moved or hashed”. Same as spec “Unsupported → placeholder, never moved/hashed”.
  - **Loading/error** (not mocked but same slot): spinner + badge in real.
- Background stays `#1e1e2e` in all states (no white flash).

### Action Bar

- Top row: `Prev`/`Next` arrows, `Separator`, index counter `42 / 312 — foo.jpg`, `Duplicate` badge (pink `#f38ba8`, visible only for dup mock), spacer, `Undo`/`Redo` (sensitive only when history/redo non-empty), circular gear `Settings`.
- Bottom row: N action buttons centered, each `Label + [n] shortcut badge`. Default 3 (Keep/Maybe/Reject) with `suggested-action`/`destructive-action` styling; reflects `organizer.toml` 1–9 model. Overflow not handled v1 (wraps if N>fits; future: flowbox/scroll).
- Hint line: `Press 1–3 or Ctrl+1–3 …`.

### Keyboard

- Single `EventControllerKey` on window (global, works regardless of focus):
  - `1`–`9` and `Ctrl+1`–`9` both trigger same Action (per ADR 0002). Unconfigured slot toasts `Action N unconfigured`.
  - `Ctrl+Z` undo, `Ctrl+Shift+Z` / `Ctrl+Y` redo (50-entry LIFO, mock).
  - `Ctrl+,` settings, arrow/`n`/`p`/`Space` prev/next.
- Propagates `Stop` on handled keys to avoid double firing.

### Mock data

8 files (mix to show placeholder + dup badge):
`001-beach-sunset.jpg` (image), `002-notes.pdf` (unsupported), `003-funny-cat.gif` (animated), `004-clip.webm` (video), `005-portrait.heic` (image), `006-backup.zip` (unsupported), `007-city.tiff` (image, duplicate), `008-icon.svg` (vector). No real moves: button press shows toast `Move foo.jpg → Keep (mock)` or `Duplicate … → duplicate/ (mock toast)` then auto-advances; `Next/Prev` just steps index. Undo/Redo pops history (mock `_undo` suffix semantics).

### How to run

```bash
# Fedora
sudo dnf install gtk4-devel libadwaita-devel
cargo run -p organizer-prototype

# Ubuntu 22.04 (AppImage base)
sudo apt install libgtk-4-dev libadwaita-1-dev
cargo run -p organizer-prototype
```

Headless CI: `cargo check -p organizer-prototype` or `cargo build`. No display needed.

### Key choices & next steps

- **Contain, not Fill** — preserves aspect, dark letterbox matches HiDPI Texture path; fill would crop.
- **Paned not fixed Box** — lets user trade preview vs controls; ratio not persisted v1 to avoid settings churn before validation.
- **Single Picture** for all media — matches rendering-stack decision (glycin + GStreamer both produce Paintable).
- **EventControllerKey on window** — satisfies “both 1-9 and Ctrl+1-9” even when Settings field focused (Ctrl variant) vs bare digit fast-path.
- **Toast overlay, not modal** — auto-move-duplicate feedback without blocking triage flow.
- Next: wire real `glycin::Loader` + `gst-plugin-gtk4` paintable, `AdwPreferencesWindow` live+Save, pre-hash spinner, 10k-file perf.

### Screenshot (headless description)

Maximized window, top ~80% dark preview showing centered 🖼️ + “png/jpg … Contain • dark #1e1e2e”, bottom bar with 3 buttons (`Keep [1]` blue, `Maybe [2]`, `Reject [3]` red), nav `1 / 8 — 001-beach-sunset.jpg`, `Undo` disabled, gear on right, toast at bottom center after action. Unsupported file shows card with filename + peach `Unsupported` badge. Duplicate file shows pink `Duplicate` badge next to index.
