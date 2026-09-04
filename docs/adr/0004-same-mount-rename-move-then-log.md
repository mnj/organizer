# Same-mount rename, move-then-log with rollback, suffix on clash, busy-ignore

Files are guaranteed same mountpoint on Linux ext4/btrfs only, so moves are `std::fs::rename` only (atomic, no EXDEV copy+remove fallback). We intentionally do not support cross-filesystem or non-Linux FS for v1.

Ordering is `check union → move → append log`. After a successful `rename` to `<folder>/` or `duplicate/` inside the Source Folder, we `flock` + `O_APPEND` + `fsync` the hash to `SourceFolder/<folder>.txt` and update the in-memory HashSet. If log fsync fails, we **rollback** by renaming the file back to Source Folder, toast “Disk full / I/O error — move reverted”, and do not update the HashSet. On crash after move but before log, the orphan (sorted but not logged) is tolerated; an optional low-priority startup heal can re-hash `<folder>/` contents and append missing hashes, but is not required for v1.

Name collisions in the destination are resolved by suffixing `_1`, `_2` before rename, never overwriting. While a move+log future is in flight, further `1-9`/`Ctrl+1-9` are ignored (spinner on Preview, buttons disabled) — single in-flight invariant, no queue. On any failure (move or log) we stay on Current File with a toast and `tracing::warn`, never auto-advance; corrupted log lines at startup are skipped with a warning and counted, not fatal.

Considered: log-then-move (phantom hash on crash), always copy+remove (slower, not needed for same-mount), overwriting, queuing keypresses. This keeps the critical path atomic where the filesystem allows it and makes failure branches observable.
