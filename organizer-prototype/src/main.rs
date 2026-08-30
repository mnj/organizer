//! Throwaway prototype — maximized split-view shell & preview pane
//! No real file I/O, glycin, or GStreamer — mock data only.
//! See docs/prototype-shell.md for layout decisions.

use gdk4::prelude::*;
use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, Label,
    Orientation, Paned, Picture, Stack,
};
use libadwaita::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Debug)]
struct MockFile {
    name: &'static str,
    supported: bool,
    kind: &'static str, // hint for icon
    duplicate: bool,
}

const MOCK_FILES: &[MockFile] = &[
    MockFile { name: "001-beach-sunset.jpg", supported: true, kind: "image", duplicate: false },
    MockFile { name: "002-notes.pdf", supported: false, kind: "unsupported", duplicate: false },
    MockFile { name: "003-funny-cat.gif", supported: true, kind: "animated", duplicate: false },
    MockFile { name: "004-clip.webm", supported: true, kind: "video", duplicate: false },
    MockFile { name: "005-portrait.heic", supported: true, kind: "image", duplicate: false },
    MockFile { name: "006-backup.zip", supported: false, kind: "unsupported", duplicate: false },
    MockFile { name: "007-city.tiff", supported: true, kind: "image", duplicate: true },
    MockFile { name: "008-icon.svg", supported: true, kind: "vector", duplicate: false },
];

const ACTIONS: &[(&str, &str, &str)] = &[
    ("Keep", "keep", "1"),
    ("Maybe", "maybe", "2"),
    ("Reject", "reject", "3"),
];

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id("com.example.organizer-prototype")
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    // --- state ---
    let idx: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));
    let history: Rc<RefCell<Vec<(usize, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let redo_stack: Rc<RefCell<Vec<(usize, String)>>> = Rc::new(RefCell::new(Vec::new()));

    // --- CSS ---
    let css = CssProvider::new();
    css.load_from_data(
        r#"
        .preview-area {
            background-color: #1e1e2e;
        }
        .preview-placeholder {
            background-color: #1e1e2e;
        }
        .unsupported-badge {
            background: #fab387;
            color: #1e1e2e;
            border-radius: 8px;
            padding: 2px 8px;
            font-size: 11px;
            font-weight: 700;
        }
        .duplicate-badge {
            background: #f38ba8;
            color: #1e1e2e;
            border-radius: 8px;
            padding: 2px 8px;
            font-size: 11px;
            font-weight: 700;
        }
        .action-btn {
            padding: 10px 18px;
            font-weight: 600;
        }
        .shortcut-badge {
            background: alpha(@accent_bg_color, 0.2);
            border-radius: 4px;
            padding: 0 6px;
            font-size: 11px;
            font-weight: 700;
        }
        .toast {
            background: #313244;
            color: #cdd6f4;
            border-radius: 10px;
            padding: 8px 14px;
            box-shadow: 0 4px 12px alpha(black, 0.35);
        }
        .index-label {
            font-weight: 600;
        }
    "#,
    );
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // --- Window ---
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Organizer — Prototype (shell + preview)")
        .default_width(1280)
        .default_height(800)
        .build();
    window.maximize();

    // overlay for toast
    let overlay = gtk4::Overlay::new();

    // --- Paned vertical split ---
    let paned = Paned::new(Orientation::Vertical);
    paned.set_wide_handle(true);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);
    // 80/20 split realized after window maps: set_position via size-allocate
    // We'll set initial position via default size: 800 * 0.8 ~= 640
    paned.set_position(640);
    paned.set_resize_start_child(true);
    paned.set_resize_end_child(false);

    // ===== TOP: Preview =====
    let preview_box = GtkBox::new(Orientation::Vertical, 0);
    preview_box.add_css_class("preview-area");
    preview_box.set_hexpand(true);
    preview_box.set_vexpand(true);

    // header inside preview: filename dup info (also in action bar, but preview shows type)
    // Stack: picture vs unsupported placeholder
    let stack = Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    stack.set_transition_duration(150);

    // Picture for supported files
    let picture = Picture::new();
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_halign(gtk4::Align::Fill);
    picture.set_valign(gtk4::Align::Fill);
    // Mock paintable: icon paintable via IconTheme lookup fallback
    // Use a simple icon name paintable via Texture placeholder: we create a 1x1 transparent texture
    // and rely on overlay icon label to show state. Instead use an icon widget inside picture overlay.
    // For realism we keep a centered icon + label overlay.
    let picture_wrapper = GtkBox::new(Orientation::Vertical, 12);
    picture_wrapper.set_halign(gtk4::Align::Center);
    picture_wrapper.set_valign(gtk4::Align::Center);
    picture_wrapper.set_hexpand(true);
    picture_wrapper.set_vexpand(true);
    picture_wrapper.add_css_class("preview-area");

    let preview_icon = Label::new(None);
    preview_icon.set_markup(r#"<span size="60000">🖼️</span>"#);
    preview_icon.add_css_class("preview-placeholder");
    let preview_kind_label = Label::new(None);
    preview_kind_label.add_css_class("dim-label");
    preview_kind_label.set_wrap(true);

    // picture itself underneath icon (for HiDPI Texture path in real app)
    // we keep picture visible but empty; icon/label sits on top via Overlay
    let picture_overlay = gtk4::Overlay::new();
    picture_overlay.set_hexpand(true);
    picture_overlay.set_vexpand(true);
    picture_overlay.set_child(Some(&picture));
    // overlay child: centered box
    let center_box = GtkBox::new(Orientation::Vertical, 6);
    center_box.set_halign(gtk4::Align::Center);
    center_box.set_valign(gtk4::Align::Center);
    center_box.append(&preview_icon);
    center_box.append(&preview_kind_label);
    picture_overlay.add_overlay(&center_box);

    // Unsupported placeholder card
    let placeholder = GtkBox::new(Orientation::Vertical, 12);
    placeholder.set_halign(gtk4::Align::Center);
    placeholder.set_valign(gtk4::Align::Center);
    placeholder.set_hexpand(true);
    placeholder.set_vexpand(true);
    let ph_icon = Label::new(None);
    ph_icon.set_markup(r#"<span size="50000">📄</span>"#);
    let ph_name = Label::new(None);
    ph_name.add_css_class("title-3");
    ph_name.set_wrap(true);
    ph_name.set_justify(gtk4::Justification::Center);
    let ph_badge = Label::new(Some("Unsupported"));
    ph_badge.add_css_class("unsupported-badge");
    ph_badge.set_halign(gtk4::Align::Center);
    let ph_hint = Label::new(Some("Skippable via Prev/Next — never moved or hashed"));
    ph_hint.add_css_class("dim-label");
    ph_hint.set_wrap(true);
    ph_hint.set_justify(gtk4::Justification::Center);
    placeholder.append(&ph_icon);
    placeholder.append(&ph_name);
    placeholder.append(&ph_badge);
    placeholder.append(&ph_hint);

    stack.add_named(&picture_overlay, Some("picture"));
    stack.add_named(&placeholder, Some("placeholder"));

    preview_box.append(&stack);
    paned.set_start_child(Some(&preview_box));

    // ===== BOTTOM: Action Bar =====
    let action_bar = GtkBox::new(Orientation::Vertical, 8);
    action_bar.set_margin_top(10);
    action_bar.set_margin_bottom(10);
    action_bar.set_margin_start(12);
    action_bar.set_margin_end(12);

    // Row 1: navigation + index + duplicate + undo/redo + settings
    let nav_row = GtkBox::new(Orientation::Horizontal, 8,);
    nav_row.set_halign(gtk4::Align::Fill);

    let btn_prev = Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Previous (← / p)")
        .build();
    let btn_next = Button::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text("Next (→ / n / Space)")
        .build();

    let index_label = Label::new(Some("1 / 8 — 001-beach-sunset.jpg"));
    index_label.add_css_class("index-label");
    index_label.set_halign(gtk4::Align::Start);
    index_label.set_hexpand(false);

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
    let btn_redo = Button::builder()
        .label("Redo")
        .icon_name("edit-redo-symbolic")
        .tooltip_text("Redo (Ctrl+Shift+Z / Ctrl+Y)")
        .build();
    btn_redo.set_sensitive(false);

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
    // spacer
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    nav_row.append(&spacer);
    nav_row.append(&btn_undo);
    nav_row.append(&btn_redo);
    nav_row.append(&btn_settings);

    // Row 2: Action buttons
    let actions_row = GtkBox::new(Orientation::Horizontal, 10);
    actions_row.set_halign(gtk4::Align::Center);
    actions_row.set_hexpand(true);

    let mut action_buttons: Vec<Button> = Vec::new();
    for (label, folder, shortcut) in ACTIONS {
        let btn = Button::new();
        btn.add_css_class("action-btn");
        // style suggested/destructive for demo
        if *label == "Keep" {
            btn.add_css_class("suggested-action");
        } else if *label == "Reject" {
            btn.add_css_class("destructive-action");
        }
        // content: label + badge [1]
        let inner = GtkBox::new(Orientation::Horizontal, 6);
        inner.set_halign(gtk4::Align::Center);
        let lbl = Label::new(Some(label));
        let badge = Label::new(Some(&format!("[{}]", shortcut)));
        badge.add_css_class("shortcut-badge");
        inner.append(&lbl);
        inner.append(&badge);
        btn.set_child(Some(&inner));
        btn.set_tooltip_text(Some(&format!("{} → {}/  ({} / Ctrl+{})", label, folder, shortcut, shortcut)));
        actions_row.append(&btn);
        action_buttons.push(btn);
    }

    // hint label below buttons
    let hint = Label::new(Some("Press 1–3 or Ctrl+1–3 to move • Ctrl+Z undo • Ctrl+Shift+Z redo"));
    hint.add_css_class("dim-label");
    hint.set_halign(gtk4::Align::Center);

    action_bar.append(&nav_row);
    action_bar.append(&actions_row);
    action_bar.append(&hint);

    // make action bar not collapse: wrap in Box with fixed min
    let action_bar_wrapper = GtkBox::new(Orientation::Vertical, 0);
    action_bar_wrapper.set_size_request(-1, 140);
    action_bar_wrapper.append(&action_bar);
    paned.set_end_child(Some(&action_bar_wrapper));

    overlay.set_child(Some(&paned));

    // Toast label (hidden)
    let toast = Label::new(None);
    toast.add_css_class("toast");
    toast.set_halign(gtk4::Align::Center);
    toast.set_valign(gtk4::Align::End);
    toast.set_margin_bottom(24);
    toast.set_visible(false);
    overlay.add_overlay(&toast);

    window.set_child(Some(&overlay));

    // --- helpers ---
    let idx_dup = idx.clone();
    let history_dup = history.clone();
    let redo_dup = redo_stack.clone();

    // Build icon name for mock kind (visual hint)
    let kind_to_icon = |kind: &str| match kind {
        "video" => "🎬",
        "animated" => "🎞️",
        "vector" => "🔷",
        "unsupported" => "📄",
        _ => "🖼️",
    };

    // update preview chrome
    let update_ui = {
        let stack = stack.clone();
        let index_label = index_label.clone();
        let dup_badge = dup_badge.clone();
        let ph_name = ph_name.clone();
        let preview_kind_label = preview_kind_label.clone();
        let preview_icon = preview_icon.clone();
        let btn_prev = btn_prev.clone();
        let btn_next = btn_next.clone();
        let idx_c = idx.clone();
        move || {
            let i = *idx_c.borrow();
            let file = &MOCK_FILES[i];
            let total = MOCK_FILES.len();
            index_label.set_text(&format!("{} / {} — {}", i + 1, total, file.name));
            dup_badge.set_visible(file.duplicate);
            btn_prev.set_sensitive(i > 0);
            btn_next.set_sensitive(i + 1 < total);
            if file.supported {
                stack.set_visible_child_name("picture");
                preview_icon.set_markup(&format!(
                    r#"<span size="60000">{}</span>"#,
                    kind_to_icon(file.kind)
                ));
                let kind_desc = match file.kind {
                    "video" => "webm/mp4 — GStreamer Paintable (mock) — auto-play loop",
                    "animated" => "gif — glycin → Texture animated (mock)",
                    "vector" => "svg — glycin → Texture (mock)",
                    _ => "png/jpg/webp/tiff/heic — glycin → Texture (mock, HiDPI)",
                };
                preview_kind_label.set_markup(&format!(
                    r#"<span size="small">{}</span>  <span alpha="60%" size="small">Contain • dark #1e1e2e</span>"#,
                    glib::markup_escape_text(kind_desc)
                ));
                // For supported we could set a placeholder paintable via icon theme
                // Keep picture empty — icon overlay conveys mock.
            } else {
                stack.set_visible_child_name("placeholder");
                ph_name.set_text(file.name);
            }
        }
    };

    // Toast helper (auto-hide after 2.5s)
    let show_toast = {
        let toast = toast.clone();
        move |msg: String| {
            toast.set_text(&msg);
            toast.set_visible(true);
            let t = toast.clone();
            glib::timeout_add_seconds_local(3, move || {
                t.set_visible(false);
                glib::ControlFlow::Break
            });
        }
    };

    // Action handler: mock move
    let do_action = {
        let idx = idx.clone();
        let update_ui = update_ui.clone();
        let show_toast = show_toast.clone();
        let history = history.clone();
        let redo_stack = redo_stack.clone();
        let btn_undo = btn_undo.clone();
        let btn_redo = btn_redo.clone();
        move |action_idx: usize| {
            let i = *idx.borrow();
            let file = &MOCK_FILES[i];
            if !file.supported {
                show_toast(format!("Skipped {} — unsupported (mock)", file.name));
                return;
            }
            let (label, folder, _) = ACTIONS[action_idx];
            // push to history (cap 50)
            {
                let mut h = history.borrow_mut();
                if h.len() >= 50 {
                    h.remove(0);
                }
                h.push((i, folder.to_string()));
                btn_undo.set_sensitive(true);
            }
            redo_stack.borrow_mut().clear();
            btn_redo.set_sensitive(false);
            if file.duplicate {
                show_toast(format!("Duplicate {} → duplicate/ (mock toast)", file.name));
            } else {
                show_toast(format!("Move {} → {}/ (mock)", file.name, folder));
                let _ = label;
            }
            // auto-advance if not at end
            if i + 1 < MOCK_FILES.len() {
                *idx.borrow_mut() = i + 1;
            }
            update_ui();
        }
    };

    // Wire action buttons
    for (ai, btn) in action_buttons.iter().enumerate() {
        let h = do_action.clone();
        btn.connect_clicked(move |_| h(ai));
    }

    // Prev/Next
    {
        let idx = idx.clone();
        let update_ui = update_ui.clone();
        btn_prev.connect_clicked(move |_| {
            let mut v = idx.borrow_mut();
            if *v > 0 {
                *v -= 1;
            }
            drop(v);
            update_ui();
        });
    }
    {
        let idx = idx.clone();
        let update_ui = update_ui.clone();
        btn_next.connect_clicked(move |_| {
            let mut v = idx.borrow_mut();
            if *v + 1 < MOCK_FILES.len() {
                *v += 1;
            }
            drop(v);
            update_ui();
        });
    }

    // Undo/Redo (mock: move back)
    {
        let idx = idx.clone();
        let history = history.clone();
        let redo_stack = redo_stack.clone();
        let update_ui = update_ui.clone();
        let show_toast = show_toast.clone();
        let bu = btn_undo.clone();
        let br = btn_redo.clone();
        let bu2 = bu.clone();
        let br2 = br.clone();
        bu.connect_clicked(move |_| {
            let entry = history.borrow_mut().pop();
            if let Some((prev_idx, folder)) = entry {
                redo_stack.borrow_mut().push((prev_idx, folder.clone()));
                // heuristic: go back to that file
                *idx.borrow_mut() = prev_idx;
                show_toast(format!("Undo → {} (mock, move back _undo)", MOCK_FILES[prev_idx].name));
                br2.set_sensitive(true);
                if history.borrow().is_empty() {
                    bu2.set_sensitive(false);
                }
                update_ui();
            }
        });
    }
    {
        let idx = idx.clone();
        let history = history.clone();
        let redo_stack = redo_stack.clone();
        let update_ui = update_ui.clone();
        let show_toast = show_toast.clone();
        let bu = btn_undo.clone();
        let br = btn_redo.clone();
        let bu2 = bu.clone();
        let br2 = br.clone();
        br.connect_clicked(move |_| {
            let entry = redo_stack.borrow_mut().pop();
            if let Some((r_idx, folder)) = entry {
                history.borrow_mut().push((r_idx, folder.clone()));
                *idx.borrow_mut() = r_idx;
                show_toast(format!("Redo → {} (mock)", MOCK_FILES[r_idx].name));
                bu2.set_sensitive(true);
                if redo_stack.borrow().is_empty() {
                    br2.set_sensitive(false);
                }
                update_ui();
            }
        });
    }

    // Settings gear
    {
        let show_toast = show_toast.clone();
        let window_weak = window.downgrade();
        btn_settings.connect_clicked(move |_| {
            if let Some(win) = window_weak.upgrade() {
                // Mock AdwPreferencesWindow
                let dlg = libadwaita::PreferencesWindow::builder()
                    .transient_for(&win)
                    .modal(true)
                    .title("Settings (mock)")
                    .default_width(560)
                    .default_height(420)
                    .build();
                let page = libadwaita::PreferencesPage::new();
                page.set_title("Actions");
                page.set_description("3-field rows: display name + folder + shortcut — mock");
                let group = libadwaita::PreferencesGroup::new();
                group.set_title("Configured Actions (mock, live+Save)");
                for (label, folder, sc) in ACTIONS {
                    let row = libadwaita::ActionRow::new();
                    row.set_title(label);
                    row.set_subtitle(&format!("{}/  •  [{}]  •  a-z0-9_-", folder, sc));
                    let badge = Label::new(Some(&format!("[{}]", sc)));
                    badge.add_css_class("shortcut-badge");
                    row.add_suffix(&badge);
                    group.add(&row);
                }
                let add_row = libadwaita::ActionRow::new();
                add_row.set_title("Add / Remove / UpDown (max 9) — mock");
                add_row.set_subtitle("Strict validation: slug + unique shortcut");
                group.add(&add_row);
                page.add(&group);
                dlg.add(&page);
                dlg.present();
            } else {
                show_toast("Settings (mock)".into());
            }
        });
    }

    // Keyboard: EventControllerKey for 1-9 and Ctrl+1-9, Ctrl+Z, Ctrl+Shift+Z
    let key_ctl = gtk4::EventControllerKey::new();
    {
        let idx = idx_dup.clone();
        let history = history_dup.clone();
        let redo_stack = redo_dup.clone();
        let update_ui_k = update_ui.clone();
        let show_toast_k = show_toast.clone();
        let do_action_k = do_action.clone();
        let btn_undo_k = btn_undo.clone();
        let btn_redo_k = btn_redo.clone();
        let btn_next_k = btn_next.clone();
        let btn_prev_k = btn_prev.clone();
        key_ctl.connect_key_pressed(move |_, key, _code, state| {
            let mods = state;
            let is_ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
            let is_shift = mods.contains(gdk::ModifierType::SHIFT_MASK);

            // Ctrl+, or Ctrl+Shift+Z redo handled below, Ctrl+Z undo
            if is_ctrl && !is_shift && key == gdk::Key::z {
                // undo
                let entry = history.borrow_mut().pop();
                if let Some((prev_idx, folder)) = entry {
                    redo_stack.borrow_mut().push((prev_idx, folder.clone()));
                    *idx.borrow_mut() = prev_idx;
                    show_toast_k(format!("Undo (Ctrl+Z) → {} (mock)", MOCK_FILES[prev_idx].name));
                    btn_redo_k.set_sensitive(true);
                    if history.borrow().is_empty() {
                        btn_undo_k.set_sensitive(false);
                    }
                    update_ui_k();
                }
                return glib::Propagation::Stop;
            }
            if is_ctrl && (key == gdk::Key::Z || (is_shift && key == gdk::Key::z) || key == gdk::Key::y) {
                // redo: Ctrl+Shift+Z or Ctrl+Y
                let entry = redo_stack.borrow_mut().pop();
                if let Some((r_idx, folder)) = entry {
                    history.borrow_mut().push((r_idx, folder.clone()));
                    *idx.borrow_mut() = r_idx;
                    show_toast_k(format!("Redo → {} (mock)", MOCK_FILES[r_idx].name));
                    btn_undo_k.set_sensitive(true);
                    if redo_stack.borrow().is_empty() {
                        btn_redo_k.set_sensitive(false);
                    }
                    update_ui_k();
                }
                return glib::Propagation::Stop;
            }
            if is_ctrl && key == gdk::Key::comma {
                show_toast_k("Settings (Ctrl+,) — mock".into());
                return glib::Propagation::Stop;
            }

            // digits 1-9: both bare and Ctrl+1-9 trigger same action
            let digit: Option<usize> = match key {
                gdk::Key::_1 => Some(0),
                gdk::Key::_2 => Some(1),
                gdk::Key::_3 => Some(2),
                gdk::Key::_4 => Some(3),
                gdk::Key::_5 => Some(4),
                gdk::Key::_6 => Some(5),
                gdk::Key::_7 => Some(6),
                gdk::Key::_8 => Some(7),
                gdk::Key::_9 => Some(8),
                gdk::Key::KP_1 => Some(0),
                gdk::Key::KP_2 => Some(1),
                gdk::Key::KP_3 => Some(2),
                gdk::Key::KP_4 => Some(3),
                gdk::Key::KP_5 => Some(4),
                gdk::Key::KP_6 => Some(5),
                gdk::Key::KP_7 => Some(6),
                gdk::Key::KP_8 => Some(7),
                gdk::Key::KP_9 => Some(8),
                _ => None,
            };
            if let Some(d) = digit {
                if d < ACTIONS.len() {
                    do_action_k(d);
                    return glib::Propagation::Stop;
                } else if d < 9 {
                    show_toast_k(format!("Action {} unconfigured (mock)", d + 1));
                    return glib::Propagation::Stop;
                }
            }

            // navigation keys
            match key {
                gdk::Key::Right | gdk::Key::n | gdk::Key::N | gdk::Key::space => {
                    btn_next_k.emit_clicked();
                    return glib::Propagation::Stop;
                }
                gdk::Key::Left | gdk::Key::p | gdk::Key::P => {
                    btn_prev_k.emit_clicked();
                    return glib::Propagation::Stop;
                }
                _ => {}
            }

            glib::Propagation::Proceed
        });
    }
    window.add_controller(key_ctl);

    // Keep paned handle position ~80% on resize: connect to notify::position? v1: no persistence, just initial.
    // Ensure action bar stays 140px min: already via size_request.

    // initial render
    update_ui();
    window.present();

    // After map, enforce 80/20 ratio based on actual window height
    // Use idle to set position once allocated.
    let paned_weak = paned.downgrade();
    let window_weak = window.downgrade();
    glib::idle_add_local_once(move || {
        if let (Some(p), Some(w)) = (paned_weak.upgrade(), window_weak.upgrade()) {
            let h = w.height();
            if h > 0 {
                p.set_position((h as f64 * 0.80) as i32);
            }
        }
    });
}
