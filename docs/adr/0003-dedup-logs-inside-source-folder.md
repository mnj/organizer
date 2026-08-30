# Dedup logs inside Source Folder, duplicate sibling, pre-hash next

`sha256` is computed eagerly on preview load in a background `spawn_blocking` pool, cached per path for the session, with pre-hash of the next queue entry while the user decides; if the user presses before ready we await with a spinner. This keeps action latency to ~ms for small images.

Each Action’s dedup log is `SourceFolder/<folder_name>.txt` (one lower-case hex sha256 per line, no filename, `\n`, `O_APPEND` + `fs2` exclusive lock + `fsync`, case-normalized) **inside the Source Folder itself**, alongside `organizer.toml`. We chose inside over siblings because `.toml`/`.txt` are not in Supported Format and are silently skipped by the Queue scanner (never shown as Unsupported placeholders), so they don’t pollute triage; they travel with the folder and are trivial to find/backup. Rejected: siblings `../<folder>.txt` (extra parent-dir bookkeeping) and XDG.

Duplicate check is the **union** of all `<folder>.txt` logs (loaded into a `HashSet` at launch, incrementally updated). If the current hash exists in any log, we **auto-move to sibling `../duplicate/`** (fixed name, lazy-created) with toast “Duplicate — already in ‘keep’ — moved to duplicate/”, filename collision → suffix `_1`, `_2`, and do not append to a `duplicate.txt`. No confirmation dialog.

Considered: checking only target log, or maintaining a `duplicate.txt`. Union matches prompt “not in any of the actions_name.txt files” and survives re-runs and manual edits.
