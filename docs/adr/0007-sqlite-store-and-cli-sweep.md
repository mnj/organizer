# SQLite store and CLI sweep

Replace per-folder `organizer.toml` + `Source Folder/*.txt` hash logs with a single `Source Folder/organizer.db` sqlite file (WAL, `BEGIN IMMEDIATE` around hash-check → move → insert, all paths Source Folder-relative so folder + db move together), with fresh seeding of default Categories and no legacy import; GUI Classification writes to it, CLI Sweep (`organizer cleanup <TARGET> [--db ./organizer.db] [--categories …] [--action report|trash|perm-delete] [--format …] [--execute]`) checks TARGET (flat, Supported Format, Snapshot semantics) read-only against the reference db union and applies the Action, defaulting to dry-run `report`.

Considered: keeping txt as fallback (rejected: split-brain), global XDG db (rejected: breaks folder portability per ADR 0001/0003), CLI doing classification/moves (rejected: Sweep never classifies).

Consequences: txt+toml persistence becomes sqlite-only with same Category validation rules; Sweep needs `trash` crate for freedesktop trash vs `remove_file` perm-delete, `--categories` defaults to all Categories with TUI toggles, and CLI writes no rows and keeps no undo (unlike GUI ADR 0005).
