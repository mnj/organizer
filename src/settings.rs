use gtk4::prelude::*;
use gtk4::{gio, glib, ApplicationWindow, Box as GtkBox, Button, Entry, Label, Orientation};
use libadwaita as adw;
use adw::prelude::*;
use crate::config::{
    canonicalize_categories, first_free_shortcut, slugify, suggest_unique_category,
    validate_categories, Category,
};
use crate::store::Store;
use std::cell::RefCell;
use std::collections::HashMap;
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

/// Validate the draft, persist slug folder names, and only then update live/disk.
/// Returns Err without touching live/disk when validation or the database write fails.
fn persist_categories(
    store: &Store,
    categories: &[Category],
    live: &Rc<RefCell<Vec<Category>>>,
    disk: &Rc<RefCell<Vec<Category>>>,
    rebuild_bar: &Rc<dyn Fn()>,
) -> Result<(), String> {
    if let Err(errs) = validate_categories(categories) {
        return Err(errs
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n"));
    }
    let canonical = canonicalize_categories(categories);
    store
        .set_categories(&canonical)
        .map_err(|e| e.to_string())?;
    *live.borrow_mut() = canonical.clone();
    *disk.borrow_mut() = canonical;
    rebuild_bar();
    Ok(())
}

/// Whether closing the settings dialog must prompt Save/Discard/Cancel.
/// Pure seam for the close_request handler: prompt exactly when the draft
/// differs from the last-saved disk state.
pub(crate) fn needs_save_prompt(draft: &[Category], disk: &[Category]) -> bool {
    draft != disk
}

/// Reconcile state on Discard/Cancel: restore both live preview and draft
/// to disk so a follow-up close_request sees a clean state and proceeds.
/// Forgetting draft here re-prompts forever (discard loop).
pub(crate) fn apply_discard(
    live: &Rc<RefCell<Vec<Category>>>,
    draft: &Rc<RefCell<Vec<Category>>>,
    disk: &[Category],
) {
    *live.borrow_mut() = disk.to_vec();
    *draft.borrow_mut() = disk.to_vec();
}

/// Reconcile draft after a successful Save: draft tracks the canonical disk
/// state so a follow-up close_request proceeds instead of re-prompting.
pub(crate) fn apply_saved(draft: &Rc<RefCell<Vec<Category>>>, disk: &[Category]) {
    *draft.borrow_mut() = disk.to_vec();
}

fn show_save_error(parent: &adw::PreferencesWindow, message: &str) {
    let dlg = gtk4::AlertDialog::builder()
        .message(format!("Failed to save: {message}"))
        .build();
    dlg.show(Some(parent));
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

    // draft_categories mirrors edits
    let draft_categories: Rc<RefCell<Vec<Category>>> = Rc::new(RefCell::new(ctx.live_categories.borrow().clone()));
    let handles: Rc<RefCell<Vec<RowHandle>>> = Rc::new(RefCell::new(Vec::new()));

    // We need a rebuild closure that can be called after any mutation that changes order or length.
    // Use Rc<RefCell<Option<Rc<dyn Fn()>>>> to allow self-reference.
    let rebuild_holder: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));

    // Validate + live-apply helper (per-row errors)
    let validate_and_live = {
        let draft = draft_categories.clone();
        let validation = validation_label.clone();
        let save_button = btn_save.clone();
        let add_button = btn_add.clone();
        let live = ctx.live_categories.clone();
        let rebuild_bar = ctx.rebuild_category_bar.clone();
        let handles = handles.clone();
        Rc::new(move || {
            let categories = draft.borrow().clone();
            // update dropdown tooltips for conflict preview
            {
                let hs = handles.borrow();
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
                    validation.set_text("");
                    save_button.set_sensitive(true);
                    for (h, err) in handles.borrow().iter().zip(per_row.iter()) {
                        if err.display { h.display.add_css_class("error"); } else { h.display.remove_css_class("error"); }
                        if err.folder { h.folder.add_css_class("error"); } else { h.folder.remove_css_class("error"); }
                        if err.shortcut { h.dropdown.add_css_class("error"); } else { h.dropdown.remove_css_class("error"); }
                    }
                    // Live Classification uses slugs; the form keeps the raw typed text.
                    *live.borrow_mut() = canonicalize_categories(&categories);
                    rebuild_bar();
                }
                Err(errs) => {
                    let msg = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
                    validation.set_text(&msg);
                    save_button.set_sensitive(false);
                    for (h, err) in handles.borrow().iter().zip(per_row.iter()) {
                        if err.display { h.display.add_css_class("error"); } else { h.display.remove_css_class("error"); }
                        if err.folder { h.folder.add_css_class("error"); } else { h.folder.remove_css_class("error"); }
                        if err.shortcut { h.dropdown.add_css_class("error"); } else { h.dropdown.remove_css_class("error"); }
                    }
                }
            }
            add_button.set_sensitive(draft.borrow().len() < 9);
            if draft.borrow().len() >= 9 {
                add_button.set_tooltip_text(Some("Max 9 categories"));
            } else {
                add_button.set_tooltip_text(Some("Add new category (max 9)"));
            }
            // update Up/Down/Remove sensitivities
            let len = handles.borrow().len();
            for (idx, h) in handles.borrow().iter().enumerate() {
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

    // Rebuild all rows from draft_categories (fresh indices)
    let rebuild_rows = {
        let draft = draft_categories.clone();
        let rows_box = rows_box.clone();
        let handles = handles.clone();
        let revalidate = validate_and_live.clone();
        let rebuild_holder = rebuild_holder.clone();
        Rc::new(move || {
            // clear rows_box children except validation_label and bottom? Actually validation_label and bottom are not in rows_box's rows; rows_box contains rows + validation_label?
            // We appended validation_label inside rows_box earlier, so clearing would remove it. Instead we keep validation_label separate.
            // To avoid removing validation_label, we remove only RowHandle containers.
            for h in handles.borrow().iter() {
                rows_box.remove(&h.container);
            }
            handles.borrow_mut().clear();

            let categories = draft.borrow().clone();
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
                let had_validation = rows_box.first_child().is_some() && {
                    // check if validation_label is child
                    let mut found = false;
                    let mut c = rows_box.first_child();
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
                    rows_box.remove(&validation_label);
                }
                rows_box.append(&row);
                rows_box.append(&validation_label);

                handles.borrow_mut().push(RowHandle {
                    container: row.clone(),
                    display: display_entry.clone(),
                    folder: folder_entry.clone(),
                    slug: slug_label.clone(),
                    dropdown: dropdown.clone(),
                });

                // Wire signals capturing idx (fresh)
                {
                    let draft = draft.clone();
                    let revalidate = revalidate.clone();
                    display_entry.connect_changed(move |e| {
                        let txt = e.text().to_string();
                        if let Some(cat) = draft.borrow_mut().get_mut(idx) {
                            cat.display_name = txt;
                        }
                        revalidate();
                    });
                }
                {
                    let draft = draft.clone();
                    let slug_view = slug_label.clone();
                    let revalidate = revalidate.clone();
                    folder_entry.connect_changed(move |e| {
                        let txt = e.text().to_string();
                        let slug = slugify(&txt);
                        slug_view.set_text(&format!("→ ../{}/", if slug.is_empty() { "—".into() } else { slug.clone() }));
                        slug_view.set_tooltip_text(Some(&format!("subfolder {}/, slug a-z0-9_-", slug)));
                        if let Some(cat) = draft.borrow_mut().get_mut(idx) {
                            cat.folder_name = txt;
                        }
                        revalidate();
                    });
                }
                {
                    let draft = draft.clone();
                    let revalidate = revalidate.clone();
                    dropdown.connect_selected_notify(move |dd| {
                        let sc = format!("{}", dd.selected() + 1);
                        if let Some(cat) = draft.borrow_mut().get_mut(idx) {
                            cat.shortcut = sc;
                        }
                        revalidate();
                    });
                }
                // Up
                {
                    let draft = draft.clone();
                    let rerender = rebuild_holder.clone();
                    btn_up.connect_clicked(move |_| {
                        if idx == 0 { return; }
                        {
                            let mut v = draft.borrow_mut();
                            swap_categories(&mut v, idx, idx - 1);
                        }
                        if let Some(rb) = rerender.borrow().as_ref() {
                            rb();
                        }
                    });
                }
                // Down
                {
                    let draft = draft.clone();
                    let rerender = rebuild_holder.clone();
                    btn_down.connect_clicked(move |_| {
                        let len = draft.borrow().len();
                        if idx + 1 >= len { return; }
                        {
                            let mut v = draft.borrow_mut();
                            swap_categories(&mut v, idx, idx + 1);
                        }
                        if let Some(rb) = rerender.borrow().as_ref() {
                            rb();
                        }
                    });
                }
                // Remove
                {
                    let draft = draft.clone();
                    let rerender = rebuild_holder.clone();
                    btn_remove.connect_clicked(move |_| {
                        if draft.borrow().len() <= 1 { return; }
                        {
                            let mut v = draft.borrow_mut();
                            if idx < v.len() { v.remove(idx); }
                        }
                        if let Some(rb) = rerender.borrow().as_ref() {
                            rb();
                        }
                    });
                }
            }
            revalidate();
        }) as Rc<dyn Fn()>
    };
    *rebuild_holder.borrow_mut() = Some(rebuild_rows.clone());

    // Initial build
    rebuild_rows();

    // Add button: push a uniquely-named category and rebuild, so repeated
    // Adds stay valid (suffixed "New Category 2", …) instead of piling up
    // duplicates the validator would reject.
    {
        let draft = draft_categories.clone();
        let rerender = rebuild_rows.clone();
        btn_add.connect_clicked(move |_| {
            if draft.borrow().len() >= 9 { return; }
            let current = draft.borrow().clone();
            let Some(mut new_cat) = suggest_unique_category(&current, "New Category") else { return };
            let Some(short) = first_free_shortcut(&current) else { return };
            new_cat.shortcut = short;
            draft.borrow_mut().push(new_cat);
            rerender();
        });
    }

    // Save
    // allow_close bypasses the close_request prompt after an explicit
    // Save/Discard/Cancel already reconciled draft with disk. Without it,
    // dialog.close() re-enters close_request, sees stale draft != disk,
    // and re-prompts forever (discard loop).
    let allow_close: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    {
        let dialog = adw_win.clone();
        let store = ctx.store.clone();
        let draft = draft_categories.clone();
        let live = ctx.live_categories.clone();
        let disk = ctx.disk_categories.clone();
        let rebuild_bar = ctx.rebuild_category_bar.clone();
        let allow_close = allow_close.clone();
        btn_save.connect_clicked(move |_| {
            let collected = draft.borrow().clone();
            match persist_categories(&store, &collected, &live, &disk, &rebuild_bar) {
                Ok(()) => {
                    let saved = disk.borrow().clone();
                    apply_saved(&draft, &saved);
                    *allow_close.borrow_mut() = true;
                    dialog.close()
                }
                Err(e) => show_save_error(&dialog, &e),
            }
        });
    }
    // Cancel
    {
        let dialog = adw_win.clone();
        let draft = draft_categories.clone();
        let live = ctx.live_categories.clone();
        let disk = ctx.disk_categories.clone();
        let rebuild_bar = ctx.rebuild_category_bar.clone();
        let allow_close = allow_close.clone();
        btn_cancel.connect_clicked(move |_| {
            let saved = disk.borrow().clone();
            apply_discard(&live, &draft, &saved);
            rebuild_bar();
            *allow_close.borrow_mut() = true;
            dialog.close();
        });
    }

    // Close without Save prompts Save/Discard/Cancel
    {
        let draft = draft_categories.clone();
        let live = ctx.live_categories.clone();
        let disk = ctx.disk_categories.clone();
        let store = ctx.store.clone();
        let rebuild_bar = ctx.rebuild_category_bar.clone();
        let allow_close = allow_close.clone();
        adw_win.connect_close_request(move |w| {
            if *allow_close.borrow() {
                return glib::Propagation::Proceed;
            }
            let collected = draft.borrow().clone();
            let saved = disk.borrow().clone();
            if !needs_save_prompt(&collected, &saved) {
                return glib::Propagation::Proceed;
            }
            let alert = gtk4::AlertDialog::builder()
                .message("Save changes?")
                .detail("You have unsaved changes to Categories. Save, discard, or cancel?")
                .buttons(vec!["Save", "Discard", "Cancel"])
                .default_button(2)
                .cancel_button(2)
                .build();
            let dialog = w.clone();
            let store = store.clone();
            let draft = draft.clone();
            let live = live.clone();
            let disk = disk.clone();
            let rebuild_bar = rebuild_bar.clone();
            let allow_close = allow_close.clone();
            let collected = collected.clone();
            alert.choose(Some(w), gio::Cancellable::NONE, move |res| {
                if let Ok(idx) = res {
                    match idx {
                        0 => {
                            match persist_categories(&store, &collected, &live, &disk, &rebuild_bar) {
                                Ok(()) => {
                                    let saved = disk.borrow().clone();
                                    apply_saved(&draft, &saved);
                                    *allow_close.borrow_mut() = true;
                                    dialog.close()
                                }
                                Err(e) => show_save_error(&dialog, &e),
                            }
                        }
                        1 => {
                            let saved = disk.borrow().clone();
                            apply_discard(&live, &draft, &saved);
                            rebuild_bar();
                            *allow_close.borrow_mut() = true;
                            dialog.close();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Category;

    fn cats(names: &[&str]) -> Vec<Category> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| Category {
                display_name: (*n).into(),
                folder_name: n.to_ascii_lowercase().replace(' ', "_"),
                shortcut: format!("{}", i + 1),
            })
            .collect()
    }

    #[test]
    fn close_prompts_only_when_draft_differs_from_disk() {
        let disk = cats(&["Keep", "Maybe"]);
        let clean = disk.clone();
        assert!(!needs_save_prompt(&clean, &disk));
        let mut dirty = disk.clone();
        dirty.push(Category {
            display_name: "New".into(),
            folder_name: "new".into(),
            shortcut: "3".into(),
        });
        assert!(needs_save_prompt(&dirty, &disk));
    }

    #[test]
    fn discard_resets_draft_so_second_close_proceeds() {
        // Regression for the discard infinite-prompt loop: Discard must
        // reconcile both live and draft, otherwise close_request re-prompts.
        let disk = cats(&["Keep", "Maybe"]);
        let live: Rc<RefCell<Vec<Category>>> =
            Rc::new(RefCell::new(cats(&["Keep", "Maybe", "New"])));
        let draft: Rc<RefCell<Vec<Category>>> = Rc::new(RefCell::new(live.borrow().clone()));
        assert!(needs_save_prompt(&draft.borrow(), &disk));
        let saved = disk.clone();
        apply_discard(&live, &draft, &saved);
        assert_eq!(*live.borrow(), disk);
        assert!(!needs_save_prompt(&draft.borrow(), &disk));
    }

    #[test]
    fn saved_draft_tracks_disk_so_second_close_proceeds() {
        let disk = cats(&["Keep", "Maybe"]);
        let draft: Rc<RefCell<Vec<Category>>> =
            Rc::new(RefCell::new(cats(&["Keep", "Maybe", "New"])));
        assert!(needs_save_prompt(&draft.borrow(), &disk));
        apply_saved(&draft, &disk);
        assert!(!needs_save_prompt(&draft.borrow(), &disk));
    }
}
