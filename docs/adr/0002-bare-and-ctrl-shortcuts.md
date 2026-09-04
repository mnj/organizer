# Both 1-9 and Ctrl+1-9 trigger the same Action

Actions are bound to both bare `1`–`9` and `Ctrl+1`–`9` simultaneously for the same slot. Single-key `1-9` is fastest for triage throughput, while `Ctrl+` mirrors muscle memory and still works when a settings text field has focus (bare digits type there). `Ctrl+Z` remains undo, avoiding clash. Action folders are subfolders of the Source Folder, created lazily on first move, with slug-sanitized `folder_name` (`a-z0-9_-`) and live-apply settings.
