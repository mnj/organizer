use clap::Parser;
use gdk4::prelude::*;
use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, Entry,
    Label, Orientation, Paned, Stack,
};
use organizer_lib::config::{load_or_create, Action};
use organizer_lib::dedup::{compute_sha256, load_union};
use organizer_lib::mover::{move_to_action, MoverError};
use organizer_lib::queue::build_snapshot;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Parser, Debug)]
#[command(name = "organizer", about = "File-triaging desktop app")]
struct Args {
    #[arg(value_name = "SOURCE_FOLDER")]
    source_folder: Option<PathBuf>,
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

/// Hash preloading service (Divergent Change fix) — caches sha256 for Current + next.
struct HashService {
    cache: Rc<RefCell<HashMap<PathBuf, String>>>,
    snapshot: Rc<Vec<PathBuf>>,
    idx: Rc<RefCell<usize>>,
}

impl HashService {
    fn new(cache: Rc<RefCell<HashMap<PathBuf, String>>>, snapshot: Rc<Vec<PathBuf>>, idx: Rc<RefCell<usize>>) -> Self {
        Self { cache, snapshot, idx }
    }
    fn get_cached(&self, path: &PathBuf) -> Option<String> {
        self.cache.borrow().get(path).cloned()
    }
    fn remove(&self, path: &PathBuf) {
        self.cache.borrow_mut().remove(path);
    }
    /// Synchronous pre-hash of current and next (used on startup and after move).
    /// Future async variant can offload via glib channel without changing call sites.
    fn prehash_sync(&self) {
        let i = *self.idx.borrow();
        let len = self.snapshot.len();
        let mut to_hash = Vec::new();
        if i < len {
            let cur = self.snapshot[i].clone();
            if !self.cache.borrow().contains_key(&cur) && cur.exists() {
                to_hash.push(cur);
            }
        }
        if i + 1 < len {
            let nxt = self.snapshot[i + 1].clone();
            if !self.cache.borrow().contains_key(&nxt) && nxt.exists() {
                to_hash.push(nxt);
            }
        }
        for p in to_hash {
            if let Ok(h) = compute_sha256(&p) {
                self.cache.borrow_mut().insert(p, h.to_ascii_lowercase());
            }
        }
    }
    fn prehash_async(&self) {
        // For v1 we keep pre-hash synchronous to avoid Send complexity with Rc<RefCell>.
        // The HashService design isolates this; a future change can switch to Arc<Mutex>
        // + glib channel spawn_blocking without touching call sites (spec spawn_blocking).
        self.prehash_sync();
    }
    fn prehash_next(&self) {
        self.prehash_async();
    }
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

    // Load or create config
    let config = match load_or_create(&source_folder) {
        Ok(c) => c,
        Err(e) => {
            let window = ApplicationWindow::builder()
                .application(app)
                .title("Organizer — config error")
                .default_width(500)
                .default_height(120)
                .build();
            window.maximize();
            let lbl = Label::new(Some(&format!("Failed to load {}: {}", source_folder.join("organizer.toml").display(), e)));
            window.set_child(Some(&lbl));
            window.present();
            return;
        }
    };
    let config_version = config.config_version;
    let live_actions: Rc<RefCell<Vec<Action>>> = Rc::new(RefCell::new(config.actions.clone()));
    let disk_actions: Rc<RefCell<Vec<Action>>> = Rc::new(RefCell::new(config.actions.clone()));

    // Load union HashSet and warn count (ADR 0003/0004) : inside SourceFolder *.txt
    let (union_initial, warning_count) = load_union(&source_folder);
    if warning_count > 0 {
        tracing::warn!("{} corrupted log lines skipped at startup in {}", warning_count, source_folder.display());
    }
    let union_set: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(union_initial));
    let hash_cache: Rc<RefCell<HashMap<PathBuf, String>>> = Rc::new(RefCell::new(HashMap::new()));
    let busy: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));

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

    let ph_box = GtkBox::new(Orientation::Vertical, 12);
    ph_box.set_halign(gtk4::Align::Center);
    ph_box.set_valign(gtk4::Align::Center);
    let ph_icon = Label::new(None);
    ph_icon.set_markup(r#"<span size="50000">📄</span>"#);
    let ph_name = Label::new(None);
    ph_name.add_css_class("title-3");
    let ph_badge = Label::new(Some("Unsupported"));
    ph_badge.add_css_class("unsupported-badge");
    ph_box.append(&ph_icon);
    ph_box.append(&ph_name);
    ph_box.append(&ph_badge);

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
    let btn_undo = Button::builder()
        .label("Undo")
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Undo (Ctrl+Z)")
        .build();
    btn_undo.set_sensitive(false);
    let btn_settings = Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings (Ctrl+,)")
        .build();
    btn_settings.add_css_class("circular");

    nav_row.append(&btn_prev);
    nav_row.append(&btn_next);
    nav_row.append(&sep);
    nav_row.append(&index_label);
    nav_row.append(&dup_badge);
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    nav_row.append(&spacer);
    nav_row.append(&btn_undo);
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

    // Toast warning about corrupted logs if any (after window shown)
    if warning_count > 0 {
        let toast_clone = show_toast.clone();
        let wc = warning_count;
        glib::idle_add_local_once(move || {
            toast_clone(format!("{} corrupted log lines skipped (see warnings)", wc));
        });
    }

    let update_ui = {
        let idx_c = idx.clone();
        let snap_c = snapshot_rc.clone();
        let stack_c = stack.clone();
        let file_label_c = file_label.clone();
        let index_label_c = index_label.clone();
        let btn_prev_c = btn_prev.clone();
        let btn_next_c = btn_next.clone();
        let empty_label_c = empty_label.clone();
        let hint_c = hint.clone();
        let source_c = source_folder.clone();
        let busy_c = busy.clone();
        let actions_row_c = actions_row.clone();
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
                hint_c.set_text("");
                return;
            }
            let i = *idx_c.borrow();
            // clamp i to len - 1 (if we advanced beyond, show last + empty hint)
            let effective = i.min(len - 1);
            let path = &snap_c[effective];
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("—");
            // If i >= len, we have triaged past end: show empty
            if i >= len {
                stack_c.set_visible_child_name("empty");
                empty_label_c.set_text(&format!(
                    "All triaged — {} files sorted\n{}",
                    len,
                    source_c.display()
                ));
                index_label_c.set_text(&format!("{len} / {len} — done"));
                btn_prev_c.set_sensitive(len > 0 && !*busy_c.borrow());
                btn_next_c.set_sensitive(false);
                hint_c.set_text("");
                return;
            }
            stack_c.set_visible_child_name("file");
            file_label_c.set_text(name);
            index_label_c.set_text(&format!("{} / {} — {}", i + 1, len, name));
            let is_busy = *busy_c.borrow();
            btn_prev_c.set_sensitive(i > 0 && !is_busy);
            btn_next_c.set_sensitive(i + 1 < len && !is_busy);
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
        }
    };

    let hash_service = Rc::new(HashService::new(hash_cache.clone(), snapshot_rc.clone(), idx.clone()));
    let prehash_next: Rc<dyn Fn()> = {
        let hs = hash_service.clone();
        Rc::new(move || hs.prehash_next())
    };

    // Trigger move for a given Action — uses HashService + set_busy_state + glib channel for spawn_blocking
    let trigger_move: Rc<dyn Fn(Action)> = {
        let idx_c = idx.clone();
        let snap_c = snapshot_rc.clone();
        let source_c = source_folder.clone();
        let union_c = union_set.clone();
        let hash_service_c = hash_service.clone();
        let busy_c = busy.clone();
        let spinner_c = spinner.clone();
        let actions_row_c = actions_row.clone();
        let btn_prev_c = btn_prev.clone();
        let btn_next_c = btn_next.clone();
        let show_toast_c = show_toast.clone();
        let update_ui_c = update_ui.clone();
        let prehash_c = prehash_next.clone();
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
            set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, true);
            update_ui_c();

            let folder_name = action.folder_name.clone();
            let display_name = action.display_name.clone();
            let file_name = current.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();

            let hash = if let Some(cached) = hash_service_c.get_cached(&current) {
                hash_service_c.remove(&current);
                cached
            } else {
                match compute_sha256(&current).map(|h| h.to_ascii_lowercase()) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!("hash failed for {}: {}", current.display(), e);
                        show_toast_c(format!("Failed to hash {}: {}", file_name, e));
                        set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, false);
                        update_ui_c();
                        return;
                    }
                }
            };
            let mut union = union_c.borrow_mut();
            match move_to_action(&source_c, &current, &folder_name, &hash, &mut union) {
                Ok(dest) => {
                    drop(union);
                    show_toast_c(format!("Moved {} → {}/{} ", file_name, display_name, dest.file_name().and_then(|n| n.to_str()).unwrap_or("")));
                    let len2 = snap_c.len();
                    let mut v = idx_c.borrow_mut();
                    if *v + 1 < len2 { *v += 1; } else if *v + 1 == len2 { *v += 1; }
                    drop(v);
                    set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, false);
                    update_ui_c();
                    prehash_c();
                }
                Err(e) => {
                    drop(union);
                    let msg = match e {
                        MoverError::Exdev(_) => "Cross-device move not supported — move reverted".to_string(),
                        MoverError::Io(ref ioe) => {
                            tracing::warn!("move log failure for {}: {}", file_name, ioe);
                            "Disk full / I/O error — move reverted".to_string()
                        }
                    };
                    show_toast_c(msg);
                    set_busy_state(&busy_c, &spinner_c, &actions_row_c, &btn_prev_c, &btn_next_c, false);
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
        btn_prev.connect_clicked(move |_| {
            if guard_busy(&busy_c) { return; }
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
        btn_next.connect_clicked(move |_| {
            if guard_busy(&busy_c) { return; }
            let mut v = idx_c.borrow_mut();
            if *v + 1 < snap_c.len() { *v += 1; }
            drop(v);
            upd();
            prehash_c();
        });
    }

    // Settings trigger via gear button
    {
        let win_c = window.clone();
        let live_c = live_actions.clone();
        let disk_c = disk_actions.clone();
        let rebuild_c = rebuild_action_bar.clone();
        let source_c = source_folder.clone();
        btn_settings.connect_clicked(move |_| {
            open_settings(&win_c, SettingsContext { source_folder: source_c.clone(), live_actions: live_c.clone(), disk_actions: disk_c.clone(), rebuild_action_bar: rebuild_c.clone(), config_version });
        });
    }

    // Keyboard
    let key_ctl = gtk4::EventControllerKey::new();
    {
        let snap_k = snapshot_rc.clone();
        let btn_next_k = btn_next.clone();
        let btn_prev_k = btn_prev.clone();
        let show_toast_k = show_toast.clone();
        let live_k = live_actions.clone();
        let window_weak = window.downgrade();
        let live_for_settings = live_actions.clone();
        let disk_for_settings = disk_actions.clone();
        let rebuild_for_settings = rebuild_action_bar.clone();
        let source_for_settings = source_folder.clone();
        let trigger_k = trigger_move.clone();
        let busy_k = busy.clone();
        key_ctl.connect_key_pressed(move |_, key, _code, state| {
            let is_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let is_shift = state.contains(gdk::ModifierType::SHIFT_MASK);

            // Ctrl+, opens settings (comma)
            if is_ctrl && (key == gdk::Key::comma || key == gdk::Key::less) {
                if let Some(win) = window_weak.upgrade() {
                    open_settings(&win, SettingsContext { source_folder: source_for_settings.clone(), live_actions: live_for_settings.clone(), disk_actions: disk_for_settings.clone(), rebuild_action_bar: rebuild_for_settings.clone(), config_version });
                    return glib::Propagation::Stop;
                }
            }

            // Undo/Redo stubs still? Keep Ctrl+Z
            if is_ctrl && key == gdk::Key::z && !is_shift {
                show_toast_k("Undo (Ctrl+Z) — stub for ticket 01".into());
                return glib::Propagation::Stop;
            }
            if is_ctrl && ((key == gdk::Key::z && is_shift) || key == gdk::Key::y) {
                show_toast_k("Redo (Ctrl+Shift+Z / Ctrl+Y) — stub".into());
                return glib::Propagation::Stop;
            }

            if guard_busy(&busy_k) {
                if matches!(key, gdk::Key::_1 | gdk::Key::_2 | gdk::Key::_3 | gdk::Key::_4 | gdk::Key::_5 | gdk::Key::_6 | gdk::Key::_7 | gdk::Key::_8 | gdk::Key::_9
                    | gdk::Key::KP_1 | gdk::Key::KP_2 | gdk::Key::KP_3 | gdk::Key::KP_4 | gdk::Key::KP_5 | gdk::Key::KP_6 | gdk::Key::KP_7 | gdk::Key::KP_8 | gdk::Key::KP_9) {
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

    // Also add shortcut action for Edit → Preferences fallback via gio::SimpleAction
    {
        let win_c = window.clone();
        let live_c = live_actions.clone();
        let disk_c = disk_actions.clone();
        let rebuild_c = rebuild_action_bar.clone();
        let source_c = source_folder.clone();
        let action = gio::SimpleAction::new("preferences", None);
        action.connect_activate(move |_, _| {
            open_settings(&win_c, SettingsContext { source_folder: source_c.clone(), live_actions: live_c.clone(), disk_actions: disk_c.clone(), rebuild_action_bar: rebuild_c.clone(), config_version });
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
    let app = Application::builder()
        .application_id("com.example.organizer")
        .build();

    // Edit → Preferences menubar (third trigger per ADR 0006)
    {
        let edit_menu = gio::Menu::new();
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
