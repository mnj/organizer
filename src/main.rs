use clap::Parser;
use gdk4::prelude::*;
use gtk4::prelude::*;
use gtk4::{
    gdk, gio, glib, Application, ApplicationWindow, Box as GtkBox, Button, CssProvider, Label,
    Orientation, Paned, Stack,
};
use organizer_lib::queue::build_snapshot;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Parser, Debug)]
#[command(name = "organizer", about = "File-triaging desktop app")]
struct Args {
    /// Source Folder to triage (flat, direct children only)
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
        "#,
    );
    p
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

    // Placeholder for supported (filename)
    let file_label = Label::new(None);
    file_label.add_css_class("title-2");
    file_label.set_wrap(true);
    file_label.set_justify(gtk4::Justification::Center);
    file_label.set_halign(gtk4::Align::Center);
    file_label.set_valign(gtk4::Align::Center);

    let preview_center = GtkBox::new(Orientation::Vertical, 8);
    preview_center.set_halign(gtk4::Align::Center);
    preview_center.set_valign(gtk4::Align::Center);
    preview_center.set_hexpand(true);
    preview_center.set_vexpand(true);
    let icon = Label::new(None);
    icon.set_markup(r#"<span size="60000">🖼️</span>"#);
    preview_center.append(&icon);
    preview_center.append(&file_label);

    // Unsupported placeholder
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

    // Empty state
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

    stack.add_named(&preview_center, Some("file"));
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
        .tooltip_text("Undo (Ctrl+Z) — stub for ticket 01")
        .build();
    btn_undo.set_sensitive(false);
    let btn_settings = Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings (Ctrl+,) — stub")
        .build();
    btn_settings.add_css_class("circular");
    btn_settings.set_sensitive(false);

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

    // Hint
    let hint = Label::new(Some("Preview placeholder — moves not yet implemented (ticket 01)"));
    hint.add_css_class("dim-label");
    hint.set_halign(gtk4::Align::Center);
    action_bar.append(&hint);

    let wrapper = GtkBox::new(Orientation::Vertical, 0);
    wrapper.set_size_request(-1, 140);
    wrapper.append(&action_bar);
    paned.set_end_child(Some(&wrapper));

    overlay.set_child(Some(&paned));

    // Toast
    let toast = Label::new(None);
    toast.add_css_class("toast");
    toast.set_halign(gtk4::Align::Center);
    toast.set_valign(gtk4::Align::End);
    toast.set_margin_bottom(24);
    toast.set_visible(false);
    overlay.add_overlay(&toast);
    window.set_child(Some(&overlay));

    // State
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

    let update_ui = {
        let idx_c = idx.clone();
        let snap_c = snapshot_rc.clone();
        let stack_c = stack.clone();
        let file_label_c = file_label.clone();
        let index_label_c = index_label.clone();
        let btn_prev_c = btn_prev.clone();
        let btn_next_c = btn_next.clone();
        let empty_label_c = empty_label.clone();
        move || {
            let len = snap_c.len();
            if len == 0 {
                stack_c.set_visible_child_name("empty");
                empty_label_c.set_text(&format!(
                    "All triaged — {} files sorted\n{}",
                    len,
                    source_folder.display()
                ));
                index_label_c.set_text("—");
                btn_prev_c.set_sensitive(false);
                btn_next_c.set_sensitive(false);
                return;
            }
            let i = *idx_c.borrow();
            let path = &snap_c[i];
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("—");
            // For ticket 01, all files in snapshot are Supported, so we show file page.
            // If we ever encounter an unsupported that slipped in, show unsupported placeholder.
            stack_c.set_visible_child_name("file");
            file_label_c.set_text(name);
            index_label_c.set_text(&format!("{} / {} — {}", i + 1, len, name));
            btn_prev_c.set_sensitive(i > 0);
            btn_next_c.set_sensitive(i + 1 < len);
        }
    };

    // Empty button re-opens picker
    {
        let app_w = window.clone();
        let app_clone = app.clone();
        empty_btn.connect_clicked(move |_| {
            let dlg = gtk4::FileDialog::new();
            dlg.set_title("Choose Source Folder");
            let app_c = app_clone.clone();
            let win_c = app_w.clone();
            dlg.select_folder(Some(&win_c), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        if let Ok(snap) = build_snapshot(&p) {
                            // rebuild shell with new folder — for scaffold, just toast
                            let _ = &snap;
                            // In real app we would rebuild; for scaffold we restart
                            // Simplest: quit and ask user to relaunch
                            let _ = app_c;
                        }
                    }
                }
            });
        });
    }

    // Prev/Next
    {
        let idx_c = idx.clone();
        let upd = update_ui.clone();
        btn_prev.connect_clicked(move |_| {
            let mut v = idx_c.borrow_mut();
            if *v > 0 {
                *v -= 1;
            }
            drop(v);
            upd();
        });
    }
    {
        let idx_c = idx.clone();
        let upd = update_ui.clone();
        let snap_c = snapshot_rc.clone();
        btn_next.connect_clicked(move |_| {
            let mut v = idx_c.borrow_mut();
            if *v + 1 < snap_c.len() {
                *v += 1;
            }
            drop(v);
            upd();
        });
    }

    // Keyboard: Left/Right/n/p/Space for navigation
    let key_ctl = gtk4::EventControllerKey::new();
    {
        let idx_k = idx.clone();
        let snap_k = snapshot_rc.clone();
        let btn_next_k = btn_next.clone();
        let btn_prev_k = btn_prev.clone();
        let show_toast_k = show_toast.clone();
        key_ctl.connect_key_pressed(move |_, key, _code, state| {
            // Allow navigation even without modifiers
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
            // Stub for future 1-9: show toast that moves not yet implemented
            let is_ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let digit = match key {
                gdk::Key::_1 | gdk::Key::KP_1 => Some('1'),
                gdk::Key::_2 | gdk::Key::KP_2 => Some('2'),
                gdk::Key::_3 | gdk::Key::KP_3 => Some('3'),
                _ => None,
            };
            if let Some(d) = digit {
                let i = *idx_k.borrow();
                if i < snap_k.len() {
                    let name = snap_k[i]
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    if is_ctrl {
                        show_toast_k(format!("Ctrl+{} for {} — moves not yet implemented (01)", d, name));
                    } else {
                        show_toast_k(format!("{} for {} — moves not yet implemented (01)", d, name));
                    }
                }
                return glib::Propagation::Stop;
            }
            if is_ctrl && key == gdk::Key::z {
                show_toast_k("Undo (Ctrl+Z) — stub for ticket 01".into());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(key_ctl);

    // Initial render
    update_ui();

    // Enforce 80/20 ratio on idle
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

    // Keep source_folder for closure
    let source_opt = args.source_folder.clone();

    app.connect_activate(move |app| {
        if let Some(ref p) = source_opt {
            let path = p.clone();
            // Validate
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
            // No arg — show picker first
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
                        // User cancelled — show empty state with button
                        let lbl = Label::new(Some(&format!("No folder chosen: {e}")));
                        win_for_closure.set_child(Some(&lbl));
                    }
                }
            });
        }
    });

    app.run()
}
