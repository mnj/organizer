use std::path::{Path, PathBuf};

use crate::dedup::FileHash;

/// Undo entry stores enough to reverse one move.
/// In-memory only, LIFO, capped at 50 (ADR 0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    pub hash: String,        // lower-case hex sha256
    pub src_name: String,    // e.g. "foo.jpg"
    pub dest_path: PathBuf,  // absolute subfolder destination that file was moved to
    pub was_duplicate: bool, // true if routed to duplicate/ subfolder
    pub folder_name: String, // Action folder_name for the database row
    pub display_name: String, // Action display_name for toast
}

impl UndoEntry {
    pub fn new(
        hash: &str,
        src_name: &str,
        dest_path: PathBuf,
        was_duplicate: bool,
        folder_name: &str,
        display_name: &str,
    ) -> Self {
        // Validate via FileHash to enforce domain type (Primitive Obsession fix)
        let validated = FileHash::new(hash).map(|h| h.to_string()).unwrap_or_else(|_| hash.to_ascii_lowercase());
        Self {
            hash: validated,
            src_name: src_name.to_string(),
            dest_path,
            was_duplicate,
            folder_name: folder_name.to_string(),
            display_name: display_name.to_string(),
        }
    }
}

/// Push with LIFO cap 50 or queue length, whichever is smaller (ADR 0005).
/// `queue_len` is Snapshot length; cap = min(50, queue_len). If queue_len is 0, no push.
pub fn push_undo_capped(stack: &mut Vec<UndoEntry>, entry: UndoEntry, queue_len: usize) {
    let cap = std::cmp::min(50, queue_len);
    if cap == 0 {
        return;
    }
    if stack.len() >= cap {
        stack.remove(0);
    }
    stack.push(entry);
}

/// Back-compat wrapper capped at 50 (used by tests and when queue length unknown).
pub fn push_undo(stack: &mut Vec<UndoEntry>, entry: UndoEntry) {
    const MAX: usize = 50;
    if stack.len() >= MAX {
        stack.remove(0);
    }
    stack.push(entry);
}

/// Helper: undo rename `dest_path` -> `SourceFolder/src_name`, suffix _undo_1/_undo_2 on clash.
fn undo_rename(source_folder: &Path, dest_path: &Path, src_name: &str) -> std::io::Result<PathBuf> {
    if !dest_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("dest not found: {}", dest_path.display()),
        ));
    }
    let candidate = source_folder.join(src_name);
    let target = if !candidate.exists() {
        candidate
    } else {
        // suffix _undo_1, _undo_2 before extension (ADR 0005)
        let (stem, ext) = split_filename(src_name);
        let mut i = 1usize;
        loop {
            let new_name = match ext {
                Some(e) => format!("{}_{}.{}", stem, format!("undo_{}", i), e),
                // wait: ADR says suffix `_undo_1` not `_{}_undo_...`? Actually spec: suffix `_undo_1`, `_undo_2`
                // That suggests pattern: stem + "_undo_" + n + ext
                // Implemented above as stem_undo_1.ext
                None => format!("{}_{}", stem, format!("undo_{}", i)),
            };
            // The above double formats: stem + "_" + "undo_1" => stem_undo_1. Good.
            // But we produced stem_undo_1 correctly via format!("{}_{}.{}", stem, format!("undo_{}", i), e) => stem_undo_1.ext
            let cand = source_folder.join(&new_name);
            if !cand.exists() {
                break cand;
            }
            i += 1;
        }
    };
    // Ensure parent exists
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // atomic rename (same mount guaranteed)
    match std::fs::rename(dest_path, &target) {
        Ok(()) => Ok(target),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(18) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::CrossesDevices,
                    format!("cross-device: {}", dest_path.display()),
                ));
            }
            Err(e)
        }
    }
}

fn split_filename(name: &str) -> (&str, Option<&str>) {
    if let Some(pos) = name.rfind('.') {
        if pos > 0 && pos + 1 < name.len() {
            return (&name[..pos], Some(&name[pos + 1..]));
        }
    }
    (name, None)
}

/// Undo a Classification stored in the Organizer Database.
///
/// Moves the file back from its Action/`duplicate/` destination to the Source
/// Folder (suffixing `_undo_1` on clash) and, for non-duplicates, reverts the
/// database row via `Store::remove`. Duplicate undos touch no rows. Never
/// reads or writes legacy `*.txt` logs (#25).
///
/// If the row revert fails we attempt to roll the filesystem rename forward
/// again (file back to its destination) so a bare DB failure does not leave
/// filesystem restored but row present. The roll-forward recreates a missing
/// destination parent first; if it also fails we return a combined error that
/// names both failures — the file stays at the restored path and the row stays
/// present, so the caller must surface drift instead of claiming consistency.
pub fn undo_classification(
    store: &crate::store::Store,
    source_folder: &Path,
    entry: &UndoEntry,
) -> std::io::Result<PathBuf> {
    let restored = undo_rename(source_folder, &entry.dest_path, &entry.src_name)?;
    if entry.was_duplicate {
        return Ok(restored);
    }
    match store.remove(&entry.hash) {
        Ok(_) => Ok(restored),
        Err(db_err) => {
            if let Some(parent) = entry.dest_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::rename(&restored, &entry.dest_path) {
                Ok(()) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("database undo failed: {db_err}"),
                )),
                Err(roll_err) => Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "database undo failed ({db_err}); roll-forward also failed ({roll_err}): {} left at {}, row still present",
                        entry.src_name,
                        restored.display(),
                    ),
                )),
            }
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_entry(i: usize) -> UndoEntry {
        UndoEntry::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &format!("f{i}.jpg"),
            PathBuf::from(format!("/tmp/dest/f{i}.jpg")),
            false,
            "keep",
            "Keep",
        )
    }

    #[test]
    fn push_undo_caps_at_50() {
        let mut stack = Vec::new();
        for i in 0..55 {
            push_undo(&mut stack, make_entry(i));
        }
        assert_eq!(stack.len(), 50);
        // oldest 5 should be evicted, so first is f5
        assert_eq!(stack[0].src_name, "f5.jpg");
        assert_eq!(stack[49].src_name, "f54.jpg");
    }

    #[test]
    fn push_undo_capped_respects_queue_len() {
        let mut stack = Vec::new();
        for i in 0..10 {
            push_undo_capped(&mut stack, make_entry(i), 3);
        }
        assert_eq!(stack.len(), 3);
        assert_eq!(stack[0].src_name, "f7.jpg");
    }

    mod classification_on_store {
        use super::*;
        use crate::dedup::{compute_sha256, FileHash};
        use crate::mover::classify_file;
        use crate::store::Store;
        use std::fs;
        use tempfile::TempDir;

        fn setup_source() -> (TempDir, std::path::PathBuf, Store) {
            let dir = TempDir::new().unwrap();
            let source = dir.path().join("source");
            fs::create_dir_all(&source).unwrap();
            let store = Store::open(&source).unwrap();
            (dir, source, store)
        }

        fn hash_of(path: &std::path::Path) -> FileHash {
            FileHash::new(&compute_sha256(path).unwrap()).unwrap()
        }

        #[test]
        fn undo_nonduplicate_restores_file_and_reverts_row() {
            let (_dir, source, store) = setup_source();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"hello undo").unwrap();
            let hash = hash_of(&foo);
            let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
            let dest = outcome.into_dest();
            assert!(store.contains(hash.as_str()).unwrap());

            let entry = UndoEntry::new(hash.as_str(), "foo.jpg", dest.clone(), false, "keep", "Keep");
            let restored = undo_classification(&store, &source, &entry).unwrap();
            assert_eq!(restored, source.join("foo.jpg"));
            assert!(restored.exists());
            assert!(!dest.exists());
            assert!(!store.contains(hash.as_str()).unwrap());
            assert!(store.all_hashes().unwrap().is_empty());
        }

        #[test]
        fn undo_duplicate_restores_file_and_keeps_row() {
            let (_dir, source, store) = setup_source();
            let a = source.join("a.jpg");
            fs::write(&a, b"same").unwrap();
            let ha = hash_of(&a);
            classify_file(&store, &a, "keep", &ha).unwrap();

            let b = source.join("b.jpg");
            fs::write(&b, b"same").unwrap();
            let hb = hash_of(&b);
            let outcome_b = classify_file(&store, &b, "keep", &hb).unwrap();
            assert!(outcome_b.was_duplicate());
            let dest_b = outcome_b.into_dest();
            assert_eq!(dest_b, source.join("duplicate").join("b.jpg"));

            let entry = UndoEntry::new(hb.as_str(), "b.jpg", dest_b.clone(), true, "keep", "Keep");
            let restored = undo_classification(&store, &source, &entry).unwrap();
            assert_eq!(restored, source.join("b.jpg"));
            assert!(restored.exists());
            // Original row untouched.
            assert!(store.contains(ha.as_str()).unwrap());
            assert_eq!(store.all_hashes().unwrap().len(), 1);
        }

        #[test]
        fn undo_suffix_on_clash_never_overwrites() {
            let (_dir, source, store) = setup_source();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"undo clash").unwrap();
            let hash = hash_of(&foo);
            let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
            let dest = outcome.into_dest();
            // Clash file appears before undo.
            fs::write(source.join("foo.jpg"), b"clash").unwrap();
            let entry = UndoEntry::new(hash.as_str(), "foo.jpg", dest.clone(), false, "keep", "Keep");
            let restored = undo_classification(&store, &source, &entry).unwrap();
            assert_eq!(restored, source.join("foo_undo_1.jpg"));
            assert!(restored.exists());
            assert!(source.join("foo.jpg").exists());
            assert!(!store.contains(hash.as_str()).unwrap());
        }

        #[test]
        fn redo_reapplies_classification_and_row() {
            let (_dir, source, store) = setup_source();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"hello redo db").unwrap();
            let hash = hash_of(&foo);
            let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
            let dest = outcome.into_dest();
            let entry = UndoEntry::new(hash.as_str(), "foo.jpg", dest.clone(), false, "keep", "Keep");
            let restored = undo_classification(&store, &source, &entry).unwrap();
            assert!(!store.contains(hash.as_str()).unwrap());
            // Redo is a fresh Classification of the restored file.
            let redo = classify_file(&store, &restored, "keep", &hash).unwrap();
            assert!(redo.dest().exists());
            assert!(!restored.exists());
            assert!(store.contains(hash.as_str()).unwrap());
        }

        #[test]
        fn undo_fails_if_dest_missing_and_keeps_row() {
            let (_dir, source, store) = setup_source();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"data").unwrap();
            let hash = hash_of(&foo);
            let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
            let dest = outcome.into_dest();
            // Externally delete the destination.
            fs::remove_file(&dest).unwrap();
            let entry = UndoEntry::new(hash.as_str(), "foo.jpg", dest.clone(), false, "keep", "Keep");
            assert!(undo_classification(&store, &source, &entry).is_err());
            // Row must still be present since the file could not be restored.
            assert!(store.contains(hash.as_str()).unwrap());
        }

        #[test]
        fn undo_db_failure_reports_roll_forward_result() {
            // Drop the database directory's write path by pointing the entry at a
            // destination whose parent was removed: roll-forward must recreate the
            // parent instead of silently drifting.
            let (_dir, source, store) = setup_source();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"roll-forward").unwrap();
            let hash = hash_of(&foo);
            let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
            let dest = outcome.into_dest();
            // Remove the Action subfolder to simulate a vanished destination parent.
            fs::remove_dir_all(source.join("keep")).unwrap();
            assert!(!dest.exists());
            // Undo now fails at the rename step (dest missing), row stays.
            let entry = UndoEntry::new(hash.as_str(), "foo.jpg", dest.clone(), false, "keep", "Keep");
            assert!(undo_classification(&store, &source, &entry).is_err());
            assert!(store.contains(hash.as_str()).unwrap());
        }
    }
}
