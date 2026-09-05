use gtk4::prelude::*;
use gtk4::{gio, glib, ApplicationWindow, Box as GtkBox, Button, Entry, Label, Orientation};
use libadwaita as adw;
use adw::prelude::*;
use crate::config::{slugify, validate_categories, Category};
use crate::store::Store;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Bundled context for settings dialog — avoids Data Clumps
pub struct SettingsContext {
    pub store: Store,
    pub live_categories: Rc<RefCell<Vec<Category>>>,
    pub disk_categories: Rc<RefCell<Vec<Category>>>,
    pub rebuild_category_bar: Rc<dyn Fn()>,
}

#[allow(dead_code)]
struct RowHandle {
    container: GtkBox,
    display: Entry,
    folder: Entry,
    slug: Label,
    dropdown: gtk4::DropDown,
}

#[derive(Default, Clone)]
struct RowError {
    display: bool,
    folder: bool,
    shortcut: bool,
}

/// Helper to compute per-row validation flags for precise inline `error` CSS
fn per_row_errors(categories: &[Category]) -> Vec<RowError> {
    let mut out = vec![RowError::default(); categories.len()];

    // Too few/many is global, not per-row

    // display duplicates case-insensitive
    let mut display_counts: HashMap<String, usize> = HashMap::new();
    for c in categories {
        let key = c.display_name.trim().to_ascii_lowercase();
        if !key.is_empty() {
            *display_counts.entry(key).or_default() += 1;
        }
    }
    for (i, c) in categories.iter().enumerate() {
        let trimmed = c.display_name.trim();
        if trimmed.is_empty() {
            out[i].display = true;
        } else {
            let key = trimmed.to_ascii_lowercase();
            if display_counts.get(&key).copied().unwrap_or(0) > 1 {
                out[i].display = true;
            }
        }
    }

    // folder: duplicate slug, reserved, empty, duplicate, path
    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    let mut slugs: Vec<String> = Vec::new();
    for c in categories {
        let s = slugify(&c.folder_name);
        slugs.push(s.clone());
        if !s.is_empty() {
            *slug_counts.entry(s).or_default() += 1;
        }
    }
    for (i, c) in categories.iter().enumerate() {
        let slug = &slugs[i];
        if slug.is_empty() {
            out[i].folder = true;
            continue;
        }
        if slug == "duplicate" {
            out[i].folder = true;
        }
        if c.folder_name.contains('/') || c.folder_name.contains('\\') || c.folder_name.trim() == ".." {
            out[i].folder = true;
        }
        if slug_counts.get(slug).copied().unwrap_or(0) > 1 {
            out[i].folder = true;
        }
    }

    // shortcut duplicates / invalid
    let mut sc_counts: HashMap<String, usize> = HashMap::new();
    for c in categories {
        let s = c.shortcut.trim();
        if s.len() == 1 && matches!(s.chars().next().unwrap(), '1'..='9') {
            *sc_counts.entry(s.to_string()).or_default() += 1;
        }
    }
    for (i, c) in categories.iter().enumerate() {
        let s = c.shortcut.trim();
        if s.len() != 1 || !matches!(s.chars().next().unwrap_or(' '), '1'..='9') {
            out[i].shortcut = true;
        } else if sc_counts.get(s).copied().unwrap_or(0) > 1 {
            out[i].shortcut = true;
        }
    }
    out.into_iter().map(|e| RowError { display: e.display, folder: e.folder, shortcut: e.shortcut }).collect()
}

fn swap_categories(categories: &mut Vec<Category>, i: usize, j: usize) {
    if i < categories.len() && j < categories.len() {
        categories.swap(i, j);
    }
}

/// Open modal settings dialog. Uses AdwPreferencesWindow when libadwaita is available,
/// fallback is plain gtk4::Window. Shows 3-field rows + Add/Remove/Up/Down,
/// live slug preview, inline per-row errors, Save sensitive=valid, Add disabled at 9,
/// live in-memory apply, Save via temp+rename, Cancel reverts, close prompts.
pub fn open_settings(parent: &ApplicationWindow, ctx: SettingsContext) {
    // Prefer AdwPreferencesWindow for GNOME HIG searchability; fallback to gtk Window if needed
    // We always use AdwPreferencesWindow since libadwaita is a dependency (ADR 0006)
    let adw_win = adw::PreferencesWindow::builder()
        .transient_for(parent)
        .modal(true)
        .title("Settings — Categories")
        .default_width(780)
        .default_height(560)
        .build();

    // PreferencesWindow content: single page + group containing our custom rows
    let page = adw::PreferencesPage::new();
    page.set_title("Categories");
    page.set_description("Organizer Database inside the Source Folder — copied with the folder, slug a-z0-9_-");
    adw_win.add(&page);

    let group = adw::PreferencesGroup::new();
    group.set_title("Category mappings");
    group.set_description(Some("Display name, folder name, shortcut 1-9 — max 9, min 1"));
    page.add(&group);

    // Custom container for rows (inside group)
    let rows_box = GtkBox::new(Orientation::Vertical, 8);
    rows_box.set_margin_top(8);
    group.add(&rows_box);

    // Validation label below rows (outside group but inside page via box)
    let validation_label = Label::new(None);
    validation_label.add_css_class("error");
    validation_label.set_wrap(true);
    validation_label.set_halign(gtk4::Align::Start);
    validation_label.set_margin_top(8);
    rows_box.append(&validation_label);

    // Bottom bar: Add / Save / Cancel (placed below group via rows_box)
    let bottom = GtkBox::new(Orientation::Horizontal, 8);
    bottom.set_margin_top(12);
    let btn_add = Button::with_label("Add Category");
    btn_add.set_tooltip_text(Some("Add new category (max 9)"));
    let btn_save = Button::with_label("Save");
    btn_save.add_css_class("suggested-action");
    let btn_cancel = Button::with_label("Cancel");
    bottom.append(&btn_add);
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    bottom.append(&spacer);
    bottom.append(&btn_cancel);
    bottom.append(&btn_save);
    // Add bottom as separate group to keep inside PreferencesWindow's content
    let bottom_group = adw::PreferencesGroup::new();
    bottom_group.add(&bottom);
    page.add(&bottom_group);

    // ui_categories mirrors edits
    let ui_categories: Rc<RefCell<Vec<Category>>> = Rc::new(RefCell::new(ctx.live_categories.borrow().clone()));
    let handles: Rc<RefCell<Vec<RowHandle>>> = Rc::new(RefCell::new(Vec::new()));

    // We need a rebuild closure that can be called after any mutation that changes order or length.
    // Use Rc<RefCell<Option<Rc<dyn Fn()>>>> to allow self-reference.
    let rebuild_holder: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    // Validate + live-apply helper (per-row errors)
    let validate_and_live = {
        let ui_clone = ui_categories.clone();
        let validation_c = validation_label.clone();
        let btn_save_c = btn_save.clone();
        let btn_add_c = btn_add.clone();
        let live_c = ctx.live_categories.clone();
        let rebuild_c = ctx.rebuild_category_bar.clone();
        let handles_c = handles.clone();
        Rc::new(move || {
            let categories = ui_clone.borrow().clone();
            // update dropdown tooltips for conflict preview
            {
                let hs = handles_c.borrow();
                for h in hs.iter() {
                    let cur = format!("{}", h.dropdown.selected() + 1);
                    let count = categories.iter().filter(|c| c.shortcut == cur).count();
                    if count > 1 {
                        if let Some(owner) = categories.iter().find(|c| c.shortcut == cur) {
                            h.dropdown.set_tooltip_text(Some(&format!("Shortcut {} already used by '{}'", cur, owner.display_name)));
                        }
                    } else {
                        h.dropdown.set_tooltip_text(Some(&format!("Shortcut {} — {} or Ctrl+{}", cur, cur, cur)));
                    }
                    // update slug preview (already via entry handler, but keep in sync)
                    // not needed here
                }
            }

            let per_row = per_row_errors(&categories);
            match validate_categories(&categories) {
                Ok(()) => {
                    validation_c.set_text("");
                    btn_save_c.set_sensitive(true);
                    for (h, err) in handles_c.borrow().iter().zip(per_row.iter()) {
                        if err.display { h.display.add_css_class("error"); } else { h.display.remove_css_class("error"); }
                        if err.folder { h.folder.add_css_class("error"); } else { h.folder.remove_css_class("error"); }
                        if err.shortcut { h.dropdown.add_css_class("error"); } else { h.dropdown.remove_css_class("error"); }
                    }
                    *live_c.borrow_mut() = categories.clone();
                    rebuild_c();
                }
                Err(errs) => {
                    let msg = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
                    validation_c.set_text(&msg);
                    btn_save_c.set_sensitive(false);
                    for (h, err) in handles_c.borrow().iter().zip(per_row.iter()) {
                        if err.display { h.display.add_css_class("error"); } else { h.display.remove_css_class("error"); }
                        if err.folder { h.folder.add_css_class("error"); } else { h.folder.remove_css_class("error"); }
                        if err.shortcut { h.dropdown.add_css_class("error"); } else { h.dropdown.remove_css_class("error"); }
                    }
                }
            }
            btn_add_c.set_sensitive(ui_clone.borrow().len() < 9);
            if ui_clone.borrow().len() >= 9 {
                btn_add_c.set_tooltip_text(Some("Max 9 categories"));
            } else {
                btn_add_c.set_tooltip_text(Some("Add new category (max 9)"));
            }
            // update Up/Down/Remove sensitivities
            let len = handles_c.borrow().len();
            for (idx, h) in handles_c.borrow().iter().enumerate() {
                let up_sensitive = idx > 0;
                let down_sensitive = idx + 1 < len;
                let mut child = h.container.first_child();
                while let Some(c) = child {
                    if let Some(btn) = c.downcast_ref::<Button>() {
                        if let Some(t) = btn.tooltip_text() {
                            if t == "Move up" {
                                btn.set_sensitive(up_sensitive);
                            } else if t == "Move down" {
                                btn.set_sensitive(down_sensitive);
                            } else if t.contains("Remove") || t.contains("At least") {
                                btn.set_sensitive(len > 1);
                                if len <= 1 {
                                     btn.set_tooltip_text(Some("At least 1 category required"));
                                } else {
                                    btn.set_tooltip_text(Some("Remove"));
                                }
                            }
                        }
                    }
                    child = c.next_sibling();
                }
            }
        }) as Rc<dyn Fn()>
    };

    // Rebuild all rows from ui_categories (fresh indices)
    let rebuild_rows = {
        let ui_clone = ui_categories.clone();
        let rows_box_c = rows_box.clone();
        let handles_c = handles.clone();
        let validate_c = validate_and_live.clone();
        let rebuild_holder_c = rebuild_holder.clone();
        Rc::new(move || {
            // clear rows_box children except validation_label and bottom? Actually validation_label and bottom are not in rows_box's rows; rows_box contains rows + validation_label?
            // We appended validation_label inside rows_box earlier, so clearing would remove it. Instead we keep validation_label separate.
            // To avoid removing validation_label, we remove only RowHandle containers.
            for h in handles_c.borrow().iter() {
                rows_box_c.remove(&h.container);
            }
            handles_c.borrow_mut().clear();

            let categories = ui_clone.borrow().clone();
            for (idx, cat) in categories.into_iter().enumerate() {
                let row = GtkBox::new(Orientation::Horizontal, 8);
                row.set_margin_bottom(4);

                let display_entry = Entry::new();
                display_entry.set_placeholder_text(Some("Display name"));
                display_entry.set_text(&cat.display_name);
                display_entry.set_hexpand(true);
                display_entry.set_width_chars(12);

                let folder_entry = Entry::new();
                folder_entry.set_placeholder_text(Some("Folder name"));
                folder_entry.set_text(&cat.folder_name);
                folder_entry.set_hexpand(true);
                folder_entry.set_width_chars(12);

                let slug = slugify(&cat.folder_name);
                let slug_label = Label::new(Some(&format!("→ ../{}/", if slug.is_empty() { "—".to_string() } else { slug.clone() })));
                slug_label.add_css_class("dim-label");
                slug_label.set_width_chars(14);
                slug_label.set_xalign(0.0);
                    slug_label.set_tooltip_text(Some(&format!("subfolder {}/, slug a-z0-9_-", slug)));

                let list = gtk4::StringList::new(&["1","2","3","4","5","6","7","8","9"]);
                let dropdown = gtk4::DropDown::new(Some(list), None::<gtk4::Expression>);
                let sel = cat.shortcut.parse::<u32>().ok().and_then(|n| if (1..=9).contains(&n) { Some(n-1) } else { None }).unwrap_or(0);
                dropdown.set_selected(sel);
                dropdown.set_tooltip_text(Some(&format!("Shortcut {} — {} or Ctrl+{}", cat.shortcut, cat.shortcut, cat.shortcut)));

                let btn_up = Button::builder().icon_name("go-up-symbolic").tooltip_text("Move up").build();
                let btn_down = Button::builder().icon_name("go-down-symbolic").tooltip_text("Move down").build();
                let btn_remove = Button::builder().icon_name("edit-delete-symbolic").tooltip_text("Remove").build();
                btn_remove.add_css_class("destructive-action");

                row.append(&display_entry);
                row.append(&folder_entry);
                row.append(&slug_label);
                row.append(&dropdown);
                row.append(&btn_up);
                row.append(&btn_down);
                row.append(&btn_remove);

                // Insert before validation_label: remove validation_label, append row, then re-append validation_label
                // Since rows_box contains validation_label at end, we need to keep order: insert row before validation_label
                // Remove validation_label temporarily, append row, re-add validation_label
                let had_validation = rows_box_c.first_child().is_some() && {
                    // check if validation_label is child
                    let mut found = false;
                    let mut c = rows_box_c.first_child();
                    while let Some(ch) = c {
                        if ch == validation_label.clone().upcast::<gtk4::Widget>() {
                            found = true;
                            break;
                        }
                        c = ch.next_sibling();
                    }
                    found
                };
                if had_validation {
                    rows_box_c.remove(&validation_label);
                }
                rows_box_c.append(&row);
                rows_box_c.append(&validation_label);

                handles_c.borrow_mut().push(RowHandle {
                    container: row.clone(),
                    display: display_entry.clone(),
                    folder: folder_entry.clone(),
                    slug: slug_label.clone(),
                    dropdown: dropdown.clone(),
                });

                // Wire signals capturing idx (fresh)
                {
                    let ui_c = ui_clone.clone();
                    let validate_cc = validate_c.clone();
                    display_entry.connect_changed(move |e| {
                        let txt = e.text().to_string();
                        if let Some(a) = ui_c.borrow_mut().get_mut(idx) {
                            a.display_name = txt;
                        }
                        validate_cc();
                    });
                }
                {
                    let ui_c = ui_clone.clone();
                    let slug_c = slug_label.clone();
                    let validate_cc = validate_c.clone();
                    folder_entry.connect_changed(move |e| {
                        let txt = e.text().to_string();
                        let slug = slugify(&txt);
                        slug_c.set_text(&format!("→ ../{}/", if slug.is_empty() { "—".into() } else { slug.clone() }));
                        slug_c.set_tooltip_text(Some(&format!("subfolder {}/, slug a-z0-9_-", slug)));
                        if let Some(a) = ui_c.borrow_mut().get_mut(idx) {
                            a.folder_name = txt;
                        }
                        validate_cc();
                    });
                }
                {
                    let ui_c = ui_clone.clone();
                    let validate_cc = validate_c.clone();
                    dropdown.connect_selected_notify(move |dd| {
                        let sc = format!("{}", dd.selected() + 1);
                        if let Some(a) = ui_c.borrow_mut().get_mut(idx) {
                            a.shortcut = sc;
                        }
                        validate_cc();
                    });
                }
                // Up
                {
                    let ui_c = ui_clone.clone();
                    let rebuild_c = rebuild_holder_c.clone();
                    btn_up.connect_clicked(move |_| {
                        if idx == 0 { return; }
                        {
                            let mut v = ui_c.borrow_mut();
                            swap_categories(&mut v, idx, idx - 1);
                        }
                        if let Some(rb) = rebuild_c.borrow().as_ref() {
                            rb();
                        }
                    });
                }
                // Down
                {
                    let ui_c = ui_clone.clone();
                    let rebuild_c = rebuild_holder_c.clone();
                    btn_down.connect_clicked(move |_| {
                        let len = ui_c.borrow().len();
                        if idx + 1 >= len { return; }
                        {
                            let mut v = ui_c.borrow_mut();
                            swap_categories(&mut v, idx, idx + 1);
                        }
                        if let Some(rb) = rebuild_c.borrow().as_ref() {
                            rb();
                        }
                    });
                }
                // Remove
                {
                    let ui_c = ui_clone.clone();
                    let rebuild_c = rebuild_holder_c.clone();
                    btn_remove.connect_clicked(move |_| {
                        if ui_c.borrow().len() <= 1 { return; }
                        {
                            let mut v = ui_c.borrow_mut();
                            if idx < v.len() { v.remove(idx); }
                        }
                        if let Some(rb) = rebuild_c.borrow().as_ref() {
                            rb();
                        }
                    });
                }
            }
            validate_c();
        }) as Rc<dyn Fn()>
    };
    *rebuild_holder.borrow_mut() = Some(rebuild_rows.clone());

    // Initial build
    rebuild_rows();

    // Add button: push new category and rebuild
    {
        let ui_c = ui_categories.clone();
        let rebuild_c = rebuild_rows.clone();
        btn_add.connect_clicked(move |_| {
            if ui_c.borrow().len() >= 9 { return; }
            let used: HashSet<String> = ui_c.borrow().iter().map(|c| c.shortcut.clone()).collect();
            let mut new_short = "1".to_string();
            for n in 1..=9 {
                let s = format!("{n}");
                if !used.contains(&s) {
                    new_short = s;
                    break;
                }
            }
            let new_cat = Category { display_name: "New Category".into(), folder_name: "new_category".into(), shortcut: new_short };
            ui_c.borrow_mut().push(new_cat);
            rebuild_c();
        });
    }

    // Save
    {
        let win_c = adw_win.clone();
        let store_c = ctx.store.clone();
        let ui_c = ui_categories.clone();
        let live_c = ctx.live_categories.clone();
        let disk_c = ctx.disk_categories.clone();
        let rebuild_c = ctx.rebuild_category_bar.clone();
        btn_save.connect_clicked(move |_| {
            let collected = ui_c.borrow().clone();
            if validate_categories(&collected).is_err() { return; }
            match store_c.set_categories(&collected) {
                Ok(()) => {
                    *live_c.borrow_mut() = collected.clone();
                    *disk_c.borrow_mut() = collected.clone();
                    rebuild_c();
                    win_c.close();
                }
                Err(e) => {
                    let dlg = gtk4::AlertDialog::builder().message(format!("Failed to save: {e}")).build();
                    dlg.show(Some(&win_c));
                }
            }
        });
    }
    // Cancel
    {
        let win_c = adw_win.clone();
        let live_c = ctx.live_categories.clone();
        let disk_c = ctx.disk_categories.clone();
        let rebuild_c = ctx.rebuild_category_bar.clone();
        btn_cancel.connect_clicked(move |_| {
            *live_c.borrow_mut() = disk_c.borrow().clone();
            rebuild_c();
            win_c.close();
        });
    }

    // Close without Save prompts Save/Discard/Cancel
    {
        let ui_c = ui_categories.clone();
        let live_c = ctx.live_categories.clone();
        let disk_c = ctx.disk_categories.clone();
        let store_c = ctx.store.clone();
        let rebuild_c = ctx.rebuild_category_bar.clone();
        adw_win.connect_close_request(move |w| {
            let collected = ui_c.borrow().clone();
            let disk = disk_c.borrow().clone();
            if collected == disk {
                return glib::Propagation::Proceed;
            }
            let alert = gtk4::AlertDialog::builder()
                .message("Save changes?")
                .detail("You have unsaved changes to Categories. Save, discard, or cancel?")
                .buttons(vec!["Save", "Discard", "Cancel"])
                .default_button(2)
                .cancel_button(2)
                .build();
            let win_clone = w.clone();
            let store_clone = store_c.clone();
            let live_clone = live_c.clone();
            let disk_clone = disk_c.clone();
            let rebuild_clone = rebuild_c.clone();
            let collected_clone = collected.clone();
            alert.choose(Some(w), gio::Cancellable::NONE, move |res| {
                if let Ok(idx) = res {
                    match idx {
                        0 => {
                            let _ = store_clone.set_categories(&collected_clone);
                            *live_clone.borrow_mut() = collected_clone.clone();
                            *disk_clone.borrow_mut() = collected_clone.clone();
                            rebuild_clone();
                            win_clone.close();
                        }
                        1 => {
                            *live_clone.borrow_mut() = disk_clone.borrow().clone();
                            rebuild_clone();
                            win_clone.close();
                        }
                        _ => {}
                    }
                }
            });
            glib::Propagation::Stop
        });
    }

    adw_win.present();
}
