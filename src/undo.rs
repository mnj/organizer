use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::dedup::is_valid_hash;
use crate::mover::{move_to_action, MoverError};

/// Undo entry stores enough to reverse one move.
/// In-memory only, LIFO, capped at 50 (ADR 0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    pub hash: String,        // lower-case hex sha256
    pub src_name: String,    // e.g. "foo.jpg"
    pub dest_path: PathBuf,  // absolute sibling destination that file was moved to
    pub was_duplicate: bool, // true if routed to ../duplicate/
    pub folder_name: String, // Action folder_name for the log `SourceFolder/<folder>.txt` (empty if was_duplicate)
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
        Self {
            hash: hash.to_ascii_lowercase(),
            src_name: src_name.to_string(),
            dest_path,
            was_duplicate,
            folder_name: folder_name.to_string(),
            display_name: display_name.to_string(),
        }
    }
}

/// Push with LIFO cap 50 (ADR: up to 50 entries or queue length).
/// We cap at 50 unconditionally; queue length is at most snapshot len which is >= stack len.
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

/// Remove last occurrence of `hash` (case-insensitive) from `SourceFolder/<folder_name>.txt`.
/// Returns true if a line was removed. Uses flock exclusive + fsync + fsync parent dir.
/// If file does not exist, returns Ok(false).
pub fn remove_last_hash_occurrence(
    source_folder: &Path,
    folder_name: &str,
    hash: &str,
) -> std::io::Result<bool> {
    if !is_valid_hash(hash) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid hash: {}", hash),
        ));
    }
    let lower = hash.to_ascii_lowercase();
    let log_path = source_folder.join(format!("{}.txt", folder_name));
    if !log_path.exists() {
        return Ok(false);
    }
    // Read all lines with shared lock
    let file = OpenOptions::new().read(true).open(&log_path)?;
    #[cfg(unix)]
    {
        let _ = fs2::FileExt::lock_shared(&file);
    }
    let mut lines: Vec<String> = Vec::new();
    {
        let reader = BufReader::new(&file);
        for line in reader.lines().flatten() {
            lines.push(line);
        }
    }
    #[cfg(unix)]
    {
        let _ = fs2::FileExt::unlock(&file);
    }
    // Find last index where trimmed lower == lower
    let mut last_idx: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().to_ascii_lowercase() == lower {
            last_idx = Some(i);
        }
    }
    let idx = match last_idx {
        Some(i) => i,
        None => return Ok(false),
    };
    lines.remove(idx);
    // Rewrite file with exclusive lock, truncate, fsync
    let mut out = OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(&log_path)?;
    #[cfg(unix)]
    {
        use fs2::FileExt;
        out.lock_exclusive()?;
        for line in &lines {
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        out.sync_all()?;
        out.unlock()?;
    }
    #[cfg(not(unix))]
    {
        for line in &lines {
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        out.sync_all()?;
    }
    if let Ok(dir_file) = OpenOptions::new().read(true).open(source_folder) {
        let _ = dir_file.sync_all();
    }
    Ok(true)
}

/// Perform undo: move file back from dest_path to source_folder (with _undo suffix on clash),
/// and if not was_duplicate, remove last hash occurrence from `folder_name.txt` and
/// remove from union HashSet (only if hash no longer present in any txt after removal).
/// Returns the restored path on success.
pub fn undo_move(
    source_folder: &Path,
    entry: &UndoEntry,
    union_set: &mut HashSet<String>,
) -> std::io::Result<PathBuf> {
    let restored = undo_rename(source_folder, &entry.dest_path, &entry.src_name)?;
    if !entry.was_duplicate && !entry.folder_name.is_empty() {
        let _removed = remove_last_hash_occurrence(source_folder, &entry.folder_name, &entry.hash)?;
        // Update union: remove only if hash no longer appears in any remaining txt
        // Scan all *.txt inside source_folder to see if lower still present
        let lower = entry.hash.to_ascii_lowercase();
        let still_present = {
            let mut found = false;
            if let Ok(dir) = std::fs::read_dir(source_folder) {
                for e in dir.flatten() {
                    let p = e.path();
                    if !p.is_file() {
                        continue;
                    }
                    if !matches!(p.extension().and_then(|x| x.to_str()), Some(ext) if ext.eq_ignore_ascii_case("txt")) {
                        continue;
                    }
                    if let Ok(f) = OpenOptions::new().read(true).open(&p) {
                        #[cfg(unix)]
                        {
                            let _ = fs2::FileExt::lock_shared(&f);
                        }
                        let reader = BufReader::new(&f);
                        for line in reader.lines().flatten() {
                            if line.trim().to_ascii_lowercase() == lower {
                                found = true;
                                break;
                            }
                        }
                        #[cfg(unix)]
                        {
                            let _ = fs2::FileExt::unlock(&f);
                        }
                        if found {
                            break;
                        }
                    }
                }
            }
            found
        };
        if !still_present {
            union_set.remove(&lower);
        }
    }
    Ok(restored)
}

/// Redo is same as original move via `move_to_action` (ADR: same move+log path).
/// `current_file` is the restored file path in SourceFolder (may be _undo suffixed).
pub fn redo_move(
    source_folder: &Path,
    current_file: &Path,
    folder_name: &str,
    hash: &str,
    union_set: &mut HashSet<String>,
) -> Result<PathBuf, MoverError> {
    move_to_action(source_folder, current_file, folder_name, hash, union_set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::compute_sha256;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;



    #[test]
    fn undo_nonduplicate_moves_back_removes_log_and_union() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let foo = source.join("foo.jpg");
        fs::write(&foo, b"hello").unwrap();
        let hash = compute_sha256(&foo).unwrap();
        let mut union = HashSet::new();
        let dest = move_to_action(&source, &foo, "keep", &hash, &mut union).unwrap();
        assert!(dest.exists());
        assert_eq!(union.len(), 1);
        assert!(source.join("keep.txt").exists());
        let entry = UndoEntry::new(&hash, "foo.jpg", dest.clone(), false, "keep", "Keep");
        let restored = undo_move(&source, &entry, &mut union).unwrap();
        assert_eq!(restored, source.join("foo.jpg"));
        assert!(restored.exists());
        assert!(!dest.exists());
        // log should be empty now (removed last occurrence)
        let log = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert!(log.trim().is_empty(), "log should be empty after undo, got {:?}", log);
        assert!(!union.contains(&hash.to_ascii_lowercase()));
        // toast message would be "Undid Keep: foo.jpg → Source" - not tested here
    }

    #[test]
    fn undo_duplicate_skip_log_and_union() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        // first file to create union entry
        let a = source.join("a.jpg");
        fs::write(&a, b"same").unwrap();
        let ha = compute_sha256(&a).unwrap();
        let mut union = HashSet::new();
        let _da = move_to_action(&source, &a, "keep", &ha, &mut union).unwrap();
        assert_eq!(union.len(), 1);
        let log_before = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(log_before.lines().count(), 1);
        // second file same hash -> duplicate
        let b = source.join("b.jpg");
        fs::write(&b, b"same").unwrap();
        let hb = compute_sha256(&b).unwrap();
        assert_eq!(ha, hb);
        let db = move_to_action(&source, &b, "keep", &hb, &mut union).unwrap();
        assert_eq!(db, base.join("duplicate").join("b.jpg"));
        assert_eq!(union.len(), 1);
        let log_mid = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(log_mid.lines().count(), 1);
        let entry = UndoEntry::new(&hb, "b.jpg", db.clone(), true, "keep", "Keep");
        let restored = undo_move(&source, &entry, &mut union).unwrap();
        assert_eq!(restored, source.join("b.jpg"));
        assert!(restored.exists());
        // union unchanged, log unchanged
        assert_eq!(union.len(), 1);
        let log_after = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(log_after.lines().count(), 1);
        assert!(!base.join("duplicate").join("b.jpg").exists());
        // no duplicate.txt
        assert!(!source.join("duplicate.txt").exists());
    }

    #[test]
    fn undo_suffix_on_clash_never_overwrites() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let a = source.join("a.jpg");
        fs::write(&a, b"content a").unwrap();
        let ha = compute_sha256(&a).unwrap();
        let mut union = HashSet::new();
        let da = move_to_action(&source, &a, "keep", &ha, &mut union).unwrap();
        // create a new file with same name in source to cause clash on undo
        let clash = source.join("a.jpg");
        fs::write(&clash, b"clash").unwrap();
        let entry = UndoEntry::new(&ha, "a.jpg", da.clone(), false, "keep", "Keep");
        let restored = undo_move(&source, &entry, &mut union).unwrap();
        // should be a_undo_1.jpg
        assert_eq!(restored, source.join("a_undo_1.jpg"));
        assert!(restored.exists());
        assert!(source.join("a.jpg").exists()); // clash still there
        // second undo with same name should create _undo_2
        // need another entry pointing to same dest? Simulate another file
        let b = source.join("b.jpg");
        fs::write(&b, b"content b").unwrap();
        let hb = compute_sha256(&b).unwrap();
        let _db = move_to_action(&source, &b, "keep", &hb, &mut union).unwrap();
        // create clash again: a_undo_1 exists, a.jpg exists => next undo for a.jpg would be _undo_1 but already exists, so _undo_2
        // Let's test suffix directly by creating a_undo_1 already, then undo a.jpg with clash -> _undo_2
        // Actually we need to undo a file named a.jpg again but source already has a.jpg and a_undo_1.jpg
        // Create a new dest file named a.jpg in keep
        let a2 = source.join("a2.jpg");
        fs::write(&a2, b"another a").unwrap();
        let ha2 = compute_sha256(&a2).unwrap();
        // Move a2 to keep as a.jpg? Need to control file name: move will preserve file name a2.jpg, not a.jpg
        // Instead test suffix logic directly: create a file a.jpg in source, try undo that would restore a.jpg but clash
        // We already have a_undo_1, so next suffix should be _undo_2
        // Create a new dest: use a different keep file but restore as "a.jpg" again
        let fake_dest = base.join("keep").join("a_fake.jpg");
        fs::write(&fake_dest, b"fake").unwrap();
        let entry2 = UndoEntry::new(&ha2, "a.jpg", fake_dest.clone(), false, "keep", "Keep");
        let restored2 = undo_move(&source, &entry2, &mut union).unwrap();
        assert_eq!(restored2, source.join("a_undo_2.jpg"));
    }

    #[test]
    fn undo_remove_last_occurrence_only() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        // Create keep.txt with duplicate lines and corrupted lines
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let log_path = source.join("keep.txt");
        // write 3 lines: hash, other, hash (second occurrence)
        fs::write(&log_path, format!("{}\n{}\n{}\n", hash, other, hash)).unwrap();
        let mut union = HashSet::new();
        union.insert(hash.to_string());
        union.insert(other.to_string());
        // remove last occurrence of hash -> should keep first hash and other
        let removed = remove_last_hash_occurrence(&source, "keep", hash).unwrap();
        assert!(removed);
        let content = fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines, vec![hash, other]);
        // union still contains hash because it still appears once
        // But undo_move would check still_present and not remove. Here we test low-level still removes line.
        // Now remove again -> should remove remaining hash
        let removed2 = remove_last_hash_occurrence(&source, "keep", hash).unwrap();
        assert!(removed2);
        let content2 = fs::read_to_string(&log_path).unwrap();
        let lines2: Vec<&str> = content2.lines().collect();
        assert_eq!(lines2, vec![other]);
        // remove when not found returns false
        let removed3 = remove_last_hash_occurrence(&source, "keep", hash).unwrap();
        assert!(!removed3);
    }

    #[test]
    fn push_undo_caps_at_50() {
        let mut stack = Vec::new();
        for i in 0..55 {
            let entry = UndoEntry::new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &format!("f{}.jpg", i),
                PathBuf::from(format!("/tmp/dest/f{}.jpg", i)),
                false,
                "keep",
                "Keep",
            );
            push_undo(&mut stack, entry);
        }
        assert_eq!(stack.len(), 50);
        // oldest 5 should be evicted, so first is f5
        assert_eq!(stack[0].src_name, "f5.jpg");
        assert_eq!(stack[49].src_name, "f54.jpg");
    }

    #[test]
    fn redo_reapplies_move_and_log() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let foo = source.join("foo.jpg");
        fs::write(&foo, b"hello redo").unwrap();
        let hash = compute_sha256(&foo).unwrap();
        let mut union = HashSet::new();
        let dest = move_to_action(&source, &foo, "keep", &hash, &mut union).unwrap();
        assert!(dest.exists());
        let entry = UndoEntry::new(&hash, "foo.jpg", dest.clone(), false, "keep", "Keep");
        // undo
        let restored = undo_move(&source, &entry, &mut union).unwrap();
        assert!(restored.exists());
        assert_eq!(union.len(), 0);
        // redo: move restored file back
        let dest2 = redo_move(&source, &restored, "keep", &hash, &mut union).unwrap();
        assert!(dest2.exists());
        assert!(!restored.exists());
        assert_eq!(union.len(), 1);
        let log = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert!(log.contains(&hash.to_ascii_lowercase()));
    }

    #[test]
    fn undo_fails_if_dest_missing() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let entry = UndoEntry::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "missing.jpg",
            source.join("../keep/missing.jpg"),
            false,
            "keep",
            "Keep",
        );
        let mut union = HashSet::new();
        let res = undo_move(&source, &entry, &mut union);
        assert!(res.is_err());
    }

    #[test]
    fn headless_multistep_undo_redo_lifo() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let mut union = HashSet::new();
        let mut undo_stack: Vec<UndoEntry> = Vec::new();
        let mut redo_stack: Vec<UndoEntry> = Vec::new();

        // move a.jpg -> keep
        let a = source.join("a.jpg");
        fs::write(&a, b"a").unwrap();
        let ha = compute_sha256(&a).unwrap();
        let da = move_to_action(&source, &a, "keep", &ha, &mut union).unwrap();
        push_undo(&mut undo_stack, UndoEntry::new(&ha, "a.jpg", da.clone(), false, "keep", "Keep"));
        // move b.jpg -> maybe
        let b = source.join("b.jpg");
        fs::write(&b, b"b").unwrap();
        let hb = compute_sha256(&b).unwrap();
        let db = move_to_action(&source, &b, "maybe", &hb, &mut union).unwrap();
        push_undo(&mut undo_stack, UndoEntry::new(&hb, "b.jpg", db.clone(), false, "maybe", "Maybe"));
        assert_eq!(undo_stack.len(), 2);

        // undo last (b.jpg)
        let entry_b = undo_stack.pop().unwrap();
        let restored_b = undo_move(&source, &entry_b, &mut union).unwrap();
        redo_stack.push(entry_b.clone());
        assert!(restored_b.exists());
        assert!(!db.exists());
        assert!(!union.contains(&hb.to_ascii_lowercase()));
        assert!(union.contains(&ha.to_ascii_lowercase()));

        // undo next (a.jpg)
        let entry_a = undo_stack.pop().unwrap();
        let restored_a = undo_move(&source, &entry_a, &mut union).unwrap();
        redo_stack.push(entry_a);
        assert!(restored_a.exists());
        assert!(union.is_empty());

        // redo a.jpg
        let redo_a = redo_stack.pop().unwrap();
        let restored_a_path = source.join(&redo_a.src_name);
        // find actual restored file (may be suffix but here no clash)
        let dest_a2 = redo_move(&source, &restored_a_path, &redo_a.folder_name, &redo_a.hash, &mut union).unwrap();
        push_undo(&mut undo_stack, UndoEntry::new(&redo_a.hash, &redo_a.src_name, dest_a2.clone(), false, &redo_a.folder_name, &redo_a.display_name));
        assert!(dest_a2.exists());
        assert_eq!(union.len(), 1);

        // redo b.jpg
        let redo_b = redo_stack.pop().unwrap();
        let restored_b_path = source.join(&redo_b.src_name);
        let dest_b2 = redo_move(&source, &restored_b_path, &redo_b.folder_name, &redo_b.hash, &mut union).unwrap();
        push_undo(&mut undo_stack, UndoEntry::new(&redo_b.hash, &redo_b.src_name, dest_b2.clone(), false, &redo_b.folder_name, &redo_b.display_name));
        assert!(dest_b2.exists());
        assert_eq!(union.len(), 2);
        assert_eq!(undo_stack.len(), 2);
        assert!(redo_stack.is_empty());
    }
}
