# Settings as AdwPreferencesWindow, 3-field rows, strict validation, live+Save

Settings is a modal `AdwPreferencesWindow` (libadwaita, fallback `GtkDialog` if not bundled) opened via `Ctrl+,`, gear button in the Action Bar, or `Edit → Preferences`. It blocks the main window and is searchable per GNOME HIG.

Each Action row has `Display name` [Entry], `Folder name` [Entry, helper “subfolder <folder>/, slug a-z0-9_-”], and `Shortcut` [DropDown 1–9 showing “1 or Ctrl+1”]; row controls are `Add Action` / `Remove` (×) / `Up`/`Down` reorder. Max 9 rows, min 1, no color/icon v1. The `Folder name` field live-sanitizes and previews the slug.

Validation is strict and inline: `display_name` trimmed non-empty unique case-insensitive; `folder_name` slug `a-z0-9_-` (lower, spaces→ `_`, strip `[^a-z0-9_-]`), unique, not `duplicate` or empty after slug, not `..`/`/`; `shortcut` unique 1–9; violations show `error` CSS + helper text and disable Save (`sensitive = valid`), Add disabled at 9 with tooltip.

Action Bar buttons show a shortcut hint (`Keep [1]` badge) with tooltip `Keep — 1 or Ctrl+1`; the Settings dropdown previews conflicts (“Shortcut 2 already used by ‘Maybe’”). Edits update the in-memory `Vec<Action>` live for the next keypress, but `SourceFolder/organizer.toml` is only written on Save (`write temp + rename` + `flock`+`fsync`); Cancel reverts in-memory to disk, and close without Save prompts Save/Discard/Cancel. Window geometry is not persisted (always maximized). Considered: `GtkDialog` only, looser duplicate-allow, auto-save on keystroke, no hints.
