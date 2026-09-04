# Organizer

File-triaging desktop app that shows one image at a time and moves it to a chosen action folder with shortcuts, while tracking duplicates by content hash.

## Language

**Source Folder**:
The single flat folder the user picks at launch (CLI arg or picker) whose direct children are triaged.
_Avoid_: Input directory, working directory

**Queue (Snapshot)**:
The immutable, alphabetically sorted list of supported files captured at launch from the Source Folder; not a live watch.
_Avoid_: File list, directory listing

**Current File**:
The file at the queue index currently shown in preview; the target of the next action or duplicate check.
_Avoid_: Selected file, active file

**Supported Format**:
A file extension decodeable by glycin (png, jpg/jpeg, bmp, tiff, webp, gif-anim, avif/heic/svg/ico) or GStreamer (webm/mp4/mov/mkv/avi); only these enter the Queue.
_Avoid_: Image type, valid file

**Unsupported File**:
A file with an extension outside Supported Format; shown as a placeholder card with filename and “Unsupported” badge, skippable via Next/Prev but never moved or hashed.
_Avoid_: Invalid file, skipped file

**Action**:
A user-configured triage destination with a display name, folder name (subfolder of Source Folder), and shortcut; moving Current File to it appends its sha256 to that Action’s log.
_Avoid_: Category, label

**Duplicate**:
A file whose sha256 already exists in any Action log; routed to the `duplicate/` subfolder instead of the chosen Action.
_Avoid_: Copy, clone

**Preview**:
The top split-view area that renders Current File via `gtk::Picture`/`Paintable` (Texture for stills/animated, GStreamer paintable for video).
_Avoid_: Viewer, canvas

**Action Bar**:
The bottom split-view area with Action buttons, Prev/Next, index counter, and Undo.
_Avoid_: Toolbar, bottom panel

**Snapshot**:
The point-in-time capture semantics — Queue does not reflect external files added/removed mid-session until relaunch.
_Avoid_: Live view, watched folder
