//! Video Preview backend (spec #20).
//!
//! GStreamer `playbin` + `gtk4paintablesink` renders Supported Format video
//! (`webm`/`mp4`/`mov`/`mkv`/`avi`) into the same Preview `gtk::Picture` used
//! for glycin stills: the sink exposes a `gdk::Paintable` that is swapped in
//! via `Picture::set_paintable`, with `Contain` + the dark Preview letterbox
//! unchanged.
//!
//! Policy (per research `sandboxed-decoding-raw-appimage` §6, option A):
//! video decoding runs **in-process and unsandboxed** in v1 — glycin stills
//! stay `bwrap`-sandboxed. A helper-process sandbox is tracked as follow-up.
//! Behavioural contract:
//! - muted (`mute=true` + video-only `flags`), auto-play with loop on EOS,
//!   no Play/Pause controls in v1;
//! - corrupt video surfaces a bus `Error` → caller shows the same error
//!   placeholder + toast as glycin decode failure and the Main stays alive;
//! - only `file://` URIs are accepted (Source Folder triage, no network).

use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;

use crate::preview::{classify_extension, SupportedKind};

/// Element factory name for the GTK4 paintable sink.
///
/// Provided at runtime by `gst-plugin-gtk4` (`libgstgtk4.so`), bundled into
/// the AppImage via `cargo cinstall gst-plugin-gtk4
/// --features waylandegl,x11egl,dmabuf --library-type=cdylib`
/// (see `packaging/AppRun`). `GST_PLUGIN_SYSTEM_PATH` must point at the
/// bundled `$APPDIR/usr/lib/gstreamer-1.0` so no host GStreamer is required.
pub const PAINTABLE_SINK: &str = "gtk4paintablesink";

/// Check whether a path is a Supported Format video (case-insensitive).
/// Mirrors [`crate::preview::is_glycin_supported`] for the video half of the
/// Queue Snapshot.
pub fn is_video_supported(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        classify_extension(ext) == SupportedKind::Video
    } else {
        false
    }
}

/// Convert an absolute filesystem path to a `file://` URI for `playbin`.
/// Percent-encodes (spaces etc.) via `glib::filename_to_uri`.
/// Returns `Err` for relative paths — the Queue Snapshot holds absolute
/// Source Folder children, so this signals a programming error, not a codec
/// failure.
pub fn to_file_uri(path: &Path) -> Result<String, String> {
    if !path.is_absolute() {
        return Err(format!(
            "video path must be absolute for file:// URI: {}",
            path.display()
        ));
    }
    glib::filename_to_uri(path, None)
        .map(|s| s.to_string())
        .map_err(|e| format!("cannot build file URI for {}: {e}", path.display()))
}

/// Idempotent GStreamer init. Safe to call on every Preview switch.
pub fn ensure_init() -> Result<(), String> {
    gst::init().map_err(|e| format!("GStreamer init failed: {e}"))?;
    Ok(())
}

/// Build a muted, video-only `playbin` for `uri` with `sink_name` as its
/// video sink. Returns `(playbin, sink)` — the caller reads the sink's
/// `paintable` property into the Preview `Picture`.
///
/// Only `file://` URIs are accepted (triage-local files, no network fetch).
/// `playbin` flags are video-only (`GST_PLAY_FLAG_VIDEO`) and `mute` is set
/// as belt-and-braces so no audio path is negotiated.
pub fn build_playbin_with_sink(
    uri: &str,
    sink_name: &str,
) -> Result<(gst::Element, gst::Element), String> {
    ensure_init()?;
    if !uri.starts_with("file://") {
        return Err(format!(
            "refusing non-file video URI (triage-local only): {uri}"
        ));
    }
    let sink = gst::ElementFactory::make(sink_name)
        .build()
        .map_err(|e| format!("video sink '{sink_name}' unavailable: {e}"))?;
    let playbin = gst::ElementFactory::make("playbin")
        .build()
        .map_err(|e| format!("playbin unavailable: {e}"))?;
    playbin.set_property("uri", uri);
    playbin.set_property_from_str("flags", "video");
    playbin.set_property("mute", true);
    playbin.set_property("video-sink", &sink);
    Ok((playbin, sink))
}

/// [`build_playbin_with_sink`] with the production [`PAINTABLE_SINK`].
pub fn build_playbin(uri: &str) -> Result<(gst::Element, gst::Element), String> {
    build_playbin_with_sink(uri, PAINTABLE_SINK)
}

/// Borrow the sink's `paintable` for the Preview `Picture`.
/// The caller keeps the sink alive as long as the paintable is shown.
pub fn sink_paintable(sink: &gst::Element) -> Result<gdk4::Paintable, String> {
    if !sink.has_property("paintable") {
        return Err(format!(
            "video sink '{}' has no paintable property",
            sink.factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "?".into())
        ));
    }
    Ok(sink.property("paintable"))
}

/// Restart from the first frame — the auto-play loop for short clips.
/// Called on bus EOS. Failure (e.g. torn-down pipeline) is ignored: the
/// next Preview switch rebuilds the pipeline anyway.
pub fn restart(pipeline: &gst::Element) {
    let _ = pipeline.seek_simple(gst::SeekFlags::FLUSH, gst::ClockTime::ZERO);
}

/// Tear down a pipeline: leave no PLAYING pipeline behind on file switch.
/// Dropping the bus watch guard (held by the caller) removes the watch.
pub fn stop(pipeline: &gst::Element) {
    let _ = pipeline.set_state(gst::State::Null);
}

/// Start playback. The caller attaches [`watch_bus`] first so early
/// decode errors are caught instead of missed.
pub fn play(pipeline: &gst::Element) -> Result<(), String> {
    pipeline
        .set_state(gst::State::Playing)
        .map(|_| ())
        .map_err(|e| format!("cannot play video: {e:?}"))
}

/// Attach a main-context bus watch: `on_error` fires once per bus Error
/// (corrupt video → placeholder + toast, Main stays alive), `on_eos` fires
/// per loop iteration. Returns the guard that must be held while playing.
///
/// Callbacks run on the GTK main thread (`add_watch_local`), so they may
/// touch Preview widgets directly.
pub fn watch_bus(
    pipeline: &gst::Element,
    mut on_error: impl FnMut(String) + 'static,
    mut on_eos: impl FnMut() + 'static,
) -> Result<gst::bus::BusWatchGuard, String> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| "playbin has no bus".to_string())?;
    bus.add_watch_local(move |_, msg| {
        match msg.view() {
            gst::MessageView::Error(err) => {
                on_error(format!("{} ({})", err.error(), err.debug().unwrap_or_default()));
            }
            gst::MessageView::Eos(..) => on_eos(),
            _ => {}
        }
        glib::ControlFlow::Continue
    })
    .map_err(|e| format!("bus watch failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn video_supported_path_case_insensitive() {
        assert!(is_video_supported(&PathBuf::from("/s/clip.webm")));
        assert!(is_video_supported(&PathBuf::from("/s/CLIP.WEBM")));
        assert!(is_video_supported(&PathBuf::from("/s/movie.mp4")));
        assert!(is_video_supported(&PathBuf::from("/s/film.MOV")));
        assert!(is_video_supported(&PathBuf::from("/s/rec.mkv")));
        assert!(is_video_supported(&PathBuf::from("/s/old.AVI")));
        assert!(!is_video_supported(&PathBuf::from("/s/photo.jpg")));
        assert!(!is_video_supported(&PathBuf::from("/s/img.png")));
        assert!(!is_video_supported(&PathBuf::from("/s/anim.gif")));
        assert!(!is_video_supported(&PathBuf::from("/s/doc.pdf")));
        assert!(!is_video_supported(&PathBuf::from("/s/noext")));
    }

    #[test]
    fn file_uri_percent_encodes_and_rejects_relative() {
        let uri = to_file_uri(Path::new("/tmp/my clips/a b.mp4")).unwrap();
        assert_eq!(uri, "file:///tmp/my%20clips/a%20b.mp4");
        let err = to_file_uri(Path::new("relative/clip.mp4")).unwrap_err();
        assert!(err.contains("absolute"), "unexpected: {err}");
    }

    #[test]
    fn gstreamer_init_is_idempotent() {
        ensure_init().expect("first init");
        ensure_init().expect("second init must also succeed");
    }

    #[test]
    fn play_and_stop_headless() {
        // An existing (even undecodable) file prerolls async; a missing file
        // fails PLAYING synchronously and must surface as Err, not panic.
        let dir = tempfile::TempDir::new().unwrap();
        let filler = dir.path().join("filler.mp4");
        std::fs::write(&filler, b"not a video").unwrap();
        let (playbin, _sink) =
            build_playbin_with_sink(&to_file_uri(&filler).unwrap(), "fakesink").expect("playbin");
        play(&playbin).expect("PLAYING an existing file must succeed headless");
        stop(&playbin);
        let (missing, _sink) =
            build_playbin_with_sink("file:///no/such/file.mp4", "fakesink").expect("playbin");
        assert!(
            play(&missing).is_err(),
            "missing file must fail PLAYING as Err"
        );
        stop(&missing);
    }

    #[test]
    fn build_playbin_sets_uri_mute_and_video_only_flags() {
        // fakesink keeps this headless-safe; gtk4paintablesink needs a display
        // for PLAYING but shares the same playbin wiring (covered by build_playbin).
        let (playbin, _sink) =
            build_playbin_with_sink("file:///tmp/clip.mp4", "fakesink").expect("playbin");
        let uri: String = playbin.property("uri");
        assert_eq!(uri, "file:///tmp/clip.mp4");
        let mute: bool = playbin.property("mute");
        assert!(mute, "video must be muted");
        let flags = playbin.property_value("flags");
        let text = flags
            .transform::<String>()
            .expect("flags serialize")
            .get::<String>()
            .expect("flags string");
        assert!(
            text.to_ascii_lowercase().contains("video"),
            "flags must be video-only, got: {text}"
        );
        assert!(
            !text.to_ascii_lowercase().contains("audio"),
            "audio flag must be off, got: {text}"
        );
    }

    #[test]
    fn build_playbin_rejects_non_file_uri() {
        let err = build_playbin_with_sink("https://example.com/x.mp4", "fakesink").unwrap_err();
        assert!(err.contains("non-file"), "unexpected: {err}");
        let err2 = build_playbin_with_sink("file:///tmp/x.mp4", "no-such-sink-xyz").unwrap_err();
        assert!(err2.contains("unavailable"), "unexpected: {err2}");
    }

    #[test]
    fn corrupt_file_posts_bus_error_not_crash() {
        let dir = tempfile::TempDir::new().unwrap();
        let corrupt = dir.path().join("corrupt.mp4");
        std::fs::write(&corrupt, b"this is not a video file, just garbage bytes").unwrap();
        let uri = to_file_uri(&corrupt).unwrap();
        let (playbin, _sink) = build_playbin_with_sink(&uri, "fakesink").expect("playbin");
        playbin.set_state(gst::State::Playing).expect("playing");
        let bus = playbin.bus().expect("bus");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut outcome = String::from("TIMEOUT: no Error posted for corrupt video");
        while std::time::Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                if let gst::MessageView::Error(err) = msg.view() {
                    outcome = format!("ERROR as expected: {}", err.error());
                    break;
                }
            }
        }
        stop(&playbin);
        assert!(
            outcome.starts_with("ERROR"),
            "corrupt video must post bus Error (placeholder+toast path), got: {outcome}"
        );
    }

    #[test]
    fn encoded_webm_decodes_to_eos() {
        ensure_init().unwrap();
        for name in ["videotestsrc", "vp8enc", "webmmux"] {
            if gst::ElementFactory::find(name).is_none() {
                eprintln!("SKIP: encoder element {name} missing in this env");
                return;
            }
        }
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("tiny.webm");        let loc = path.to_str().unwrap().to_string();
        // Encode 15 frames of test video headlessly.
        let pipe = gst::parse::launch(&format!(
            "videotestsrc num-buffers=15 ! video/x-raw,width=64,height=64,framerate=10/1 ! vp8enc ! webmmux ! filesink location={loc}"
        ))
        .expect("encode pipeline");
        pipe.set_state(gst::State::Playing).expect("encode");
        let bus = pipe.bus().expect("bus");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut encoded = false;
        while std::time::Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                match msg.view() {
                    gst::MessageView::Eos(..) => {
                        encoded = true;
                        break;
                    }
                    gst::MessageView::Error(e) => panic!("encode failed: {}", e.error()),
                    _ => {}
                }
            }
        }
        let _ = pipe.set_state(gst::State::Null);
        assert!(encoded, "fixture webm must encode");

        // Decode it through the same playbin wiring the Preview uses.
        let uri = to_file_uri(&path).unwrap();
        let (playbin, _sink) = build_playbin_with_sink(&uri, "fakesink").expect("playbin");
        playbin.set_state(gst::State::Playing).expect("playing");
        let bus = playbin.bus().expect("bus");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut outcome = String::from("TIMEOUT: valid webm never reached EOS");
        while std::time::Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                match msg.view() {
                    gst::MessageView::Eos(..) => {
                        outcome = String::from("EOS: valid video plays through");
                        break;
                    }
                    gst::MessageView::Error(e) => {
                        outcome = format!("unexpected decode Error: {}", e.error());
                        break;
                    }
                    _ => {}
                }
            }
        }
        stop(&playbin);
        assert!(
            outcome.starts_with("EOS"),
            "valid video must play to EOS (loop-seek path), got: {outcome}"
        );
    }

    #[test]
    fn apprun_hook_exports_bundled_plugin_paths() {
        let hook = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/AppRun"
        ))
        .expect("packaging/AppRun must exist (spec #20 AppRun hook)");
        for marker in [
            "GST_PLUGIN_SYSTEM_PATH",
            "$HERE/usr/lib/gstreamer-1.0",
            "GST_PLUGIN_SCANNER",
            "gst-plugin-scanner",
            "GLYCIN_DATA_DIR",
            "LD_LIBRARY_PATH",
            "gtk4paintablesink",
        ] {
            assert!(
                hook.contains(marker),
                "AppRun hook must set {marker} for in-AppImage discovery"
            );
        }
    }
}
