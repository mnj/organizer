use clap::Parser;
use gdk4::prelude::*;
use gstreamer as gst;
use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, Entry,
    Label, Orientation, Paned, Stack,
};
use organizer_lib::config::Action;
use organizer_lib::dedup::{compute_sha256, FileHash};
use organizer_lib::mover::{classify_file, mover_error_message};
use organizer_lib::preview::{ensure_sandbox_bwrap, is_glycin_supported, load_texture};
use organizer_lib::queue::build_snapshot;
use organizer_lib::store::Store;
use organizer_lib::undo::{push_undo_capped, undo_classification, UndoEntry};
use organizer_lib::video;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Parser, Debug)]
#[command(name = "organizer", version, about = "File-triaging desktop app")]
struct Args {
    #[arg(value_name = "SOURCE_FOLDER")]
    source_folder: Option<PathBuf>,

    /// Headless bundling probe for CI smoke (no window): prints
    /// GLYCIN_DATA_DIR/XDG_DATA_DIRS/GST_PLUGIN_SYSTEM_PATH, bwrap version,
    /// sandbox status and gtk4paintablesink availability, then exits.
    /// Used by packaging/smoke.sh and .github/workflows/ci.yml inside the
    /// dep-stripped ubuntu:22.04 container.
    #[arg(long = "self-test-sandbox", hide = true)]
    self_test_sandbox: bool,
}

/// Headless probe behind `--self-test-sandbox` (spec #21 CI smoke).
/// Runs before any GTK init so it works with no display.
fn run_self_test_sandbox() -> i32 {
    println!("organizer {}", env!("CARGO_PKG_VERSION"));
    for key in [
        "GLYCIN_DATA_DIR",
        "XDG_DATA_DIRS",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_SCANNER",
        "PATH",
    ] {
        println!(
            "{}={}",
            key,
            std::env::var(key).unwrap_or_else(|_| "(unset)".into())
        );
    }
    match std::process::Command::new("bwrap").arg("--version").output() {
        Ok(out) if out.status.success() => {
            print!("bwrap {}", String::from_utf8_lossy(&out.stdout));
        }
        _ => println!("bwrap missing"),
    }
    match organizer_lib::preview::ensure_sandbox_bwrap() {
        Ok(()) => println!("sandbox: Bwrap ok"),
        Err(e) => println!("sandbox: unavailable ({e})"),
    }
    match organizer_lib::video::ensure_init() {
        Ok(()) => {
            if gstreamer::ElementFactory::find(organizer_lib::video::PAINTABLE_SINK).is_some() {
                println!("gtk4paintablesink: available");
            } else {
                println!("gtk4paintablesink: missing (bundle libgstgtk4.so)");
            }
        }
        Err(e) => println!("gstreamer: unavailable ({e})"),
    }
    // Loader conf visibility (what glycin would scan).
    let data_dir =
        std::env::var("GLYCIN_DATA_DIR").unwrap_or_else(|_| "/usr/share".into());
    let mut found = false;
    for base in [data_dir, std::env::var("XDG_DATA_DIRS").unwrap_or_default()] {
        for part in base.split(':') {
            let conf = std::path::Path::new(part)
                .join("glycin-loaders")
                .join("2+")
                .join("conf.d");
            if conf.is_dir() {
                println!("glycin conf.d: {}", conf.display());
                found = true;
            }
        }
    }
    if !found {
        println!("glycin conf.d: not found");
    }
    0
}

fn css() -> CssProvider {
    let p = CssProvider::new();
    p.load_from_data(
        r#"
        .preview-area { background-color: #1e1e2e; }
        .preview-placeholder { background-color: #1e1e2e; }
        .unsupported-badge {
            background: #fab387; color: #1e1e2e;
            border-radius: 8px; padding: 2px 8px;
            font-size: 11px; font-weight: 700;
        }
        .duplicate-badge {
            background: #f38ba8; color: #1e1e2e;
            border-radius: 8px; padding: 2px 8px;
            font-size: 11px; font-weight: 700;
        }
        .index-label { font-weight: 600; }
        .toast {
            background: #313244; color: #cdd6f4;
            border-radius: 10px; padding: 8px 14px;
        }
        .shortcut-badge {
            background: alpha(@accent_bg_color, 0.2);
            border-radius: 4px; padding: 0 6px;
            font-size: 11px; font-weight: 700;
        }
        .action-btn { padding: 8px 14px; font-weight: 600; }
        .error { color: #f38ba8; }
        entry.error { border-color: #f38ba8; }
        "#,
    );
    p
}

fn is_entry_focused(window: &ApplicationWindow) -> bool {
    if let Some(focus) = gtk4::prelude::GtkWindowExt::focus(window) {
        if focus.is::<Entry>() || focus.is::<gtk4::Text>() || focus.is::<gtk4::SearchEntry>() {
            return true;
        }
        let mut parent = focus.parent();
        while let Some(p) = parent {
            if p.is::<Entry>() {
                return true;
            }
            parent = p.parent();
        }
    }
    false
}

use organizer_lib::settings::{open_settings, SettingsContext};

/// Shared decode-failure widgets for the Preview: error placeholder + toast.
/// One `show` method so glycin stills, video startup, and video bus errors
/// cannot drift apart (same placeholder + toast, Main stays alive).
#[derive(Clone)]
struct PreviewErrorUi<F: Fn(String) + Clone> {
    stack: Stack,
    spinner: gtk4::Spinner,
    error_name: Label,
    error_detail: Label,
    toast: F,
}

impl<F: Fn(String) + Clone> PreviewErrorUi<F> {
    fn show(&self, name: &str, msg: String) {
        tracing::warn!("preview failed for {name}: {msg}");
        self.error_name.set_text(name);
        self.error_detail.set_text(&msg);
        self.stack.set_visible_child_name("error");
        self.spinner.set_visible(false);
        self.spinner.set_spinning(false);
        (self.toast)(msg);
    }
}

/// Current File context for video playback (path + display name travel together).
#[derive(Clone)]
struct VideoTarget {
    path: PathBuf,
    name: String,
}

/// Centralized busy-state helper (Duplicated Code / Repeated Switches fix).
/// Single place to toggle spinner and Action Bar sensitivity.
fn set_busy_state(
    busy: &Rc<RefCell<bool>>,
    spinner: &gtk4::Spinner,
    actions_row: &GtkBox,
    btn_prev: &Button,
    btn_next: &Button,
    is_busy: bool,
) {
    *busy.borrow_mut() = is_busy;
    spinner.set_visible(is_busy);
    spinner.set_spinning(is_busy);
    let mut child = actions_row.first_child();
    while let Some(c) = child {
        if let Some(btn) = c.downcast_ref::<Button>() {
            btn.set_sensitive(!is_busy);
        }
        child = c.next_sibling();
    }
    // Prev/Next sensitivity is restored by update_ui; here we just force false when busy
    if is_busy {
        btn_prev.set_sensitive(false);
        btn_next.set_sensitive(false);
    }
}

fn guard_busy(busy: &Rc<RefCell<bool>>) -> bool {
    *busy.borrow()
}

fn advance_idx(idx: &Rc<RefCell<usize>>, len: usize) {
    let mut v = idx.borrow_mut();
    if *v + 1 < len {
        *v += 1;
    } else if *v + 1 == len {
        *v += 1;
    }
}

fn make_nav_button(icon: &str, tooltip: &str, label: Option<&str>) -> Button {
    let b = if let Some(l) = label {
        Button::builder().label(l).icon_name(icon).tooltip_text(tooltip).build()
    } else {
        Button::builder().icon_name(icon).tooltip_text(tooltip).build()
    };
    b.set_sensitive(false);
    b
}

fn make_placeholder(
    icon_markup: &str,
    name: &Label,
    badge_text: &str,
    badge_class: &str,
    detail: Option<&Label>,
) -> GtkBox {
    let bx = GtkBox::new(Orientation::Vertical, 12);
    bx.set_halign(gtk4::Align::Center);
    bx.set_valign(gtk4::Align::Center);
    let ic = Label::new(None);
    ic.set_markup(icon_markup);
    let badge = Label::new(Some(badge_text));
    badge.add_css_class(badge_class);
    bx.append(&ic);
    bx.append(name);
    bx.append(&badge);
    if let Some(d) = detail {
        bx.append(d);
    }
    bx
}

fn preload_next_preview(
    cache: &Rc<RefCell<HashMap<PathBuf, gdk::Texture>>>,
    snapshot: &Rc<Vec<PathBuf>>,
    idx: &Rc<RefCell<usize>>,
) {
    let cur = *idx.borrow();
    if cur + 1 < snapshot.len() {
        let nxt = snapshot[cur + 1].clone();
        if organizer_lib::preview::is_glycin_supported(&nxt)
            && !cache.borrow().contains_key(&nxt)
            && nxt.exists()
        {
            let cache_clone = cache.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(t) = organizer_lib::preview::load_texture(&nxt, None).await {
                    cache_clone.borrow_mut().insert(nxt.clone(), t);
                }
            });
        }
    }
}

/// Hash preloading service (Divergent Change fix) — caches sha256 for Current + next.
/// Uses background `std::thread` with `Arc<Mutex>` cache to avoid blocking UI.
/// Falls back to synchronous when needed (e.g. immediate hash on press).
struct HashService {
    cache: Arc<Mutex<HashMap<PathBuf, String>>>,
    snapshot: Rc<Vec<PathBuf>>,
    idx: Rc<RefCell<usize>>,
}

impl HashService {
    fn new(cache: Arc<Mutex<HashMap<PathBuf, String>>>, snapshot: Rc<Vec<PathBuf>>, idx: Rc<RefCell<usize>>) -> Self {
        Self { cache, snapshot, idx }
    }
    fn get_cached(&self, path: &PathBuf) -> Option<String> {
        self.cache.lock().ok()?.get(path).cloned()
    }
    fn remove(&self, path: &PathBuf) {
        if let Ok(mut m) = self.cache.lock() {
            m.remove(path);
        }
    }
    fn pending_targets(&self) -> Vec<PathBuf> {
        let i = *self.idx.borrow();
        let len = self.snapshot.len();
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        if i < len {
            let cur = self.snapshot[i].clone();
            if !cache.contains_key(&cur) && cur.exists() {
                out.push(cur);
            }
        }
        if i + 1 < len {
            let nxt = self.snapshot[i + 1].clone();
            if !cache.contains_key(&nxt) && nxt.exists() {
                out.push(nxt);
            }
        }
        out
    }
    #[allow(dead_code)]
    /// Synchronous pre-hash of current and next (used in tests).
    fn prehash_sync(&self) {
        for p in self.pending_targets() {
            if let Ok(h) = compute_sha256(&p) {
                let mut m = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                m.insert(p, h.to_ascii_lowercase());
            }
        }
    }
    fn prehash_async(&self) {
        for p in self.pending_targets() {
            let cache_c = self.cache.clone();
            std::thread::spawn(move || {
                if let Ok(h) = compute_sha256(&p) {
                    if let Ok(mut m) = cache_c.lock() {
                        m.insert(p, h.to_ascii_lowercase());
                    } else {
                        // recover from poisoned lock
                        let mut m = cache_c.lock().unwrap_or_else(|e| e.into_inner());
                        m.insert(p, h.to_ascii_lowercase());
                    }
                }
            });
        }
    }
    fn prehash_next(&self) {
        // Single entry point for callers; delegates to async background
        self.prehash_async();
    }
}

/// Shared Organizer Database error window (Duplicated Code fix).
/// Single place for `Store::open` / `store.actions` failures in `build_shell`.
fn show_database_error_window(app: &Application, message: String) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Organizer — database error")
        .default_width(500)
        .default_height(120)
        .build();
    window.maximize();
    let lbl = Label::new(Some(&message));
    window.set_child(Some(&lbl));
    window.present();
}

fn build_shell(app: &Application, snapshot: Vec<PathBuf>, source_folder: PathBuf) {
    let provider = css();
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // Enforce sandboxed preview (Bwrap) — refuse with dialog if missing/blocked
    if let Err(msg) = ensure_sandbox_bwrap() {
        let dialog = ApplicationWindow::builder()
            .application(app)
            .title("Organizer — Sandbox unavailable")
            .default_width(600)
            .default_height(160)
            .modal(true)
            .build();
        dialog.maximize();
        let vbox = GtkBox::new(Orientation::Vertical, 12);
        vbox.set_margin_top(24);
        vbox.set_margin_bottom(24);
        vbox.set_margin_start(24);
        vbox.set_margin_end(24);
        vbox.set_halign(gtk4::Align::Center);
        vbox.set_valign(gtk4::Align::Center);
        let title = Label::new(Some(&msg));
        title.add_css_class("title-2");
        title.set_wrap(true);
        let detail = Label::new(Some(
            "Preview requires bubblewrap ≥0.8 and unprivileged user namespaces. Install with:\n\
             Debian/Ubuntu: sudo apt install bubblewrap libseccomp2 glycin-loaders\n\
             Fedora: sudo dnf install bubblewrap libseccomp glycin-loaders\n\
             Arch: sudo pacman -S bubblewrap libseccomp glycin",
        ));
        detail.set_wrap(true);
        detail.add_css_class("dim-label");
        let btn = Button::with_label("Close");
        let dlg_clone = dialog.clone();
        btn.connect_clicked(move |_| dlg_clone.close());
        vbox.append(&title);
        vbox.append(&detail);
        vbox.append(&btn);
        dialog.set_child(Some(&vbox));
        dialog.present();
        tracing::warn!("sandbox unavailable: {}", msg);
        return;
    }

    // Open or seed the portable Organizer Database (#24 Classification source).
    let store = match Store::open(&source_folder) {
        Ok(s) => s,
        Err(e) => {
            show_database_error_window(app, format!("Failed to open {}: {}", source_folder.join("organizer.db").display(), e));
            return;
        }
    };
    let initial_actions = match store.actions() {
        Ok(a) => a,
        Err(e) => {
            show_database_error_window(app, format!("Failed to load Actions: {e}"));
            return;
        }
    };
    let live_actions: Rc<RefCell<Vec<Action>>> = Rc::new(RefCell::new(initial_actions.clone()));
    let disk_actions: Rc<RefCell<Vec<Action>>> = Rc::new(RefCell::new(initial_actions));

    let hash_cache: Arc<Mutex<HashMap<PathBuf, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let busy: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let undo_stack: Rc<RefCell<Vec<UndoEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let redo_stack: Rc<RefCell<Vec<UndoEntry>>> = Rc::new(RefCell::new(Vec::new()));

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Organizer")
        .default_width(1280)
        .default_height(800)
        .build();
    window.maximize();

    let overlay = gtk4::Overlay::new();

    let paned = Paned::new(Orientation::Vertical);
    paned.set_wide_handle(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    paned.set_position(640);
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(false);

    // Preview
    let preview_box = GtkBox::new(Orientation::Vertical, 0);
    preview_box.add_css_class("preview-area");
    preview_box.set_hexpand(true);
    preview_box.set_vexpand(true);

    let stack = Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    stack.set_transition_duration(150);

    let file_label = Label::new(None);
    file_label.add_css_class("title-2");
    file_label.set_wrap(true);
    file_label.set_justify(gtk4::Justification::Center);
    file_label.set_halign(gtk4::Align::Center);
    file_label.set_valign(gtk4::Align::Center);

    let picture = gtk4::Picture::new();
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_halign(gtk4::Align::Fill);
    picture.set_valign(gtk4::Align::Fill);

    let preview_center = GtkBox::new(Orientation::Vertical, 8);
    preview_center.set_halign(gtk4::Align::Center);
    preview_center.set_valign(gtk4::Align::Center);
    preview_center.set_hexpand(true);
    preview_center.set_vexpand(true);
    let icon = Label::new(None);
    icon.set_markup(r#"<span size="60000">🖼️</span>"#);
    let file_overlay = gtk4::Overlay::new();
    file_overlay.set_hexpand(true);
    file_overlay.set_vexpand(true);
    file_overlay.set_child(Some(&picture));
    file_overlay.add_overlay(&preview_center);
    preview_center.append(&icon);
    preview_center.append(&file_label);

    // Spinner overlay on Preview (busy indicator)
    let spinner = gtk4::Spinner::new();
    spinner.set_halign(gtk4::Align::Center);
    spinner.set_valign(gtk4::Align::Center);
    spinner.set_size_request(48, 48);
    spinner.set_visible(false);
    file_overlay.add_overlay(&spinner);

    let ph_name = Label::new(None);
    ph_name.add_css_class("title-3");
    let ph_box = make_placeholder(
        r#"<span size="50000">📄</span>"#,
        &ph_name,
        "Unsupported",
        "unsupported-badge",
        None,
    );
    let error_name = Label::new(None);
    error_name.add_css_class("title-3");
    error_name.set_wrap(true);
    let error_detail = Label::new(Some("image-missing — loader error, file skipped"));
    error_detail.add_css_class("dim-label");
    error_detail.set_wrap(true);
    let error_box = make_placeholder(
        r#"<span size="50000">🖼️</span>"#,
        &error_name,
        "Decode failed",
        "duplicate-badge",
        Some(&error_detail),
    );

    let empty_box = GtkBox::new(Orientation::Vertical, 12);
    empty_box.set_halign(gtk4::Align::Center);
    empty_box.set_valign(gtk4::Align::Center);
    let empty_icon = Label::new(None);
    empty_icon.set_markup(r#"<span size="50000">✅</span>"#);
    let empty_label = Label::new(None);
    empty_label.add_css_class("title-3");
    let empty_btn = Button::with_label("Open another folder");
    empty_box.append(&empty_icon);
    empty_box.append(&empty_label);
    empty_box.append(&empty_btn);

    stack.add_named(&file_overlay, Some("file"));
    stack.add_named(&ph_box, Some("unsupported"));
    stack.add_named(&error_box, Some("error"));
    stack.add_named(&empty_box, Some("empty"));
    preview_box.append(&stack);
    paned.set_start_child(Some(&preview_box));

    // Action Bar ~140px
    let action_bar = GtkBox::new(Orientation::Vertical, 8);
    action_bar.set_margin_top(10);
    action_bar.set_margin_bottom(10);
    action_bar.set_margin_start(12);
    action_bar.set_margin_end(12);

    let nav_row = GtkBox::new(Orientation::Horizontal, 8);
    let btn_prev = Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Previous (← / p)")
        .build();
    let btn_next = Button::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text("Next (→ / n / Space)")
        .build();
    let index_label = Label::new(Some("—"));
    index_label.add_css_class("index-label");
    let dup_badge = Label::new(Some("Duplicate"));
    dup_badge.add_css_class("duplicate-badge");
    dup_badge.set_visible(false);
    let sep = gtk4::Separator::new(Orientation::Vertical);
    let btn_undo = make_nav_button("edit-undo-symbolic", "Undo (Ctrl+Z)", Some("Undo"));
    let btn_redo = make_nav_button("edit-redo-symbolic", "Redo (Ctrl+Shift+Z / Ctrl+Y)", Some("Redo"));
    let btn_settings = Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings (Ctrl+,)")
        .build();
    btn_settings.add_css_class("circular");

    nav_row.append(&btn_prev);
    nav_row.append(&btn_next);
    nav_row.append(&btn_undo);
    nav_row.append(&btn_redo);
    nav_row.append(&sep);
    nav_row.append(&index_label);
    nav_row.append(&dup_badge);
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    nav_row.append(&spacer);
    nav_row.append(&btn_settings);
    action_bar.append(&nav_row);

    // Centered Action buttons row
    let actions_row = GtkBox::new(Orientation::Horizontal, 10);
    actions_row.set_halign(gtk4::Align::Center);
    actions_row.set_hexpand(true);
    action_bar.append(&actions_row);

    let hint = Label::new(None);
    hint.add_css_class("dim-label");
    hint.set_halign(gtk4::Align::Center);
    action_bar.append(&hint);

    let wrapper = GtkBox::new(Orientation::Vertical, 0);
    wrapper.set_size_request(-1, 140);
    wrapper.append(&action_bar);
    paned.set_end_child(Some(&wrapper));

    overlay.set_child(Some(&paned));

    let toast = Label::new(None);
    toast.add_css_class("toast");
    toast.set_halign(gtk4::Align::Center);
    toast.set_valign(gtk4::Align::End);
    toast.set_margin_bottom(24);
    toast.set_visible(false);
    overlay.add_overlay(&toast);
    window.set_child(Some(&overlay));

    let snapshot_rc = Rc::new(snapshot);
    let idx: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));
    let current_override: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    // Preview caches: texture cache and in-flight cancellable (Bwrap glycin)
    let preview_cache: Rc<RefCell<HashMap<PathBuf, gdk::Texture>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let preview_cancellable: Rc<RefCell<Option<gio::Cancellable>>> =
        Rc::new(RefCell::new(None));

    let show_toast = {
        let t = toast.clone();
        move |msg: String| {
            t.set_text(&msg);
            t.set_visible(true);
            let tt = t.clone();
            glib::timeout_add_seconds_local(3, move || {
                tt.set_visible(false);
                glib::ControlFlow::Break
            });
        }
    };

    // Video Preview state (spec #20): the currently playing video, if any.
    // Clearing the slot stops playback and removes the bus watch, so file
    // switches never leave a pipeline behind.
    let video_state: Rc<RefCell<Option<video::PlayingVideo>>> =
        Rc::new(RefCell::new(None));

    let stop_video: Rc<dyn Fn()> = {
        let video_state_c = video_state.clone();
        Rc::new(move || {
            if let Some(playing) = video_state_c.borrow_mut().take() {
                playing.stop();
            }
        })
    };

    let error_ui = PreviewErrorUi {
        stack: stack.clone(),
        spinner: spinner.clone(),
        error_name: error_name.clone(),
        error_detail: error_detail.clone(),
        toast: show_toast.clone(),
    };

    // Start video playback for the Current File in the same Preview Picture
    // used for glycin stills. Muted + video-only + auto-play loop (EOS seeks
    // to zero); bus Error shows the shared decode-failure placeholder + toast
    // and the Main stays alive.
    let play_video: Rc<dyn Fn(VideoTarget)> = {
        let picture_c = picture.clone();
        let stack_c = stack.clone();
        let spinner_c = spinner.clone();
        let preview_center_c = preview_center.clone();
        let error_ui_c = error_ui.clone();
        let video_state_c = video_state.clone();
        let stop_video_c = stop_video.clone();
        Rc::new(move |target: VideoTarget| {
            stop_video_c();
            let VideoTarget { path, name } = target;
            // Single failure helper: an optional half-built pipeline is torn
            // down first so no late bus message can overwrite the placeholder.
            let fail = |pipeline: Option<&gst::Element>, msg: String| {
                if let Some(p) = pipeline {
                    video::stop(p);
                }
                error_ui_c.show(&name, msg);
            };
            if let Err(e) = video::ensure_init() {
                fail(None, format!("Video unavailable for {name}: {e}"));
                return;
            }
            let uri = match video::to_file_uri(&path) {
                Ok(u) => u,
                Err(e) => {
                    fail(None, format!("Video failed for {name}: {e}"));
                    return;
                }
            };
            let (pipeline, sink) = match video::build_playbin(&uri) {
                Ok(t) => t,
                Err(e) => {
                    fail(None, format!("Video failed for {name}: {e}"));
                    return;
                }
            };
            let paintable = match video::sink_paintable(&sink) {
                Ok(p) => p,
                Err(e) => {
                    fail(Some(&pipeline), format!("Video failed for {name}: {e}"));
                    return;
                }
            };
            // Same Picture as stills: swap its Paintable to the sink's.
            // Contain + dark letterbox come from the existing Picture/CSS.
            picture_c.set_paintable(Some(&paintable));
            preview_center_c.set_visible(false);
            stack_c.set_visible_child_name("file");
            spinner_c.set_visible(false);
            spinner_c.set_spinning(false);
            // Watch the bus before PLAYING so early decode errors are caught.
            let weak_err_c = pipeline.downgrade();
            let weak_eos_c = pipeline.downgrade();
            let video_state_c2 = video_state_c.clone();
            let error_ui_c2 = error_ui_c.clone();
            let error_ui_c3 = error_ui_c.clone();
            let name_c = name.clone();
            let name_c2 = name.clone();
            let name_c3 = name.clone();
            let watch = video::watch_bus(
                &pipeline,
                move |msg| {
                    if let Some(p) = weak_err_c.upgrade() {
                        video::stop(&p);
                    }
                    *video_state_c2.borrow_mut() = None;
                    error_ui_c2.show(&name_c, format!("Video failed for {name_c}: {msg}"));
                },
                move || {
                    // Loop failure is surfaced, never a silent stall: a
                    // non-seekable container that refuses the restart shows
                    // the shared placeholder + toast.
                    if let Some(p) = weak_eos_c.upgrade() {
                        if let Err(e) = video::restart(&p) {
                            video::stop(&p);
                            error_ui_c3.show(&name_c3, format!("Video loop failed for {name_c3}: {e}"));
                        }
                    }
                },
            );
            let guard = match watch {
                Ok(g) => g,
                Err(e) => {
                    fail(Some(&pipeline), format!("Video failed for {name}: {e}"));
                    return;
                }
            };
            if let Err(e) = video::play(&pipeline) {
                drop(guard);
                fail(
                    Some(&pipeline),
                    format!("Video failed for {name_c2}: {e}"),
                );
                return;
            }
            *video_state_c.borrow_mut() = Some(video::PlayingVideo::new(pipeline, guard));
        })
    };

    let update_ui = {
        let idx_c = idx.clone();
        let snap_c = snapshot_rc.clone();
        let stack_c = stack.clone();
        let file_label_c = file_label.clone();
        let index_label_c = index_label.clone();
        let btn_prev_c = btn_prev.clone();
        let btn_next_c = btn_next.clone();
        let btn_undo_c = btn_undo.clone();
        let btn_redo_c = btn_redo.clone();
        let empty_label_c = empty_label.clone();
        let hint_c = hint.clone();
        let source_c = source_folder.clone();
        let busy_c = busy.clone();
        let actions_row_c = actions_row.clone();
        let undo_c = undo_stack.clone();
        let redo_c = redo_stack.clone();
        let override_c = current_override.clone();
        // Preview widgets and caches — bundled to reduce Data Clumps
        let picture_c = picture.clone();
        let spinner_c = spinner.clone();
        let preview_center_c = preview_center.clone();
        let icon_c = icon.clone();
        let ph_name_c = ph_name.clone();
        let error_name_c = error_name.clone();
        let error_detail_c = error_detail.clone();
        let preview_cache_c = preview_cache.clone();
        let preview_cancellable_c = preview_cancellable.clone();
        let show_toast_c = show_toast.clone();
        let stop_video_c = stop_video.clone();
        let play_video_c = play_video.clone();
        let refresh_undo_redo = {
            let btn_undo_c = btn_undo_c.clone();
            let btn_redo_c = btn_redo_c.clone();
            let undo_c = undo_c.clone();
            let redo_c = redo_c.clone();
            let busy_c = busy_c.clone();
            move || {
                let is_busy = *busy_c.borrow();
                btn_undo_c.set_sensitive(!undo_c.borrow().is_empty() && !is_busy);
                btn_redo_c.set_sensitive(!redo_c.borrow().is_empty() && !is_busy);
            }
        };
        move || {
            let len = snap_c.len();
            if len == 0 {
                stack_c.set_visible_child_name("empty");
                empty_label_c.set_text(&format!(
                    "All triaged — {} files sorted\n{}",
                    len,
                    source_c.display()
                ));
                index_label_c.set_text("—");
                btn_prev_c.set_sensitive(false);
                btn_next_c.set_sensitive(false);
                refresh_undo_redo();
                hint_c.set_text("");
                return;
            }
            let i = *idx_c.borrow();
            // If override is set (undo with _undo suffix), show that file instead of snapshot
            let (effective_path, effective_name) = if let Some(ov) = override_c.borrow().clone() {
                if ov.exists() {
                    let n = ov.file_name().and_then(|x| x.to_str()).unwrap_or("—").to_string();
                    (ov, n)
                } else {
                    let p = snap_c[i.min(len - 1)].clone();
                    let n = p.file_name().and_then(|x| x.to_str()).unwrap_or("—").to_string();
                    (p, n)
                }
            } else {
                let p = snap_c[i.min(len - 1)].clone();
                let n = p.file_name().and_then(|x| x.to_str()).unwrap_or("—").to_string();
                (p, n)
            };
            let name = effective_name.clone();
            let effective_path_clone = effective_path.clone();
            // If i >= len, we have triaged past end: show empty
            if i >= len {
                stop_video_c();
                stack_c.set_visible_child_name("empty");
                empty_label_c.set_text(&format!(
                    "All triaged — {} files sorted\n{}",
                    len,
                    source_c.display()
                ));
                index_label_c.set_text(&format!("{len} / {len} — done"));
                let is_busy = *busy_c.borrow();
                btn_prev_c.set_sensitive(len > 0 && !is_busy);
                btn_next_c.set_sensitive(false);
                refresh_undo_redo();
                hint_c.set_text("");
                // clear preview
                picture_c.set_paintable(None::<&gdk::Paintable>);
                preview_center_c.set_visible(true);
                spinner_c.set_visible(false);
                spinner_c.set_spinning(false);
                return;
            }
            index_label_c.set_text(&format!("{} / {} — {}", i + 1, len, name));
            let is_busy = *busy_c.borrow();
            btn_prev_c.set_sensitive(i > 0 && !is_busy);
            btn_next_c.set_sensitive(i + 1 < len && !is_busy);
            refresh_undo_redo();
            // disable action buttons while busy
            let mut child = actions_row_c.first_child();
            while let Some(c) = child {
                if let Some(btn) = c.downcast_ref::<Button>() {
                    btn.set_sensitive(!is_busy);
                }
                child = c.next_sibling();
            }
            if is_busy {
                hint_c.set_text("Working…");
            } else {
                hint_c.set_text("Press 1-9 or Ctrl+1-9 to triage");
            }
            // Preview handling: sandboxed glycin for stills/animated (spec #19),
            // GStreamer playbin + gtk4paintablesink into the same Picture for
            // video (spec #20).
            if is_glycin_supported(&effective_path_clone) {
                stop_video_c();
                // cancel previous preview load if in flight - prevents stale-frame race on rapid Next/Prev
                if let Some(prev) = preview_cancellable_c.borrow().as_ref() {
                    prev.cancel();
                }
                if let Some(tex) = preview_cache_c.borrow().get(&effective_path_clone).cloned() {
                    picture_c.set_paintable(Some(&tex));
                    preview_center_c.set_visible(false);
                    stack_c.set_visible_child_name("file");
                    spinner_c.set_visible(false);
                    spinner_c.set_spinning(false);
                    preload_next_preview(&preview_cache_c, &snap_c, &idx_c);
                } else {
                    let cancellable = gio::Cancellable::new();
                    *preview_cancellable_c.borrow_mut() = Some(cancellable.clone());
                    file_label_c.set_text(&name);
                    icon_c.set_markup(r#"<span size="60000">🖼️</span>"#);
                    preview_center_c.set_visible(true);
                    picture_c.set_paintable(None::<&gdk::Paintable>);
                    stack_c.set_visible_child_name("file");
                    spinner_c.set_visible(true);
                    spinner_c.set_spinning(true);
                    let path_for_async = effective_path_clone.clone();
                    let picture_async = picture_c.clone();
                    let stack_async = stack_c.clone();
                    let spinner_async = spinner_c.clone();
                    let preview_center_async = preview_center_c.clone();
                    let preview_cache_async = preview_cache_c.clone();
                    let preview_cancellable_async = preview_cancellable_c.clone();
                    let error_name_async = error_name_c.clone();
                    let error_detail_async = error_detail_c.clone();
                    let show_toast_async = show_toast_c.clone();
                    let name_async = name.clone();
                    let snap_for_preload = snap_c.clone();
                    let idx_for_preload = idx_c.clone();
                    glib::MainContext::default().spawn_local(async move {
                        let res = load_texture(&path_for_async, Some(&cancellable)).await;
                        if cancellable.is_cancelled() {
                            return;
                        }
                        *preview_cancellable_async.borrow_mut() = None;
                        spinner_async.set_visible(false);
                        spinner_async.set_spinning(false);
                        match res {
                            Ok(tex) => {
                                preview_cache_async
                                    .borrow_mut()
                                    .insert(path_for_async.clone(), tex.clone());
                                picture_async.set_paintable(Some(&tex));
                                preview_center_async.set_visible(false);
                                stack_async.set_visible_child_name("file");
                                preload_next_preview(
                                    &preview_cache_async,
                                    &snap_for_preload,
                                    &idx_for_preload,
                                );
                            }
                            Err(e) => {
                                // Loader crash isolated via bwrap sandbox (RemoteError::Panic handled as generic Err)
                                tracing::warn!(
                                    "glycin decode failed for {}: {}",
                                    path_for_async.display(),
                                    e
                                );
                                error_name_async.set_text(&name_async);
                                error_detail_async.set_text(&format!("{} — loader error", e));
                                stack_async.set_visible_child_name("error");
                                preview_center_async.set_visible(true);
                                show_toast_async(format!(
                                    "Decode failed for {}: {}",
                                    name_async, e
                                ));
                            }
                        }
                    });
                }
            } else if video::is_video_supported(&effective_path_clone) {
                // Video via GStreamer into the same Picture (muted, looped).
                play_video_c(VideoTarget {
                    path: effective_path_clone.clone(),
                    name: name.clone(),
                });
            } else {
                stop_video_c();
                ph_name_c.set_text(&name);
                stack_c.set_visible_child_name("unsupported");
                picture_c.set_paintable(None::<&gdk::Paintable>);
                preview_center_c.set_visible(true);
                spinner_c.set_visible(false);
                spinner_c.set_spinning(false);
            }
        }
    };

    let hash_service = Rc::new(HashService::new(hash_cache.clone(), snapshot_rc.clone(), idx.clone()));
    let prehash_next: Rc<dyn Fn()> = {
        let hs = hash_service.clone();
        Rc::new(move || hs.prehash_next())
    };

    // Trigger move for a given Action — uses HashService + set_busy_state + background hash
    let trigger_move: Rc<dyn Fn(Action)> = {
        let idx_c = idx.clone();
        let snap_c = snapshot_rc.clone();
        let store_c = store.clone();
        let hash_service_c = hash_service.clone();
        let busy_c = busy.clone();
        let spinner_c = spinner.clone();
        let actions_row_c = actions_row.clone();
        let btn_prev_c = btn_prev.clone();
        let btn_next_c = btn_next.clone();
        let show_toast_c = show_toast.clone();
        let update_ui_c = update_ui.clone();
        let prehash_c = prehash_next.clone();
        let live_actions_c = live_actions.clone();
        let undo_stack_c = undo_stack.clone();
        let redo_stack_c = redo_stack.clone();
        let override_c = current_override.clone();
        let stop_video_c = stop_video.clone();
        Rc::new(move |action: Action| {
            if guard_busy(&busy_c) {
                return;
            }
            let len = snap_c.len();
            if len == 0 {
                return;
            }
            let i = *idx_c.borrow();
            if i >= len {
                return;
            }
            let current = snap_c[i].clone();
            if !current.exists() {
                show_toast_c(format!("File already moved: {}", current.display()));
                if i + 1 < len {
                    *idx_c.borrow_mut() += 1;
                    update_ui_c();
                    prehash_c();
                }
                return;
            }
            // Stop playback before hashing/moving: the file is renamed away
            // while PLAYING would otherwise post a late bus Error that
            // overwrites the success toast/placeholder.
            stop_video_c();
            set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, true);
            update_ui_c();

            let folder_name = action.folder_name.clone();
            let display_name = action.display_name.clone();
            let file_name = current.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();

            // Shared continuation for move after hash is ready (sync or async)
            let do_move: Rc<dyn Fn(String)> = {
                let idx_c = idx_c.clone();
                let snap_c = snap_c.clone();
                let store_c = store_c.clone();
                let live_actions_c = live_actions_c.clone();
                let busy_c = busy_c.clone();
                let spinner_c = spinner_c.clone();
                let actions_row_c = actions_row_c.clone();
                let btn_prev_c = btn_prev_c.clone();
                let btn_next_c = btn_next_c.clone();
                let show_toast_c = show_toast_c.clone();
                let update_ui_c = update_ui_c.clone();
                let prehash_c = prehash_c.clone();
                let undo_stack_c = undo_stack_c.clone();
                let redo_stack_c = redo_stack_c.clone();
                let override_c = override_c.clone();
                let folder_name = folder_name.clone();
                let display_name = display_name.clone();
                let file_name = file_name.clone();
                let current = current.clone();
                Rc::new(move |hash: String| {
                    let validated = match FileHash::new(&hash) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!("invalid hash for {}: {}", file_name, e);
                            show_toast_c(format!("Failed to hash {}: invalid hash", file_name));
                            set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, false);
                            update_ui_c();
                            return;
                        }
                    };
                    let hash_lower = validated.as_str().to_string();
                    // Single Classification call: the outcome names duplicate
                    // routing, so no pre-check contains/origin roundtrip here.
                    match classify_file(&store_c, &current, &folder_name, &validated) {
                        Ok(outcome) => {
                            let was_duplicate = outcome.was_duplicate();
                            let dest = outcome.into_dest();
                            // push onto undo stack capped to min(50, queue len), clear redo and override
                            {
                                let cap = snap_c.len();
                                let entry = UndoEntry::new(&hash_lower, &file_name, dest.clone(), was_duplicate, &folder_name, &display_name);
                                push_undo_capped(&mut undo_stack_c.borrow_mut(), entry, cap);
                                redo_stack_c.borrow_mut().clear();
                            }
                            // clear any suffix override on normal move
                            *override_c.borrow_mut() = None;
                            if was_duplicate {
                                // Single origin lookup post-move for the toast.
                                let origin_folder = store_c.origin(&hash_lower).unwrap_or(None);
                                let origin_display = origin_folder.as_ref().and_then(|folder| {
                                    live_actions_c.borrow().iter().find(|a| &a.folder_name == folder).map(|a| a.display_name.clone())
                                }).or(origin_folder).unwrap_or_else(|| display_name.clone());
                                show_toast_c(format!("Duplicate — already in ‘{}’ — moved to duplicate/{}", origin_display, dest.file_name().and_then(|n| n.to_str()).unwrap_or(&file_name)));
                            } else {
                                show_toast_c(format!("Moved {} → {}/{} ", file_name, display_name, dest.file_name().and_then(|n| n.to_str()).unwrap_or("")));
                            }
                            advance_idx(&idx_c, snap_c.len());
                            set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, false);
                            update_ui_c();
                            prehash_c();
                        }
                        Err(e) => {
                            show_toast_c(mover_error_message(&e, &file_name));
                            set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, false);
                            update_ui_c();
                        }
                    }
                })
            };

            if let Some(cached) = hash_service_c.get_cached(&current) {
                hash_service_c.remove(&current);
                do_move(cached);
            } else {
                // Hash not yet ready — compute in background without blocking UI (spinner already visible)
                let current_clone = current.clone();
                let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
                let rx_cell = Rc::new(RefCell::new(rx));
                let rx_cell_clone = rx_cell.clone();
                let do_move_clone = do_move.clone();
                let show_toast_err = show_toast_c.clone();
                let busy_c2 = busy_c.clone();
                let spinner_c2 = spinner_c.clone();
                let actions_row_c2 = actions_row_c.clone();
                let btn_prev_c2 = btn_prev_c.clone();
                let btn_next_c2 = btn_next_c.clone();
                let update_ui_c2 = update_ui_c.clone();
                let file_name_err = file_name.clone();
                // Polling idle on main thread — checks channel without requiring Send closure capture
                glib::idle_add_local(move || {
                    match rx_cell_clone.borrow_mut().try_recv() {
                        Ok(Ok(hash)) => {
                            do_move_clone(hash);
                            glib::ControlFlow::Break
                        }
                        Ok(Err(err_msg)) => {
                            tracing::warn!("hash failed for {}: {}", file_name_err, err_msg);
                            show_toast_err(format!("Failed to hash {}: {}", file_name_err, err_msg));
                            set_busy_state(&busy_c2, &spinner_c2, &actions_row_c2, &btn_prev_c2, &btn_next_c2, false);
                            update_ui_c2();
                            glib::ControlFlow::Break
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
                    }
                });
                std::thread::spawn(move || {
                    let res = compute_sha256(&current_clone).map(|h| h.to_ascii_lowercase()).map_err(|e| e.to_string());
                    let _ = tx.send(res);
                });
                return;
            }
        })
    };

    // Undo / Redo handlers (ADR 0005) on the Organizer Database (#24)
    let perform_undo: Rc<dyn Fn()> = {
        let source_c = source_folder.clone();
        let store_c = store.clone();
        let undo_c = undo_stack.clone();
        let redo_c = redo_stack.clone();
        let idx_c = idx.clone();
        let snap_c = snapshot_rc.clone();
        let show_toast_c = show_toast.clone();
        let update_ui_c = update_ui.clone();
        let prehash_c = prehash_next.clone();
        let busy_c = busy.clone();
        let override_c = current_override.clone();
        Rc::new(move || {
            if guard_busy(&busy_c) {
                return;
            }
            let entry_opt = undo_c.borrow_mut().pop();
            let entry = match entry_opt {
                Some(e) => e,
                None => {
                    show_toast_c("Nothing to undo".into());
                    return;
                }
            };
            // do the filesystem undo + database row revert
            match undo_classification(&store_c, &source_c, &entry) {
                Ok(restored_path) => {
                    // push to redo with adjusted src_name (actual restored file name)
                    let restored_name = restored_path.file_name().and_then(|n| n.to_str()).unwrap_or(&entry.src_name).to_string();
                    let redo_entry = UndoEntry::new(&entry.hash, &restored_name, entry.dest_path.clone(), entry.was_duplicate, &entry.folder_name, &entry.display_name);
                    redo_c.borrow_mut().push(redo_entry);
                    // Make file Current File again (adjust queue index)
                    let search_name = entry.src_name.clone();
                    let mut found_idx: Option<usize> = None;
                    for (i, p) in snap_c.iter().enumerate() {
                        if let Some(n) = p.file_name().and_then(|x| x.to_str()) {
                            if n == search_name {
                                found_idx = Some(i);
                                break;
                            }
                        }
                    }
                    if let Some(fi) = found_idx {
                        *idx_c.borrow_mut() = fi;
                    } else if let Some(fi2) = snap_c.iter().position(|p| p.file_name().and_then(|x| x.to_str()) == Some(restored_name.as_str())) {
                        *idx_c.borrow_mut() = fi2;
                    }
                    // If suffix was used (restored_name != entry.src_name), keep override so Preview shows suffixed file
                    if restored_name != entry.src_name {
                        *override_c.borrow_mut() = Some(restored_path.clone());
                    } else {
                        *override_c.borrow_mut() = None;
                    }
                    show_toast_c(format!("Undid {}: {} → Source", entry.display_name, restored_name));
                    update_ui_c();
                    // prehash the restored file and next
                    prehash_c();
                }
                Err(e) => {
                    // push back onto undo_stack since it failed? ADR says stay, but we popped. Put it back.
                    undo_c.borrow_mut().push(entry);
                    tracing::warn!("undo failed: {}", e);
                    show_toast_c(format!("Undo failed: {}", e));
                    update_ui_c();
                }
            }
        })
    };

    let perform_redo: Rc<dyn Fn()> = {
        let source_c = source_folder.clone();
        let store_c = store.clone();
        let undo_c = undo_stack.clone();
        let redo_c = redo_stack.clone();
        let idx_c = idx.clone();
        let snap_c = snapshot_rc.clone();
        let show_toast_c = show_toast.clone();
        let update_ui_c = update_ui.clone();
        let prehash_c = prehash_next.clone();
        let busy_c = busy.clone();
        let override_c = current_override.clone();
        Rc::new(move || {
            if guard_busy(&busy_c) {
                return;
            }
            let entry_opt = redo_c.borrow_mut().pop();
            let entry = match entry_opt {
                Some(e) => e,
                None => {
                    show_toast_c("Nothing to redo".into());
                    return;
                }
            };
            // current file is the restored file in source_folder
            let current_file = source_c.join(&entry.src_name);
            if !current_file.exists() {
                tracing::warn!("redo source missing: {}", current_file.display());
                show_toast_c(format!("Redo failed: {} not found", entry.src_name));
                // push back?
                redo_c.borrow_mut().push(entry);
                return;
            }
            let redo_hash = match FileHash::new(&entry.hash) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("redo invalid hash for {}: {}", entry.src_name, e);
                    show_toast_c(format!("Redo failed: invalid hash for {}", entry.src_name));
                    redo_c.borrow_mut().push(entry);
                    return;
                }
            };
            match classify_file(&store_c, &current_file, &entry.folder_name, &redo_hash) {
                Ok(outcome) => {
                    let redo_was_duplicate = outcome.was_duplicate();
                    let dest = outcome.into_dest();
                    let new_undo = UndoEntry::new(&entry.hash, &entry.src_name, dest.clone(), redo_was_duplicate, &entry.folder_name, &entry.display_name);
                    push_undo_capped(&mut undo_c.borrow_mut(), new_undo, snap_c.len());
                    *override_c.borrow_mut() = None;
                    show_toast_c(format!("Redid {}: {} → {}", entry.display_name, entry.src_name, dest.file_name().and_then(|n| n.to_str()).unwrap_or("")));
                    // Advance past this file only if it was current (like normal move), else fallback
                    let cur_name = entry.src_name.clone();
                    if snap_c.iter().position(|p| p.file_name().and_then(|x| x.to_str()) == Some(cur_name.as_str())) == Some(*idx_c.borrow()) {
                        advance_idx(&idx_c, snap_c.len());
                    } else if snap_c.iter().position(|p| p.file_name().and_then(|x| x.to_str()) == Some(cur_name.as_str())).is_none() {
                        advance_idx(&idx_c, snap_c.len());
                    }
                    update_ui_c();
                    prehash_c();
                }
                Err(e) => {
                    redo_c.borrow_mut().push(entry);
                    show_toast_c(format!("Redo failed: {}", mover_error_message(&e, "redo")));
                    update_ui_c();
                }
            }
        })
    };

    // Rebuild Action Bar buttons from live_actions (real mover)
    let rebuild_action_bar: Rc<dyn Fn()> = {
        let actions_row_c = actions_row.clone();
        let live_c = live_actions.clone();
        let trigger_c = trigger_move.clone();
        Rc::new(move || {
            while let Some(child) = actions_row_c.first_child() {
                actions_row_c.remove(&child);
            }
            let actions = live_c.borrow().clone();
            for act in actions.iter() {
                let btn = Button::new();
                btn.add_css_class("action-btn");
                if act.display_name == "Keep" {
                    btn.add_css_class("suggested-action");
                } else if act.display_name == "Reject" {
                    btn.add_css_class("destructive-action");
                }
                let inner = GtkBox::new(Orientation::Horizontal, 6);
                inner.set_halign(gtk4::Align::Center);
                let lbl = Label::new(Some(&act.display_name));
                let badge = Label::new(Some(&format!("[{}]", act.shortcut)));
                badge.add_css_class("shortcut-badge");
                inner.append(&lbl);
                inner.append(&badge);
                btn.set_child(Some(&inner));
                btn.set_tooltip_text(Some(&format!("{} — {} or Ctrl+{}", act.display_name, act.shortcut, act.shortcut)));
                let act_clone = act.clone();
                let trig = trigger_c.clone();
                btn.connect_clicked(move |_| {
                    trig(act_clone.clone());
                });
                actions_row_c.append(&btn);
            }
        })
    };
    // Initial build
    rebuild_action_bar();

    // Empty button rebuild
    {
        let app_w = window.clone();
        let app_clone = app.clone();
        empty_btn.connect_clicked(move |_| {
            let dlg = gtk4::FileDialog::new();
            dlg.set_title("Choose Source Folder");
            let app_c = app_clone.clone();
            let win_for_dialog = app_w.clone();
            let win_for_closure = app_w.clone();
            dlg.select_folder(Some(&win_for_dialog), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        match build_snapshot(&p) {
                            Ok(snap) => {
                                win_for_closure.close();
                                build_shell(&app_c, snap, p);
                            }
                            Err(e) => {
                                let lbl = Label::new(Some(&format!(
                                    "Failed to read {}: {}",
                                    p.display(),
                                    e
                                )));
                                win_for_closure.set_child(Some(&lbl));
                            }
                        }
                    }
                }
            });
        });
    }

    {
        let idx_c = idx.clone();
        let upd = update_ui.clone();
        let prehash_c = prehash_next.clone();
        let busy_c = busy.clone();
        let override_c = current_override.clone();
        btn_prev.connect_clicked(move |_| {
            if guard_busy(&busy_c) { return; }
            *override_c.borrow_mut() = None;
            let mut v = idx_c.borrow_mut();
            if *v > 0 { *v -= 1; }
            drop(v);
            upd();
            prehash_c();
        });
    }
    {
        let idx_c = idx.clone();
        let upd = update_ui.clone();
        let snap_c = snapshot_rc.clone();
        let prehash_c = prehash_next.clone();
        let busy_c = busy.clone();
        let override_c = current_override.clone();
        btn_next.connect_clicked(move |_| {
            if guard_busy(&busy_c) { return; }
            *override_c.borrow_mut() = None;
            let mut v = idx_c.borrow_mut();
            if *v + 1 < snap_c.len() { *v += 1; }
            drop(v);
            upd();
            prehash_c();
        });
    }
    {
        let undo_c = perform_undo.clone();
        let busy_c = busy.clone();
        btn_undo.connect_clicked(move |_| {
            if guard_busy(&busy_c) { return; }
            undo_c();
        });
    }
    {
        let redo_c = perform_redo.clone();
        let busy_c = busy.clone();
        btn_redo.connect_clicked(move |_| {
            if guard_busy(&busy_c) { return; }
            redo_c();
        });
    }

    // Shared Settings opener (Duplicated Code fix): gear button, Ctrl+, and
    // Edit → Preferences all open the same dialog from the same context.
    let show_settings: Rc<dyn Fn(&ApplicationWindow)> = {
        let store_c = store.clone();
        let live_c = live_actions.clone();
        let disk_c = disk_actions.clone();
        let rebuild_c = rebuild_action_bar.clone();
        Rc::new(move |parent: &ApplicationWindow| {
            open_settings(parent, SettingsContext { store: store_c.clone(), live_actions: live_c.clone(), disk_actions: disk_c.clone(), rebuild_action_bar: rebuild_c.clone() });
        })
    };

    // Settings trigger via gear button
    {
        let win_c = window.clone();
        let show_c = show_settings.clone();
        btn_settings.connect_clicked(move |_| {
            show_c(&win_c);
        });
    }

    // Keyboard
    let key_ctl = gtk4::EventControllerKey::new();
    {
        let snap_k = snapshot_rc.clone();
        let btn_next_k = btn_next.clone();
        let btn_prev_k = btn_prev.clone();
        let live_k = live_actions.clone();
        let window_weak = window.downgrade();
        let show_settings_k = show_settings.clone();
        let trigger_k = trigger_move.clone();
        let busy_k = busy.clone();
        let undo_k = perform_undo.clone();
        let redo_k = perform_redo.clone();
        key_ctl.connect_key_pressed(move |_, key, _code, state| {
            let is_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let is_shift = state.contains(gdk::ModifierType::SHIFT_MASK);

            // Ctrl+, opens settings (comma)
            if is_ctrl && (key == gdk::Key::comma || key == gdk::Key::less) {
                if let Some(win) = window_weak.upgrade() {
                    show_settings_k(&win);
                    return glib::Propagation::Stop;
                }
            }

            // Undo / Redo global even when Preview has focus (EventControllerKey)
            if is_ctrl && key == gdk::Key::z && !is_shift {
                if guard_busy(&busy_k) {
                    return glib::Propagation::Stop;
                }
                undo_k();
                return glib::Propagation::Stop;
            }
            if is_ctrl && ((key == gdk::Key::z && is_shift) || key == gdk::Key::y) {
                if guard_busy(&busy_k) {
                    return glib::Propagation::Stop;
                }
                redo_k();
                return glib::Propagation::Stop;
            }

            if guard_busy(&busy_k) {
                if matches!(key, gdk::Key::_1 | gdk::Key::_2 | gdk::Key::_3 | gdk::Key::_4 | gdk::Key::_5 | gdk::Key::_6 | gdk::Key::_7 | gdk::Key::_8 | gdk::Key::_9
                    | gdk::Key::KP_1 | gdk::Key::KP_2 | gdk::Key::KP_3 | gdk::Key::KP_4 | gdk::Key::KP_5 | gdk::Key::KP_6 | gdk::Key::KP_7 | gdk::Key::KP_8 | gdk::Key::KP_9) {
                    return glib::Propagation::Stop;
                }
                // also block undo/redo while busy
                if is_ctrl && (key == gdk::Key::z || key == gdk::Key::y) {
                    return glib::Propagation::Stop;
                }
            }

            match key {
                gdk::Key::Right | gdk::Key::n | gdk::Key::N | gdk::Key::space => {
                    if let Some(win) = window_weak.upgrade() {
                        if is_entry_focused(&win) && key == gdk::Key::space && !is_ctrl {
                            return glib::Propagation::Proceed;
                        }
                    }
                    if guard_busy(&busy_k) {
                        return glib::Propagation::Stop;
                    }
                    btn_next_k.emit_clicked();
                    return glib::Propagation::Stop;
                }
                gdk::Key::Left | gdk::Key::p | gdk::Key::P => {
                    if let Some(win) = window_weak.upgrade() {
                        if is_entry_focused(&win) && !is_ctrl {
                            return glib::Propagation::Proceed;
                        }
                    }
                    if guard_busy(&busy_k) {
                        return glib::Propagation::Stop;
                    }
                    btn_prev_k.emit_clicked();
                    return glib::Propagation::Stop;
                }
                _ => {}
            }

            // Action shortcuts 1-9
            let digit = match key {
                gdk::Key::_1 | gdk::Key::KP_1 => Some('1'),
                gdk::Key::_2 | gdk::Key::KP_2 => Some('2'),
                gdk::Key::_3 | gdk::Key::KP_3 => Some('3'),
                gdk::Key::_4 | gdk::Key::KP_4 => Some('4'),
                gdk::Key::_5 | gdk::Key::KP_5 => Some('5'),
                gdk::Key::_6 | gdk::Key::KP_6 => Some('6'),
                gdk::Key::_7 | gdk::Key::KP_7 => Some('7'),
                gdk::Key::_8 | gdk::Key::KP_8 => Some('8'),
                gdk::Key::_9 | gdk::Key::KP_9 => Some('9'),
                _ => None,
            };
            if let Some(d) = digit {
                // Bare digits should type in Settings entries: if focus is Entry and no Ctrl, proceed
                if !is_ctrl {
                    if let Some(win) = window_weak.upgrade() {
                        if is_entry_focused(&win) {
                            return glib::Propagation::Proceed;
                        }
                    }
                }
                // Find action with this shortcut in live_actions
                let actions = live_k.borrow();
                if let Some(act) = actions.iter().find(|a| a.shortcut == d.to_string()).cloned() {
                    drop(actions);
                    // trigger real move
                    let _ = &snap_k; // keep alive
                    trigger_k(act);
                    return glib::Propagation::Stop;
                } else {
                    return glib::Propagation::Proceed;
                }
            }

            glib::Propagation::Proceed
        });
    }
    window.add_controller(key_ctl);

    // Also add shortcut actions for Edit menu: Undo, Redo, Preferences
    {
        let win_c = window.clone();
        let show_c = show_settings.clone();
        let action = gio::SimpleAction::new("preferences", None);
        action.connect_activate(move |_, _| {
            show_c(&win_c);
        });
        window.add_action(&action);
    }
    {
        let undo_c = perform_undo.clone();
        let action = gio::SimpleAction::new("undo", None);
        action.connect_activate(move |_, _| {
            undo_c();
        });
        window.add_action(&action);
    }
    {
        let redo_c = perform_redo.clone();
        let action = gio::SimpleAction::new("redo", None);
        action.connect_activate(move |_, _| {
            redo_c();
        });
        window.add_action(&action);
    }

    update_ui();
    // initial pre-hash current + next
    prehash_next();

    let paned_weak = paned.downgrade();
    let win_weak = window.downgrade();
    glib::idle_add_local_once(move || {
        if let (Some(p), Some(w)) = (paned_weak.upgrade(), win_weak.upgrade()) {
            let h = w.height();
            if h > 0 {
                p.set_position((h as f64 * 0.80) as i32);
            }
        }
    });

    window.present();
}

fn main() -> glib::ExitCode {
    let args = Args::parse();
    if args.self_test_sandbox {
        std::process::exit(run_self_test_sandbox());
    }
    let app = Application::builder()
        .application_id("com.example.organizer")
        .build();

    // Edit → Undo/Redo/Preferences menubar
    {
        let edit_menu = gio::Menu::new();
        edit_menu.append(Some("Undo"), Some("win.undo"));
        edit_menu.append(Some("Redo"), Some("win.redo"));
        edit_menu.append(Some("Preferences"), Some("win.preferences"));
        let menu = gio::Menu::new();
        menu.append_submenu(Some("Edit"), &edit_menu);
        app.set_menubar(Some(&menu));
    }

    let source_opt = args.source_folder.clone();

    app.connect_activate(move |app| {
        if let Some(ref p) = source_opt {
            let path = p.clone();
            if !path.is_dir() {
                let win = ApplicationWindow::builder()
                    .application(app)
                    .title("Organizer — invalid folder")
                    .default_width(400)
                    .default_height(120)
                    .build();
                win.maximize();
                let lbl = Label::new(Some(&format!("Not a directory: {}", path.display())));
                win.set_child(Some(&lbl));
                win.present();
                return;
            }
            match build_snapshot(&path) {
                Ok(snap) => build_shell(app, snap, path),
                Err(e) => {
                    let win = ApplicationWindow::builder()
                        .application(app)
                        .title("Organizer — error")
                        .build();
                    win.maximize();
                    let lbl = Label::new(Some(&format!("Failed to read {}: {}", path.display(), e)));
                    win.set_child(Some(&lbl));
                    win.present();
                }
            }
        } else {
            let win = ApplicationWindow::builder()
                .application(app)
                .title("Organizer — choose folder")
                .default_width(200)
                .default_height(100)
                .build();
            win.maximize();
            win.present();
            let dlg = gtk4::FileDialog::new();
            dlg.set_title("Choose Source Folder");
            let app_clone = app.clone();
            let win_for_dialog = win.clone();
            let win_for_closure = win.clone();
            dlg.select_folder(Some(&win_for_dialog), gio::Cancellable::NONE, move |res| {
                match res {
                    Ok(f) => {
                        if let Some(p) = f.path() {
                            match build_snapshot(&p) {
                                Ok(snap) => {
                                    win_for_closure.close();
                                    build_shell(&app_clone, snap, p);
                                }
                                Err(e) => {
                                    let lbl = Label::new(Some(&format!(
                                        "Failed to read {}: {}",
                                        p.display(),
                                        e
                                    )));
                                    win_for_closure.set_child(Some(&lbl));
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let lbl = Label::new(Some(&format!("No folder chosen: {e}")));
                        win_for_closure.set_child(Some(&lbl));
                    }
                }
            });
        }
    });

    app.run()
}
