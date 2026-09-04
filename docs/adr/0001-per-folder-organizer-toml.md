# Per-folder organizer.toml for Action config

Action config is persisted as `organizer.toml` inside the Source Folder itself (per-folder, TOML, `config_version = 1`), not in XDG `~/.config`. We picked per-folder because each triage job has its own action set and should travel with the folder (visible, copyable, `.gitignore`-able), and because the app’s core invariant is “one Source Folder → one flat Queue → action subfolders inside the Source Folder.” XDG would force a single global action set that mismatches per-job workflows and complicates AppImage/raw portability where the Source Folder is the only stable anchor.

Considered: XDG `~/.config/organizer/config.toml` (global, shareable across folders, but hides per-job intent and drifts) and per-folder hidden `.organizer.toml` (extra `ls -a` friction). `organizer.toml` is explicit, matches binary name, and works identically for raw binary and AppImage without extra config discovery.

Consequences: Opening a new folder auto-creates `organizer.toml` with default 3 actions; switching folders loads that folder’s own config; no migration needed v1, but future global defaults will need a fallback read.
