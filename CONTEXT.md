# Organizer

File-triaging desktop app that shows one image at a time and moves it to a chosen category folder with shortcuts, while tracking duplicates by content hash.

## Language

**Source Folder**:
The single flat folder the user picks at launch (CLI arg or picker) whose direct children are triaged.
_Avoid_: Input directory, working directory

**Queue (Snapshot)**:
The immutable, alphabetically sorted list of supported files captured at launch from the Source Folder; not a live watch.
_Avoid_: File list, directory listing

**Current File**:
The file at the queue index currently shown in preview; the target of Classification or the next duplicate check.
_Avoid_: Selected file, active file

**Supported Format**:
A file extension decodeable by glycin (png, jpg/jpeg, bmp, tiff/tif, webp, gif-anim, avif/heic/heif/svg/ico) or GStreamer (webm/mp4/mov/mkv/avi); only these enter the Queue.
_Avoid_: Image type, valid file

**Unsupported File**:
A file with an extension outside Supported Format; shown as a placeholder card with filename and “Unsupported” badge, skippable via Next/Prev but never moved or hashed.
_Avoid_: Invalid file, skipped file

**Category**:
A user-configured triage destination with a display name, folder name (subfolder of Source Folder), and shortcut; moving Current File to it records its sha256 in the Organizer Database.
_Avoid_: Action (as destination), label

**Action**:
A Sweep operation applied to matched files (`report`, `trash`, or `perm-delete`); chosen after Category toggles, never a triage destination.
_Avoid_: Disposition, print, cleanup action

**Organizer Database**:
The single `organizer.db` sqlite file inside the Source Folder that stores Categories and triaged file hashes with Source Folder-relative paths; it replaces `organizer.toml` and `*.txt` logs.
_Avoid_: txt log, config file

**Classification**:
The GUI manual triage that assigns Current File to a Category.
_Avoid_: categorization, cleanup

**Sweep**:
The CLI operation that checks a Target Folder against a reference Organizer Database and applies an Action (`report`, `trash`, `perm-delete`) to files whose sha256 is already known; it never classifies or moves to Category subfolders.
_Avoid_: cleanup action, categorize

**Duplicate**:
A file whose sha256 already exists in the Organizer Database; in Classification routed to the `duplicate/` subfolder instead of the chosen Category, in Sweep matched for its Action.
_Avoid_: Copy, clone

**Preview**:
The top split-view area that renders Current File via `gtk::Picture`/`Paintable` (Texture for stills/animated, GStreamer paintable for video).
_Avoid_: Viewer, canvas

**Action Bar**:
The bottom split-view area with Category buttons, Prev/Next, index counter, and Undo.
_Avoid_: Toolbar, bottom panel

**Snapshot**:
The point-in-time capture semantics — Queue does not reflect external files added/removed mid-session until relaunch.
_Avoid_: Live view, watched folder
