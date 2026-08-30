# Research: rendering & decoding stack for GTK4 preview (ffmpeg vs native libs vs gstreamer)

> Ticket: [#2](https://github.com/mnj/organizer/issues/2) — Part of Map [#1](https://github.com/mnj/organizer/issues/1)
> Branch: `research/rendering-stack` · Date: 2026-08-30 · Status: completed

## Question

What rendering/decoding stack should the GTK4 Rust preview use to cover all required formats (png, jpg/jpeg, webp, gif, webm + other common stills/animated/video) — native Rust libs vs gdk-pixbuf vs gstreamer vs ffmpeg/ffprobe — and what are the trade-offs?

## TL;DR recommendation

**Primary stack: `gtk4` (bare) + `gtk::Picture` over `gdk::Paintable`/`gdk::Texture` + `glycin` (default) for stills/animated stills + `GStreamer + gst-plugin-gtk4 (gtk4paintablesink)` for video/animated containers.**

* Render *everything* through a single `gtk::Picture` (or `GtkPicture`-backed `RenderWidget`). Swap its `paintable` between a `gdk::Texture` (from glycin) and the `GstGtk4Paintable` exposed by `gtk4paintablesink`. This is the GNOME-blessed path used by Loupe, Showtime, and the `gst-plugins-rs` examples.
* Avoid `gdk-pixbuf` direct use for untrusted files (deprecated `PixbufAnimation`, sync-only, no sandbox). Avoid shelling out to `ffmpeg`/`ffprobe`. Avoid linking `ffmpeg-next` unless you need frame-accurate CPU thumbnailing beyond what GStreamer already does.
* Use `relm4` only if you want Elm-style components/factories (e.g. action grid, duplicate list). For the single-file triage window, bare `gtk4` is simpler to review and keeps the wayfinder shell prototype unblocked.

### Format → decoder matrix (recommended)

| Format (as seen on disk) | Decoder (recommended) | Render path | Fallback / notes |
|---|---|---|---|
| **png, jpeg/jpg, bmp, tiff, tga, pnm, ico, bmp** | `glycin` → `gdk::Texture` | `Picture::for_paintable(texture)` | `image` crate or `gdk-pixbuf` sync path if glycin loaders missing; `GdkTexture::from_bytes()` only for trusted resources |
| **webp (still)** | `glycin` | same | `gdk-pixbuf` webp loader is optional at distro build time; `image` crate `webp` feature works but requires manual `Texture` upload + extra copy |
| **webp (animated), gif (animated)** | `glycin` (keeps loader alive for frames) | `Picture` tracks `Paintable::invalidate_contents` animation | GStreamer `decodebin` can also play them; `image` crate `gif` can decode `Frames` but needs custom `Paintable` ticker |
| **svg / svgz** | `glycin` (scalable) or `gdk-pixbuf` SVG loader | `Texture`/`Paintable` | Gdk-pixbuf `is_scalable()==true` — prefer loading at desired size via glycin rather than scale-after-load |
| **avif, heic/heif, jxl, qoi, dds, farbfeld, OpenEXR** | `glycin` | same | Added for free if you adopt glycin; `image` crate covers avif (with `mp4parse`+`dav1d` native deps) / exr / qoi likewise |
| **webm, mp4/mov, mkv, avi, h264/h265, vp8/vp9, av1** | **GStreamer** `playbin`/`decodebin` + `gtk4paintablesink` → `GdkPaintable` | `picture.set_paintable(paintable_from_sink)` via `gstgtk4::RenderWidget` or direct `Picture` | `ffmpeg-next` *can* do this but then you reimplement `videoconvert`/`videoscale`/`swscale` → `Texture` upload yourself |
| **audio-only or unsupported** | — | placeholder (Icon + filename + error toast) | Do not attempt image decode; show `MediaFile` controls only if you later add audio |

> If you must remain pure-Rust with zero system libs: the `image` crate with `default-formats` + `image-webp` covers stills + animated gif/webp (via frame iterator) at the cost of manual texture upload, no video, and synchronous decode you must thread yourself. Keep it as a cargo fallback for non-Linux CI, not the primary.

## 1. GTK4 Rust binding choice: `gtk4` vs `relm4`

### `gtk4` (gtk4-rs) — bare bindings
* **Source:** `gtk4` crate docs [gtk-rs.org/gtk4-rs/stable](https://gtk-rs.org/gtk4-rs/stable/latest/docs/gtk4/) + crate `0.11.3`, MSRV `1.83`. Wraps `libgtk-4.so` via `gtk4-sys`, re-exports `gdk4`, `gio`, `glib`, `gsk4`, `gdk-pixbuf`. MIT licensed; underlying GTK is LGPL-2.1-or-later.
* Idiomatic builder pattern (`Application::builder()`, `ApplicationWindow::builder()`, `connect_activate`), explicit main loop (`app.run()`), panics if called off main thread or before `init` — matches GTK's thread model.
* Version features `v4_6`..`v4_22`, `gnome_42`..`gnome_50` gate newer APIs (e.g. `Picture::for_paintable` added earlier, `GLTexture`/`DmabufTexture` stabilized in `v4_14`).
* Pros: no macro magic, full control, every GTK example translates 1:1, CI is just `libgtk-4-dev`.
* Cons: imperative signal wiring, manual state hoisting for split-view, no built-in async helpers beyond `glib::MainContext`.

### `relm4` — Elm architecture on top of gtk4-rs
* **Source:** `relm4.org` + `docs.rs/crate/relm4` (current `0.11.x`), GitHub `Relm4/Relm4` (1.9k stars, Apache-2.0). Requires `gtk4` already; adds `Component`, `Factory`, `Worker`, `AsyncComponent`, `RelmApp`, `view!` macro, `spawn`/`spawn_blocking` helpers, typed `Sender`/`Receiver`.
* `Factory`/`TypedView` helps for collections (action grid, file queue); `AsyncComponent` + `tokio`/`glib` executors wrap background tasks.
* Platform: same as GTK4 (Linux/Windows/macOS). `relm4-template` shows Meson+Flatpak wiring identical to `gtk-rust-template`.
* Trade-offs distilled from 2025 Rust GUI survey + relm4 book: cleaner at scale, but `view!` errors are macro-obscure, inherits all GTK-on-Windows quirks, extra abstraction for a window that is essentially `Paned( top=Picture , bottom=ActionBar )` + keyboard accelerators.
* **Recommendation for Organizer:** start with bare `gtk4` for the shell/`research` prototype path. If grilling of settings/queue yields repeated `Factory`-shaped state (per-action config rows, duplicate list with selection), adopt `relm4` incrementally for those subtrees — it composes with plain `gtk4` widgets. Do not block the rendering decision on this choice.

## 2. Preview widget: `Picture`/`Paintable` is canonical

### `gtk::Picture` (since GTK 4.0)
* **Primary sources:** `docs.gtk.org/gtk4/class.Picture`, `gtk-rs.org/gtk4-rs/*/struct.Picture`, `docs.gtk.org/gdk4/class.Texture`/`interface.Paintable`.
* `Picture` displays any `gdk::Paintable`. Intrinsic size comes from paintable; `content-fit` (`Contain`/`Cover`/`Fill`/`ScaleDown`) controls fit, `can-shrink` guards against screen-overflow on large images, `halign`/`valign` centers small images — exactly what a triage preview needs.
* `Picture::for_paintable(&paintable)` tracks `invalidate_contents`/`invalidate_size`; call `set_paintable(Some(&next))` to hot-swap between image and video without recreating the widget.
* `Picture::for_filename`/`for_resource` are deprecated for untrusted input — docs emit `::: warning` and redirect to *"use a proper image loading framework such as libglycin, which can load many image formats into a `gdk::Texture`, and then use `for_paintable()`"*.
* `gtk::Image` displays an icon-sized paintable (subject to `icon-size`); wrong for a maximizing preview.

### `gdk::Texture` vs other paintables
* `Texture` is the immutable, threadsafe still-image paintable. `Texture::for_pixbuf()`, `from_bytes`, `from_file` are threadsafe so you can `gio::Task::run_in_thread` off the main loop (still deprecated for untrusted data).
* Custom paintables: `GLTexture`, `DmabufTexture`, or your own `Paintable` impl via `snapshot()` (draw with cairo/GL). `gtk4paintablesink` supplies its own `GstGtk4Paintable` that optionally backs with GL/DMABuf zero-copy (see §4).

### Alternatives considered
* `GtkDrawingArea` (cairo callback): works but *"not very efficiently because it goes around cairo"* per `discourse.gnome.org/t/gstreamer-in-gtk4`. Requires manual scaling/caching.
* `GtkGLArea`: needed only for custom shaders; overkill for triage.
* `GtkVideo`/`GtkMediaStream`/`GtkMediaFile`: built-in GTK playback with controls and `GtkMediaControls`. `GtkMediaFile` can use GStreamer under the hood via GIO extension point (`gtk_media_file_*` docs), but the default `gtk::Video` does not expose a custom `gst::Pipeline`. The `gst-plugin-gtk4` docs explicitly say *"As the default `gtk::Video` doesn't offer the possibility to use a custom `gst::Pipeline`, the plugin provides a `gst_video::VideoSink` along with a `GdkPaintable` ..."* — so for `file://` triage with hash-logging + control over pipeline (preloading, mute, loop), use the paintable sink instead of `GtkVideo`.

**Decision:** one `Picture` in `ScrolledWindow` or `AspectFrame`/`Overlay` (for OSD). No `DrawingArea`/`GLArea`.

## 3. Decoder comparison (primary sources cited)

### 3.1 `gdk-pixbuf` (C, `gdk-pixbuf` crate — gtk-rs-core)
* Docs: `gtk-rs.org/gtk-rs-core/*/gdk_pixbuf`, `stackoverflow.com/q/7704670`, `gdk-pixbuf-query-loaders` manpage.
* Default loaders (vary by distro build): `png`, `jpeg`, `gif`, `bmp`, `ico`, `tiff`, `pnm`, `svg` (via librsvg), `xpm`, `tga`, `ani/icns` — **webp is not guaranteed**. Query with `Pixbuf::formats()` / `gdk_pixbuf_get_formats()`.
* API: `Pixbuf::from_file`, `PixbufLoader` incremental, `PixbufAnimation` (deprecated header says *"Use a different image loading library for animatable assets"*), `PixbufFormat::is_scalable()`, `license()`, `is_disabled()` (can block loaders with unacceptable license).
* Threading: only `THREADSAFE`-flagged loaders (`PixbufFormatFlags::THREADSAFE`) are used; `GdkTexture::for_pixbuf()` is threadsafe.
* Cons for Organizer: sync, no sandbox, format coverage fragmented, animated webp/video absent, security footnote in GTK 4.14+ pushes to glycin. Since `gdk-pixbuf 2.43.2` the `glycin` builtin loader is the path forward.

### 3.2 `image` crate (Rust, `image-rs/image` — crates.io)
* Docs: `docs.rs/crate/image/latest`, `github.com/image-rs/image`, crate `0.25.10`, ~1.36 M downloads/mo, MSRV not specified but `1.83` via `gtk4` alignment. License MIT+Apache-2.0 for Rust, but format codecs vary: `image-png` (pure Rust), `jpeg-decoder`/`zune-jpeg`, `tiff`, `gif`, `image-webp`, `qoi`, `ravif` (needs `nasm` + `dav1d`/`mp4parse` if `avif-native`).
* With `default-formats`: AVIF, BMP, DDS, EXR, FF, GIF, HDR, ICO, JPEG, PNG, PNM, QOI, TGA, TIFF, WebP — covers Organizer's png/jpg/webp/gif; **no webm/mp4**.
* API: `ImageReader::open(path)?.with_guessed_format()?.decode()?` → `DynamicImage` → `ImageBuffer`. `ImageDecoder::dimensions/total_bytes/set_limits` + `Limits` (see Issue #938 discussion). Default `ImageReader` caps at ~512 MiB (`LimitError::InsufficientMemory`); call `Reader::no_limits()` or `Limits` to allow large files.
* Async story: **none** — decoders take `Read`/`Seek` (Issue #1397). Workaround is to download fully then `spawn_blocking`/`rayon`. For Organizer, wrap in `gio::Task` or `relm4::spawn_blocking`.
* Performance: pure-Rust decode, `rayon` feature for EXR/others, but must copy `RgbaImage` bytes into `gdk::Texture` (e.g. `Texture::new_for_pixbuf` or `Texture::from_bytes` + `MemoryTexture` with `bytes`). Double allocation for large images; no hardware path.
* Keep as fallback for headless tests / Windows/macOS without glycin sandbox, but not primary.

### 3.3 `glycin` (Rust, GNOME — `GNOME/glycin`, crates.io `glycin 3.x`, `libglycin-gtk4`)
* Docs: `gnome.pages.gitlab.gnome.org/glycin`, `github.com/GNOME/glycin`, `docs.rs/crate/glycin`, `blogs.gnome.org/sophieh 2025-06-13`.
* What it is: sandboxed, modular image loaders → `gdk::Texture`. One process per image, D-Bus over Unix socket, `bwrap` sandbox + seccomp + `setrlimit` outside Flatpak, `flatpak-spawn --sandbox` inside Flatpak; texture via sealed memfd mmap'd to GDK. Keeps process alive for animations/SVG tiles.
* Format coverage (loaders crate): AVIF, BMP, DDS, Farbfeld, QOI, GIF, HEIC, ICO, JPEG, JPEG XL, OpenEXR, PNG, PNM, SVG, TGA, TIFF, WebP — superset of Organizer needs. Rust rewrites fix historic `image-rs`/`zune` gaps (TIFF still has edge-case TODOs; fallback to old pixbuf possible).
* API (Rust): `glycin::Loader::new(file).with_mime_type(...).load(...).await?` → `Image` → `Frame` → `gdk::Texture` (with `gdk4` feature). `tokio` optional feature for `zbus` compat. The GTK docs recommend exactly this: *"load with libglycin, then create the Picture with `for_paintable()`"*.
* Packaging: `glycin-loaders` + `libglycin-2` (+ `libglycin-gtk4-1-0` on Debian trixie). Build with `-Dglycin-loaders=true` etc. For GdkPixbuf integration: `gdk-pixbuf 2.43.2+ -Dbuiltin_loaders=glycin`.
* Limits: **Linux-only sandbox**; BSD/macOS/Windows get in-process unsandboxed compilation (planned `"compile loaders into library"` mode) — still safer than C loaders because Rust memory safety, but not isolated. `bwrap` missing breaks sandboxed mode (CI/test footgun noted in glycin limitations). Content streamed via Unix socket, so network `GFile` without giving loader network access.
* Verdict: **Adopt for Organizer's Linux/Flatpak primary**. It is what `Loupe` (GNOME Image Viewer) already uses and what `GdkPixbuf` is migrating to.

### 3.4 GStreamer (`gstreamer-rs` + `gst-plugins-rs`/`gst-plugin-gtk4`)
* Docs: `gstreamer.freedesktop.org/documentation/gtk4` (`gtk4paintablesink`), `docs.rs/crate/gst-plugin-gtk4 0.15.2`, `github.com/GStreamer/gst-plugins-rs`, discourse thread `replacing videotestsrc with filesrc` (playbin + gtk4paintablesink example, `Válá` sample `PlaybinGtk4SinkWindow`).
* Role: container/codec-agnostic pipeline (`playbin` or `filesrc ! decodebin ! videoconvert ! gtk4paintablesink` / `glsinkbin`). Handles webm (VP8/VP9/AV1+Opus/Vorbis), mp4 (H.264/H.265), gif/webp-animated via `decodebin`, plus future AV1 etc.
* GTK integration: sink exposes `paintable` (`GstGtk4Paintable`) + convenience `gstgtk4::RenderWidget`. Docs: *"The plugin provides a `gst_video::VideoSink` along with a `GdkPaintable` capable of rendering the sink's frames. ... GL Textures if compiled with `waylandegl`/`x11glx`/`x11egl`; DMABuf direct on Linux if GTK 4.14+"* with `dmabuf` feature.
* Flatpak: build as `simple` module via `cargo cinstall --offline --release --features=waylandegl,x11glx,x11egl,dmabuf --library-type=cdylib --prefix=/app`, SDK extension `org.freedesktop.Sdk.Extension.rust-stable`, `GST_PLUGIN_PATH` or `GST_PLUGIN_SYSTEM_PATH`. Upcoming `Freedesktop SDK 24.08+` / `GNOME 47/48` runtimes will include `gst-plugins-rs` by default.
* Perf: hardware decode via VA-API where available; zero-copy GL/DMABuf paths documented in Centricular devlog `2024-04/gtk4-dmabuf-import` (GStreamer→GTK4 without RGB download). Bus watches `Error`/`Eos` for error handling.
* License: GStreamer LGPL-2.1+, `gstreamer-rs` MIT OR Apache-2.0 — compatible with proprietary Organizer (no GPL spill if you avoid `gst-plugins-bad` nonfree elements).

### 3.5 `ffmpeg-next` (Rust wrapper over libav*) + shelling out
* Docs: `crates.io/crates/ffmpeg-next` (WTFPL for Rust glue, but linked FFmpeg is `COPYING.LGPLv2.1` with optional `GPL`/`nonfree`), `ffmpeg.org/legal.html`.
* Linking mode (`ffmpeg-next` crate tracking FFmpeg 3.4–8.x, currently `8.1.0` with `ffmpeg-sys-next`): need `libavcodec/avformat/avutil/swscale` installed, or build FFmpeg from source. Produces decoded `AVFrame` RGB you must `swscale` + upload to `Texture`/`Paintable` yourself; no GTK sink.
* License minefield: default `LGPL-2.1+`, `--enable-gpl` (required for `libx264`) promotes to GPL-2+, `--enable-nonfree` (`libfdk_aac`) → unredistributable. Must audit Flatpak FFmpeg build — easiest to avoid entirely if GStreamer suffices.
* Shelling out (`Command::new("ffmpeg")`/`ffprobe`): avoids link-time GPL but adds process spawn + stdout parsing + failure modes + double I/O (read file, write pipe). No benefit over GStreamer except `ffprobe` JSON for duration/tags without GStreamer init (still doable via `gst::Discoverer`).
* Verdict: do not adopt for playback. If you later need precise seek/thumbnail generation at arbitrary timestamps without a running pipeline, evaluate `gstreamer-rs` `Discoverer` or `gst::ElementFactory::make("thumbnailbin")` first.

## 4. System deps, license, and distribution

| Component | System dep (Debian/Arch/Homebrew) | License (app obligation) | Flatpak runtime |
|---|---|---|---|
| `gtk4` + `libadwaita` (if used) | `libgtk-4-dev` | App: your choice (MIT/Apache recommended); GTK LGPL | `org.gnome.Platform//48`+ includes GTK 4.14–4.16 |
| `glycin` + loaders | `libgtk-4-dev liblcms2-dev libfontconfig-dev libseccomp-dev glycin-loaders bubblewrap` | Rust MIT/Apache; loaders MIT/Apache | GNOME 47+ Sdk includes `glycin` 2.x; or bundle via `glycin-loaders` module |
| `gstreamer` + plugins | `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav` | LGPL-2.1+ (good/bad split; avoid `ugly`/`nonfree` elements) | `org.gnome.Sdk` already bundles GStreamer; `gst-plugin-gtk4` built via extension `rust-stable` |
| `ffmpeg-next` (if used) | `libavcodec-dev …` or built FFmpeg | LGPL or GPL depending on configure → must ship source + LGPL notice | Adds large build + x264 GPL concern |
| `image` crate | none (pure Rust default) | MIT+Apache | none |

For Organizer's `*.Devel.json`/`*.yml` manifests, the minimal extra over stock `org.gnome.Sdk` is one `simple` module for `gst-plugin-gtk4` with `flatpak-cargo-generator`-produced `gst-plugin-gtk4-sources.json` + optional `glycin-loaders` module if runtime predates glycin. Reference manifests are in `gst-plugin-gtk4` docs and `flathub` discourse example (see References). Windows/macOS: `gtk4` via MSYS2/gvsbuild or `brew install gtk4 gstreamer gst-plugins-base gst-plugins-good ...`; `glycin` sandbox degrades gracefully (no `bwrap`/`flatpak-spawn`).

## 5. Performance & memory: large images, async, caching

* Large stills (50–100 MB on disk → 200 MB–1 GB RGBA): `image` crate default `512 MiB` cap via `Limits` protects against decompression bombs; raise via `Reader::set_limits(Limits::no_limits())` only after you check `dimensions` or `total_bytes`. With `glycin`, the loader's `setrlimit` enforces sandbox memory cap before mmap; the `Texture` itself is refcounted and shared via memfd. In both cases, prefer **scaled load** for preview: glycin/GdkPixbuf SVG-scalable loaders can load at concrete `Picture` size; for raster, downscale after decode via `Texture` with `content-fit: ScaleDown` (GPU) or `gdk::Texture` scaled snapshot. Do not keep full-res `Pixbuf` + `Texture` duplicates.
* Async: nothing in GTK's `Picture` is async. Decouple decode:
  * Glycin: `loader.load().await` on `glib::MainContext`/`tokio` (zbus), then `set_paintable` on main thread.
  * GStreamer: pipeline runs on its own threads; paintable invalidation arrives via `glib::MainContext` signal. Keep one `playbin` per preview (reuse, set `uri` + `set_state(Ready/Playing)` per file).
  * `image`/pixbuf: `gio::Task::run_in_thread` / `relm4::spawn_blocking` / `tokio::task::spawn_blocking` → `glib::idle_add_local` to set texture. Always use `GDK` threadsafe constructors (`Texture::for_pixbuf`, `Texture::from_bytes`).
* Preloading: decode next file in background while showing current. Cache `Texture`/paintable by path + `mtime` + `size` in an LRU (e.g. 2–3 entries). Evict when `invalidate_size` or file changes. Hash computation (`sha256` for log/duplicate) should be parallel `spawn_blocking` streaming read, not coupled to decode.
* Video specifics: GL/DMABuf zero-copy avoids CPU readback. Enable `gtk_v4_14` + `dmabuf` feature on Linux; on Windows/macOS the sink falls back to GL textures automatically (no extra flags). Seek via `gst_element_seek_simple`.

## 6. Error handling (corrupt/unsupported)

* GTK's built-in `Picture` file loader shows a broken-image icon on failure — insufficient for triage. With `for_paintable` you control it:
  * `Ok(texture/paintable)` → `picture.set_paintable(Some(&p))` + update window title/metadata.
  * `Err(e)` from glycin / GStreamer bus `Error` / `image::ImageError::Unsupported`/`Limits::InsufficientMemory` → show placeholder: `gtk::Image::from_icon_name("image-missing")` + `Label` with filename + `e` truncated, plus optional `Toast` (via `adw::ToastOverlay` if using libadwaita, else `InfoBar`). Offer `Skip` / `Reveal in Files` actions.
  * For `Duplicate` detection, error handling is orthogonal — the preview still renders; the action bar shows "already in `action_name.txt` → will go to `duplicate/`".
* Log decode failures; do not block queue advancement. Consider a per-session "failed preview" count in header.

## 7. Minimal proof sketch (what the shell prototype should demonstrate)

```rust
// Cargo.toml (excerpt)
// [dependencies]
// gtk4 = { version = "0.11", features = ["v4_14", "gnome_48"] }
// gdk4 = "0.11"
// gio = "0.11"
// glycin = { version = "3", features = ["gdk4", "tokio"] }
// gstreamer = "0.25"
// gst-plugin-gtk4 = { version = "0.15", features = ["waylandegl","x11egl","dmabuf"] }

use gtk4::prelude::*;
use gdk4::Paintable;
use gtk4::{Application, ApplicationWindow, Picture, Box as GtkBox, Orientation};
use gio::File;

fn build_ui(app: &Application) {
    gst::init().unwrap();
    gstgtk4::plugin_register_static().expect("gst gtk4 plugin");

    let win = ApplicationWindow::builder().application(app).title("Organizer — preview").default_width(1280).default_height(800).build();
    let vbox = GtkBox::new(Orientation::Vertical, 0);
    let picture = Picture::builder().can_shrink(true).content_fit(gtk4::ContentFit::Contain).hexpand(true).vexpand(true).build();
    vbox.append(&picture);

    // --- glycin path (stills/animated stills) ---
    let file = File::for_path("/path/to/image.webp");
    let pic_clone = picture.clone();
    glib::MainContext::default().spawn_local(async move {
        match glycin::Loader::new(file).load().await {
            Ok(img) => {
                // keep animation alive: hold `img`/`Frame` if animated; connect invalidate
                let frame = img.next_frame().await.expect("frame");
                let tex = frame.texture(); // gdk::Texture impl Paintable
                pic_clone.set_paintable(Some(&tex));
            }
            Err(e) => {
                eprintln!("glycin decode failed: {e}");
                // show placeholder instead of broken icon
            }
        }
    });

    // --- GStreamer path (webm/mp4, also animated webp/gif fallback) ---
    let picture2 = picture.clone();
    let show_video = move |uri: &str| {
        let playbin = gst::ElementFactory::make("playbin").property("uri", uri).build().unwrap();
        let gtksink = gst::ElementFactory::make("gtk4paintablesink").build().unwrap();
        let paintable = gtksink.property::<Paintable>("paintable");
        picture2.set_paintable(Some(&paintable));
        playbin.set_property("video-sink", &gtksink);
        playbin.set_state(gst::State::Playing).unwrap();
        // watch bus for Error/Eos → placeholder + teardown
        let bus = playbin.bus().unwrap();
        let _watch = bus.add_watch_local(move |_, msg| {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(e) => eprintln!("GStreamer error: {}", e.error()),
                MessageView::Eos(..) => {} ,
                _ => {}
            }
            glib::ControlFlow::Continue
        }).unwrap();
    };
    // show_video("file:///path/to/clip.webm");

    win.set_child(Some(&vbox));
    win.maximize(); // meets "maximized split-view" requirement
    win.present();
}
```

Prototype acceptance: the shell can toggle between a `.png`/`.webp`/`.gif` via glycin and a `.webm` via `gtk4paintablesink` without swapping the widget — only `set_paintable`.

## 8. Open questions / not-yet-specified (hand back to grilling/map)

* Does Organizer need in-preview video controls (play/pause/seek/mute/loop) or is autoplay-muted-loop sufficient for triage speed? Determines whether to wrap `gtk4paintablesink` paintable in a custom `snapshot` widget with OSD or just `Picture`.
* Hash-log and `duplicate/` location lifecycle (per-source-folder vs XDG state) — orthogonal to rendering.
* Undo durability vs session-only — ditto.
* Whether to support HEIC/AVIF/JXL out of the box via glycin loaders (licensing of underlying codecs `libheif`/`dav1d` is LGPL/BSD but patents may matter for distribution).

## References (primary sources)

* `gtk4` crate `0.11.3` — gtk-rs.org/gtk4-rs/stable/latest/docs/gtk4 (MSRV, Hello World, `Picture` API, `Texture` threadsafe notes).
* `docs.gtk.org/gtk4/class.Picture` + `docs.gtk.org/gdk4/class.Texture` + `interface.Paintable` (content-fit, paintable invalidation, glycin warning).
* `gdk-pixbuf` crate docs — `gtk-rs.org/gtk-rs-core/*/gdk_pixbuf` (`PixbufFormat`, `PixbufLoader`, deprecated `PixbufAnimation`).
* `image` crate `0.25.10` — `docs.rs/crate/image`, `github.com/image-rs/image`, Issues `#1397` (no async) and `#938` (Limits/memory), `crates.io/crates/image` features.
* `glycin` — `GNOME/glycin` repo README, `gnome.pages.gitlab.gnome.org/glycin`, `docs.rs/crate/glycin` (sandbox via `bwrap`/`flatpak-spawn`/seccomp, `libglycin-gtk4`, loaders coverage), blog `Making GNOME's GdkPixbuf Image Loading Safer` (Sophie Herold 2025-06-13, `gdk-pixbuf 2.43.2 builtin_loaders=glycin`).
* `GStreamer gtk4paintablesink` — `gstreamer.freedesktop.org/documentation/gtk4` + `docs.rs/crate/gst-plugin-gtk4 0.15.2` (sink + Paintable + `gstgtk4::RenderWidget`, GL/DMABuf features, Flatpak manifest snippets), `discourse.gstreamer.org t/replacing videotestsrc` (playbin + filesrc examples), `docs.vala.dev GStreamer sample playbin + gtk4paintablesink`, `centricular.com devlog 2024-04 gtk4-dmabuf-import`.
* Media/playlist plumbing — `docs.gtk.org/gtk4/class.Video`/`class.MediaFile`/`class.MediaStream` (GIO extension point note: GStreamer implementation).
* Flatpak toolchain — `flathub org.freedesktop.Sdk.Extension.rust-stable` README + `flatpak/flatpak-builder-tools cargo/README.md` (`flatpak-cargo-generator`, `.cargo/config.toml` offline pattern), `flathub discourse Bundling gst-plugin-rs` + `developer.kde.org Publishing your Rust app as flatpak`.
* Licensing — `ffmpeg.org/legal.html` + `FFmpeg/LICENSE.md` (LGPL/GPL/nonfree matrix), `gstreamer.freedesktop.org/licensing.html` (LGPL + plugin rules), `gstreamer-rs` license note (MIT OR Apache-2.0 + GStreamer LGPL), `gtk4` MIT, `relm4` Apache-2.0.
* `relm4` — `relm4.org`, `docs.rs/crate/relm4 latest`, `Relm4/Relm4` README, survey `boringcactus 2025-04-13`.

---
*Next step for map:* unblock shell prototype ticket with *recommended stack = `glycin` (stills) + `GStreamer gtk4paintablesink` (video) through single `Picture`*; emit system deps + Flatpak manifest sketch; schedule grilling of video controls & duplicate/hash-log scope.
