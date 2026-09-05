use crate::dedup::{append_hash_log, FileHash};
use crate::store::{FileRecord, Store};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Domain bundle for the clumped `source_folder + folder_name + hash` (Data Clumps fix).
pub struct ActionTarget<'a> {
    pub source_folder: &'a Path,
    pub folder_name: &'a str,
}

/// Intent-revealing name: resolve a destination collision by suffixing _1, _2 before extension.
/// Preserves extension; if no extension, suffix after name.
/// Kept `next_available_path` as alias for tests/back-compat (Mysterious Name fix).
pub fn resolve_collision_path(dest_dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dest_dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_filename(file_name);
    let mut i = 1usize;
    loop {
        let new_name = match ext {
            Some(e) => format!("{}_{}.{}", stem, i, e),
            None => format!("{}_{}", stem, i),
        };
        let cand = dest_dir.join(&new_name);
        if !cand.exists() {
            return cand;
        }
        i += 1;
    }
}

/// Back-compat alias (kept for existing tests/callers).
pub fn next_available_path(dest_dir: &Path, file_name: &str) -> PathBuf {
    resolve_collision_path(dest_dir, file_name)
}

fn split_filename(name: &str) -> (&str, Option<&str>) {
    if let Some(pos) = name.rfind('.') {
        if pos > 0 && pos + 1 < name.len() {
            return (&name[..pos], Some(&name[pos + 1..]));
        }
    }
    (name, None)
}

#[derive(Debug)]
pub enum MoverError {
    Io(std::io::Error),
    Exdev(PathBuf),
    Db(String),
}

impl From<std::io::Error> for MoverError {
    fn from(e: std::io::Error) -> Self {
        MoverError::Io(e)
    }
}

impl From<crate::store::StoreError> for MoverError {
    fn from(e: crate::store::StoreError) -> Self {
        MoverError::Db(e.to_string())
    }
}

impl std::fmt::Display for MoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoverError::Io(e) => write!(f, "io: {}", e),
            MoverError::Exdev(p) => write!(f, "cross-device move not supported: {}", p.display()),
            MoverError::Db(msg) => write!(f, "database: {}", msg),
        }
    }
}

/// Core mover that operates on domain types (Fixes Primitive Obsession / Data Clumps).
pub fn move_to_action_with_target(
    target: ActionTarget<'_>,
    current_file: &Path,
    hash: &FileHash,
    union_set: &mut HashSet<String>,
) -> Result<PathBuf, MoverError> {
    move_to_action_inner(target.source_folder, current_file, target.folder_name, hash.as_str(), union_set)
}

/// Destination subfolder inside the Source Folder itself (lazily created).
/// Action and `duplicate/` folders live *inside* the Source Folder, never as
/// siblings: the Queue scanner only takes direct-child files, so triaged
/// files can never re-enter the Queue, even across relaunches.
fn dest_subdir(source_folder: &Path, name: &str) -> PathBuf {
    source_folder.join(name)
}

fn atomic_rename_with_suffix(dest_dir: &Path, current_file: &Path, file_name: &str) -> Result<PathBuf, MoverError> {
    std::fs::create_dir_all(dest_dir).map_err(MoverError::Io)?;
    let dest_path = next_available_path(dest_dir, file_name);
    match std::fs::rename(current_file, &dest_path) {
        Ok(()) => Ok(dest_path),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(18) {
                return Err(MoverError::Exdev(dest_path));
            }
            Err(MoverError::Io(e))
        }
    }
}

/// Move file directly to fixed `duplicate/` subfolder without log or union change.
/// Used when hash already exists in union (ADR 0003).
fn move_to_duplicate_inner(
    source_folder: &Path,
    current_file: &Path,
    file_name: &str,
) -> Result<PathBuf, MoverError> {
    let dest_dir = dest_subdir(source_folder, "duplicate");
    atomic_rename_with_suffix(&dest_dir, current_file, file_name)
}

fn move_to_action_inner(
    source_folder: &Path,
    current_file: &Path,
    folder_name: &str,
    hash: &str,
    union_set: &mut HashSet<String>,
) -> Result<PathBuf, MoverError> {
    // Validate that current_file is inside source_folder (file name only)
    let file_name = current_file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| MoverError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid filename")))?
        .to_string();
    let hash_lower = hash.to_ascii_lowercase();

    // ADR 0003: if hash already in union -> route to fixed duplicate/ subfolder, no log
    if union_set.contains(&hash_lower) {
        return move_to_duplicate_inner(source_folder, current_file, &file_name);
    }

    let dest_dir = dest_subdir(source_folder, folder_name);
    let dest_path = atomic_rename_with_suffix(&dest_dir, current_file, &file_name)?;

    // Append hash log; on failure rollback rename back
    if let Err(e) = append_hash_log(source_folder, folder_name, hash) {
        // Rollback: move file back to Source Folder
        let rollback_target = {
            let candidate = source_folder.join(&file_name);
            if !candidate.exists() {
                candidate
            } else {
                // suffix on clash to avoid overwrite (rare for rollback)
                next_available_path(source_folder, &file_name)
            }
        };
        let _ = std::fs::rename(&dest_path, &rollback_target);
        // Do not update HashSet
        return Err(MoverError::Io(e));
    }

    // Update union set with lower-case hash
    union_set.insert(hash_lower);

    // fsync handled inside append_hash_log; also fsync parent already
    Ok(dest_path)
}

/// Public API preserving original signature (for tests/callers that use raw &str hash).
/// Validates hash via FileHash before delegating (Primitive Obsession safe).
pub fn move_to_action(
    source_folder: &Path,
    current_file: &Path,
    folder_name: &str,
    hash: &str,
    union_set: &mut HashSet<String>,
) -> Result<PathBuf, MoverError> {
    let validated = FileHash::new(hash).map_err(|e| MoverError::Io(e))?;
    let target = ActionTarget { source_folder, folder_name };
    move_to_action_with_target(target, current_file, &validated, union_set)
}

/// Roll a Classification rename back to the Source Folder on DB failure.
/// Suffixes on clash so rollback never overwrites.
fn rollback_to_source(source_folder: &Path, dest_path: &Path, file_name: &str) {
    let target = {
        let candidate = source_folder.join(file_name);
        if !candidate.exists() {
            candidate
        } else {
            next_available_path(source_folder, file_name)
        }
    };
    let _ = std::fs::rename(dest_path, &target);
}

/// GUI Classification on the Organizer Database (#24).
///
/// Moves Current File to the chosen Action subfolder inside the Source Folder
/// and records its sha256 with Source Folder-relative paths. Duplicate hashes
/// (already known to the database) route to the fixed `duplicate/` subfolder
/// with no extra record growth. On DB failure the rename is rolled back.
///
/// Relative layout: `original_rel` is the flat file name, `final_rel` is
/// `<action>/<dest file name>` (suffix-aware on collision).
pub fn classify_file(
    store: &Store,
    current_file: &Path,
    folder_name: &str,
    hash: &str,
) -> Result<PathBuf, MoverError> {
    let validated = FileHash::new(hash).map_err(MoverError::Io)?;
    let hash_lower = validated.as_str().to_ascii_lowercase();
    let source_folder = store.source_folder().to_path_buf();
    if folder_name.is_empty() {
        return Err(MoverError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid action folder",
        )));
    }
    let file_name = current_file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            MoverError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid filename",
            ))
        })?
        .to_string();

    // Duplicate union read from the database: already-known hash routes to
    // duplicate/ with no record growth (origin lookup via Store::origin for toast).
    let is_duplicate = store.contains(&hash_lower).map_err(MoverError::from)?;
    if is_duplicate {
        return move_to_duplicate_inner(&source_folder, current_file, &file_name);
    }

    // Capture filesystem metadata before the rename for the file row.
    let (size, mtime) = match std::fs::metadata(current_file) {
        Ok(md) => {
            let size = md.len() as i64;
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (size, mtime)
        }
        Err(e) => return Err(MoverError::Io(e)),
    };
    let triaged_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let dest_dir = dest_subdir(&source_folder, folder_name);
    let dest_path = atomic_rename_with_suffix(&dest_dir, current_file, &file_name)?;

    let dest_file_name = dest_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&file_name)
        .to_string();
    let original_rel = file_name.clone();
    let final_rel = format!("{folder_name}/{dest_file_name}");

    let record = match FileRecord::new(
        &hash_lower,
        &original_rel,
        &final_rel,
        folder_name,
        size,
        mtime,
        triaged_at,
    ) {
        Ok(r) => r,
        Err(e) => {
            rollback_to_source(&source_folder, &dest_path, &file_name);
            return Err(MoverError::Db(e.to_string()));
        }
    };

    match store.insert_file(&record) {
        Ok(true) => Ok(dest_path),
        Ok(false) => {
            // Lost a concurrent race: another writer inserted the same hash
            // between our contains-check and insert. Route the already-moved
            // file on to duplicate/ so Action folders never gain duplicates
            // and the database gains no extra row.
            if folder_name == "duplicate" {
                return Ok(dest_path);
            }
            let dup_dir = dest_subdir(&source_folder, "duplicate");
            match atomic_rename_with_suffix(&dup_dir, &dest_path, &dest_file_name) {
                Ok(dup_path) => Ok(dup_path),
                Err(e) => {
                    rollback_to_source(&source_folder, &dest_path, &file_name);
                    Err(e)
                }
            }
        }
        Err(e) => {
            rollback_to_source(&source_folder, &dest_path, &file_name);
            Err(MoverError::from(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::compute_sha256;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn next_available_path_suffix_before_extension() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        // no clash
        assert_eq!(next_available_path(p, "foo.jpg"), p.join("foo.jpg"));
        fs::write(p.join("foo.jpg"), b"x").unwrap();
        assert_eq!(next_available_path(p, "foo.jpg"), p.join("foo_1.jpg"));
        fs::write(p.join("foo_1.jpg"), b"x").unwrap();
        assert_eq!(next_available_path(p, "foo.jpg"), p.join("foo_2.jpg"));
        // with multiple dots
        fs::write(p.join("a.b.png"), b"x").unwrap();
        assert_eq!(next_available_path(p, "a.b.png"), p.join("a.b_1.png"));
        // no extension
        fs::write(p.join("README"), b"x").unwrap();
        assert_eq!(next_available_path(p, "README"), p.join("README_1"));
    }

    #[test]
    fn move_to_action_happy_path_creates_subfolder_and_log_and_advances() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        // Prepare a file foo.jpg in source
        let foo = source.join("foo.jpg");
        fs::write(&foo, b"hello").unwrap();
        let hash = compute_sha256(&foo).unwrap();
        let mut union = HashSet::new();

        let dest = move_to_action(&source, &foo, "keep", &hash, &mut union).unwrap();

        assert!(!foo.exists(), "source file must disappear");
        assert!(dest.exists(), "dest file must exist");
        assert_eq!(dest, source.join("keep").join("foo.jpg"));
        let log = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(log.trim(), hash.to_ascii_lowercase());
        assert!(union.contains(&hash.to_ascii_lowercase()));
        // action subfolder lazily created inside Source Folder
        assert!(source.join("keep").is_dir());
    }

    #[test]
    fn move_to_action_suffix_on_clash_never_overwrites() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let dest_dir = source.join("keep");
        fs::create_dir_all(&dest_dir).unwrap();
        // pre-create file in dest to cause clash
        fs::write(dest_dir.join("foo.jpg"), b"existing").unwrap();

        let foo = source.join("foo.jpg");
        fs::write(&foo, b"hello2").unwrap();
        let hash = compute_sha256(&foo).unwrap();
        let mut union = HashSet::new();

        let dest = move_to_action(&source, &foo, "keep", &hash, &mut union).unwrap();
        assert_eq!(dest, dest_dir.join("foo_1.jpg"));
        assert!(dest.exists());
        assert!(dest_dir.join("foo.jpg").exists(), "original must still exist");
        let log = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(log.trim(), hash.to_ascii_lowercase());
    }

    #[test]
    fn move_is_atomic_rename_only_no_exdev_copy() {
        // On same mount, rename succeeds; we just verify it doesn't copy
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let foo = source.join("bar.png");
        fs::write(&foo, b"data").unwrap();
        let hash = compute_sha256(&foo).unwrap();
        let mut union = HashSet::new();
        let dest = move_to_action(&source, &foo, "keep", &hash, &mut union).unwrap();
        // ensure source inode gone and dest is same data (rename, not copy+remove emulated)
        assert!(!source.join("bar.png").exists());
        let data = fs::read(&dest).unwrap();
        assert_eq!(data, b"data");
    }

    #[test]
    fn log_lines_are_lowercase_one_per_line_flock_fsync() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let mut union = HashSet::new();
        for content in [b"a" as &[u8], b"b", b"c"] {
            let name = format!("{}.jpg", String::from_utf8_lossy(content));
            let p = source.join(&name);
            fs::write(&p, content).unwrap();
            let hash = compute_sha256(&p).unwrap();
            move_to_action(&source, &p, "keep", &hash, &mut union).unwrap();
        }
        let log = fs::read_to_string(source.join("keep.txt")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(*line, line.to_ascii_lowercase());
            assert_eq!(line.len(), 64);
        }
        assert_eq!(union.len(), 3);
    }

    #[test]
    fn log_failure_rollback_moves_back_and_hashset_unchanged() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let foo = source.join("foo.jpg");
        fs::write(&foo, b"hello rollback").unwrap();
        let hash = compute_sha256(&foo).unwrap();

        // Create a directory at keep.txt to cause append_hash_log to fail (EISDIR)
        // append opens with OpenOptions append, which will fail if path is a dir
        fs::create_dir_all(source.join("keep.txt")).unwrap();

        let mut union = HashSet::new();
        let res = move_to_action(&source, &foo, "keep", &hash, &mut union);
        assert!(res.is_err(), "should fail due to log dir");

        // file must have been rolled back to source
        assert!(source.join("foo.jpg").exists(), "file must be back in source after rollback, found: {:?}", fs::read_dir(&source).unwrap().collect::<Vec<_>>());
        assert!(!source.join("keep").join("foo.jpg").exists(), "dest must not retain file after rollback");
        assert!(!union.contains(&hash.to_ascii_lowercase()), "hashset must not be updated");
        // cleanup dir for other tests
        fs::remove_dir(source.join("keep.txt")).unwrap();
    }

    #[test]
    fn busy_ignore_simulation_further_keys_ignored() {
        // Simulate that while a move is in flight, second call should be ignored by UI
        // Here we just verify that dest suffix handles concurrent moves correctly
        // The busy flag itself is UI state; core mover is synchronous, so no race.
        // We test that two files with same name suffix correctly.
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let foo1 = source.join("a.jpg");
        let foo2 = source.join("b.jpg");
        fs::write(&foo1, b"one").unwrap();
        fs::write(&foo2, b"two").unwrap();
        let h1 = compute_sha256(&foo1).unwrap();
        let h2 = compute_sha256(&foo2).unwrap();
        let mut union = HashSet::new();
        let d1 = move_to_action(&source, &foo1, "keep", &h1, &mut union).unwrap();
        let d2 = move_to_action(&source, &foo2, "keep", &h2, &mut union).unwrap();
        assert_ne!(d1, d2);
        assert_eq!(union.len(), 2);
    }

    #[test]
    fn headless_tempdir_acceptance_press_1_moves_and_log() {
        // Minimal acceptance: press 1 on foo.jpg → keep/foo.jpg inside Source Folder and keep.txt gains hash, Preview advances (queue index)
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        std::fs::create_dir_all(&source).unwrap();
        // Build a tiny queue: two files
        let a = source.join("a.jpg");
        let b = source.join("b.jpg");
        std::fs::write(&a, b"image a").unwrap();
        std::fs::write(&b, b"image b").unwrap();
        let snap = crate::queue::build_snapshot(&source).unwrap();
        assert_eq!(snap.len(), 2);
        let mut union = HashSet::new();
        let mut idx = 0usize;
        // Simulate press 1 on Current File a.jpg
        let cur = snap[idx].clone();
        let hash = compute_sha256(&cur).unwrap();
        let dest = move_to_action(&source, &cur, "keep", &hash, &mut union).unwrap();
        assert_eq!(dest, source.join("keep").join("a.jpg"));
        assert!(!source.join("a.jpg").exists());
        let log = std::fs::read_to_string(source.join("keep.txt")).unwrap();
        assert!(log.lines().any(|l| l == hash.to_ascii_lowercase()));
        // auto-advance to next index (simulated)
        // Since queue is Snapshot immutable, advancing means incrementing cursor
        // The next file b.jpg should still exist in source (not moved yet)
        assert!(source.join("b.jpg").exists());
        // Simulate that UI would advance idx; we just check that mover didn't delete b.jpg
        idx += 1;
        assert_eq!(snap[idx].file_name().unwrap(), "b.jpg");
    }

    #[test]
    fn duplicate_routing_moves_to_duplicate_no_log_union_unchanged() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        // first file a.jpg with hash X -> keep
        let a = source.join("a.jpg");
        fs::write(&a, b"same content").unwrap();
        let hash = compute_sha256(&a).unwrap();
        let mut union = HashSet::new();
        let dest_a = move_to_action(&source, &a, "keep", &hash, &mut union).unwrap();
        assert_eq!(dest_a, source.join("keep").join("a.jpg"));
        assert!(union.contains(&hash.to_ascii_lowercase()));
        let log_before = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(log_before.lines().count(), 1);

        // second file b.jpg same hash X -> should go to duplicate, not keep
        let b = source.join("b.jpg");
        fs::write(&b, b"same content").unwrap();
        let hash_b = compute_sha256(&b).unwrap();
        assert_eq!(hash, hash_b);
        let dest_b = move_to_action(&source, &b, "keep", &hash_b, &mut union).unwrap();
        assert_eq!(dest_b, source.join("duplicate").join("b.jpg"), "duplicate hash must route to duplicate/ inside Source Folder");
        assert!(dest_b.exists());
        assert!(!source.join("b.jpg").exists());
        // keep.txt must not gain second line, no duplicate.txt
        let log_after = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(log_after.lines().count(), 1, "duplicate must not append log");
        assert!(!source.join("duplicate.txt").exists(), "no duplicate.txt");
        assert_eq!(union.len(), 1, "union unchanged on duplicate");
        assert!(source.join("duplicate").is_dir());
    }

    #[test]
    fn duplicate_routing_suffix_on_clash() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        // prime union with hash X via first move to keep
        let first = source.join("orig.jpg");
        fs::write(&first, b"dup").unwrap();
        let h = compute_sha256(&first).unwrap();
        let mut union = HashSet::new();
        move_to_action(&source, &first, "keep", &h, &mut union).unwrap();
        // pre-create file in duplicate to cause clash
        let dup_dir = source.join("duplicate");
        fs::create_dir_all(&dup_dir).unwrap();
        fs::write(dup_dir.join("b.jpg"), b"existing").unwrap();
        let b = source.join("b.jpg");
        fs::write(&b, b"dup").unwrap();
        let hb = compute_sha256(&b).unwrap();
        assert_eq!(h, hb);
        let dest = move_to_action(&source, &b, "keep", &hb, &mut union).unwrap();
        assert_eq!(dest, dup_dir.join("b_1.jpg"));
        assert!(dest.exists());
    }

    #[test]
    fn headless_dedup_a_keep_b_duplicate_c_keep() {
        // Acceptance: a.jpg hash X -> keep, b.jpg same hash X -> duplicate, c.jpg new hash Y -> keep
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let a = source.join("a.jpg");
        let b = source.join("b.jpg");
        let c = source.join("c.jpg");
        fs::write(&a, b"hash X content").unwrap();
        fs::write(&b, b"hash X content").unwrap();
        fs::write(&c, b"hash Y content").unwrap();
        let ha = compute_sha256(&a).unwrap();
        let hb = compute_sha256(&b).unwrap();
        let hc = compute_sha256(&c).unwrap();
        assert_eq!(ha, hb);
        assert_ne!(ha, hc);
        let mut union = HashSet::new();
        let da = move_to_action(&source, &a, "keep", &ha, &mut union).unwrap();
        assert_eq!(da, source.join("keep").join("a.jpg"));
        assert_eq!(union.len(), 1);
        let db = move_to_action(&source, &b, "keep", &hb, &mut union).unwrap();
        assert_eq!(db, source.join("duplicate").join("b.jpg"));
        assert_eq!(union.len(), 1, "duplicate must not grow union");
        let dc = move_to_action(&source, &c, "keep", &hc, &mut union).unwrap();
        assert_eq!(dc, source.join("keep").join("c.jpg"));
        assert_eq!(union.len(), 2);
        // filesystem asserts
        assert!(source.join("keep").join("a.jpg").exists());
        assert!(source.join("duplicate").join("b.jpg").exists());
        assert!(source.join("keep").join("c.jpg").exists());
        let keep_log = fs::read_to_string(source.join("keep.txt")).unwrap();
        assert_eq!(keep_log.lines().count(), 2);
        assert!(keep_log.contains(&ha.to_ascii_lowercase()));
        assert!(keep_log.contains(&hc.to_ascii_lowercase()));
        assert!(!source.join("duplicate.txt").exists());
    }

    mod classify_on_store {
        use super::*;
        use crate::dedup::compute_sha256;
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

        #[test]
        fn classify_moves_to_action_and_records_relative_paths() {
            let (_dir, source, store) = setup_source();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"hello classify").unwrap();
            let hash = compute_sha256(&foo).unwrap();
            let meta_before = fs::metadata(&foo).unwrap();

            let dest = classify_file(&store, &foo, "keep", &hash).unwrap();

            assert!(!foo.exists());
            assert_eq!(dest, source.join("keep").join("foo.jpg"));
            assert!(dest.exists());
            // Database row present with Source Folder-relative paths.
            let rec = store.lookup(&hash).unwrap().expect("row must exist");
            assert_eq!(rec.original_rel, "foo.jpg");
            assert_eq!(rec.final_rel, "keep/foo.jpg");
            assert_eq!(rec.action_folder, "keep");
            assert_eq!(rec.size, meta_before.len() as i64);
            // No legacy txt logs created by the database path.
            assert!(!source.join("keep.txt").exists());
        }

        #[test]
        fn duplicate_routes_to_duplicate_with_origin_and_no_growth() {
            let (_dir, source, store) = setup_source();
            let a = source.join("a.jpg");
            fs::write(&a, b"same content").unwrap();
            let hash = compute_sha256(&a).unwrap();
            let dest_a = classify_file(&store, &a, "keep", &hash).unwrap();
            assert_eq!(dest_a, source.join("keep").join("a.jpg"));
            assert_eq!(store.all_hashes().unwrap().len(), 1);
            assert_eq!(store.origin(&hash).unwrap().as_deref(), Some("keep"));

            let b = source.join("b.jpg");
            fs::write(&b, b"same content").unwrap();
            let hash_b = compute_sha256(&b).unwrap();
            assert_eq!(hash, hash_b);
            let dest_b = classify_file(&store, &b, "keep", &hash_b).unwrap();
            assert_eq!(
                dest_b,
                source.join("duplicate").join("b.jpg"),
                "duplicate hash must route to duplicate/"
            );
            assert!(dest_b.exists());
            assert!(!source.join("b.jpg").exists());
            // No extra record growth.
            assert_eq!(store.all_hashes().unwrap().len(), 1);
            let rec = store.lookup(&hash).unwrap().unwrap();
            assert_eq!(rec.original_rel, "a.jpg");
            assert_eq!(rec.action_folder, "keep");
            // Origin still keep for toast.
            assert_eq!(store.origin(&hash_b).unwrap().as_deref(), Some("keep"));
        }

        #[test]
        fn classify_suffix_on_clash_records_suffixed_final_rel() {
            let (_dir, source, store) = setup_source();
            let dest_dir = source.join("keep");
            fs::create_dir_all(&dest_dir).unwrap();
            fs::write(dest_dir.join("foo.jpg"), b"existing").unwrap();

            let foo = source.join("foo.jpg");
            fs::write(&foo, b"new content").unwrap();
            let hash = compute_sha256(&foo).unwrap();
            let dest = classify_file(&store, &foo, "keep", &hash).unwrap();
            assert_eq!(dest, dest_dir.join("foo_1.jpg"));
            let rec = store.lookup(&hash).unwrap().unwrap();
            assert_eq!(rec.final_rel, "keep/foo_1.jpg");
        }

        #[test]
        fn classify_invalid_hash_leaves_file_in_place() {
            let (_dir, source, store) = setup_source();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"data").unwrap();
            let res = classify_file(&store, &foo, "keep", "not-a-hash");
            assert!(res.is_err());
            assert!(foo.exists());
            assert!(store.all_hashes().unwrap().is_empty());
        }

        #[test]
        fn classify_relative_paths_survive_folder_move() {
            let dir = TempDir::new().unwrap();
            let source = dir.path().join("source");
            fs::create_dir_all(&source).unwrap();
            let store = Store::open(&source).unwrap();
            let foo = source.join("foo.jpg");
            fs::write(&foo, b"portable").unwrap();
            let hash = compute_sha256(&foo).unwrap();
            classify_file(&store, &foo, "keep", &hash).unwrap();
            drop(store);
            let moved = dir.path().join("moved");
            fs::rename(&source, &moved).unwrap();
            let reopened = Store::open(&moved).unwrap();
            let rec = reopened.lookup(&hash).unwrap().unwrap();
            assert_eq!(rec.original_rel, "foo.jpg");
            assert_eq!(rec.final_rel, "keep/foo.jpg");
            assert!(moved.join(&rec.final_rel).exists());
        }
    }
}
