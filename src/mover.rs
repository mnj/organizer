use crate::dedup::FileHash;
use crate::store::{FileRecord, Store};
use std::path::{Path, PathBuf};

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

/// Shared user-facing message for Classification failures (Repeated Switches fix).
/// Single place mapping `MoverError` to toast text so move + redo cannot drift.
pub fn mover_error_message(e: &MoverError, file_name: &str) -> String {
    match e {
        MoverError::Exdev(_) => "Cross-device move not supported — move reverted".to_string(),
        MoverError::Db(dbe) => {
            tracing::warn!("database failure for {}: {}", file_name, dbe);
            "Database error — move reverted".to_string()
        }
        MoverError::Io(ioe) => {
            tracing::warn!("move failure for {}: {}", file_name, ioe);
            "Disk full / I/O error — move reverted".to_string()
        }
    }
}

/// Outcome of a Store-backed Classification (Feature Envy fix).
/// Callers match instead of inferring `duplicate/` from the destination path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifyOutcome {
    /// Moved to the chosen Category subfolder.
    Classified(PathBuf),
    /// Hash already known: routed to `duplicate/` with no extra row.
    Duplicate(PathBuf),
}

impl ClassifyOutcome {
    pub fn dest(&self) -> &Path {
        match self {
            ClassifyOutcome::Classified(p) | ClassifyOutcome::Duplicate(p) => p,
        }
    }
    pub fn was_duplicate(&self) -> bool {
        matches!(self, ClassifyOutcome::Duplicate(_))
    }
    pub fn into_dest(self) -> PathBuf {
        match self {
            ClassifyOutcome::Classified(p) | ClassifyOutcome::Duplicate(p) => p,
        }
    }
}

/// Destination subfolder inside the Source Folder itself (lazily created).
/// Category and `duplicate/` folders live *inside* the Source Folder, never as
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

/// Move file directly to fixed `duplicate/` subfolder with no record growth.
/// Used when the hash is already known to the Organizer Database.
fn move_to_duplicate_inner(
    source_folder: &Path,
    current_file: &Path,
    file_name: &str,
) -> Result<PathBuf, MoverError> {
    let dest_dir = dest_subdir(source_folder, "duplicate");
    atomic_rename_with_suffix(&dest_dir, current_file, file_name)
}

/// Roll a Classification rename back to the Source Folder on DB failure.
/// Suffixes on clash so rollback never overwrites. Best-effort: warns on
/// failure so a stuck file is visible instead of silently drifting.
fn rollback_to_source(source_folder: &Path, dest_path: &Path, file_name: &str) {
    let target = {
        let candidate = source_folder.join(file_name);
        if !candidate.exists() {
            candidate
        } else {
            next_available_path(source_folder, file_name)
        }
    };
    if let Err(e) = std::fs::rename(dest_path, &target) {
        tracing::warn!(
            "classification rollback failed: {} -> {}: {}",
            dest_path.display(),
            target.display(),
            e
        );
    }
}

/// GUI Classification on the Organizer Database.
///
/// Moves Current File to the chosen Category subfolder inside the Source Folder
/// and records its sha256 with Source Folder-relative paths. Duplicate hashes
/// (already known to the database) route to the fixed `duplicate/` subfolder
/// with no extra record growth. On DB failure the rename is rolled back.
/// Never reads or writes legacy `*.txt` or `organizer.toml` (#25).
///
/// Atomicity (ADR 0007): the filesystem rename cannot live inside a SQLite
/// transaction, so atomicity comes from `Store::insert_file` (`BEGIN
/// IMMEDIATE` + `INSERT OR IGNORE` idempotent claim) plus handling here:
/// a `contains` fast-path avoids moving known duplicates into Category folders,
/// and an `insert -> false` race (another writer claimed the hash between our
/// check and insert) routes the already-moved file on to `duplicate/` with no
/// extra row. `Store` retries transient BUSY internally.
///
/// Relative layout: `original_rel` is the flat file name, `final_rel` is
/// `<category>/<dest file name>` (suffix-aware on collision).
pub fn classify_file(
    store: &Store,
    current_file: &Path,
    folder_name: &str,
    hash: &FileHash,
) -> Result<ClassifyOutcome, MoverError> {
    let hash_lower = hash.as_str().to_ascii_lowercase();
    let source_folder = store.source_folder().to_path_buf();
    if folder_name.is_empty() {
        return Err(MoverError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid category folder",
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

    // Duplicate check reads only the Organizer Database; legacy `*.txt` logs
    // are ignored (#25).
    let is_duplicate = store.contains(&hash_lower).map_err(MoverError::from)?;
    if is_duplicate {
        let dup = move_to_duplicate_inner(&source_folder, current_file, &file_name)?;
        return Ok(ClassifyOutcome::Duplicate(dup));
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
        Ok(true) => Ok(ClassifyOutcome::Classified(dest_path)),
        Ok(false) => {
            // Lost a concurrent race: another writer inserted the same hash
            // between our contains-check and insert. Route the already-moved
            // file on to duplicate/ so Category folders never gain duplicates
            // and the database gains no extra row.
            if folder_name == "duplicate" {
                return Ok(ClassifyOutcome::Duplicate(dest_path));
            }
            let dup_dir = dest_subdir(&source_folder, "duplicate");
            match atomic_rename_with_suffix(&dup_dir, &dest_path, &dest_file_name) {
                Ok(dup_path) => Ok(ClassifyOutcome::Duplicate(dup_path)),
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
    use crate::dedup::{compute_sha256, FileHash};
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
        let raw = compute_sha256(path).unwrap();
        FileHash::new(&raw).unwrap()
    }

    fn legacy_files_in(source: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(dir) = std::fs::read_dir(source) {
            for e in dir.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    if name.ends_with(".txt") || name.ends_with(".toml") {
                        out.push(name.to_string());
                    }
                }
            }
        }
        out.sort();
        out
    }

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
    fn classify_moves_to_category_and_records_relative_paths() {
        let (_dir, source, store) = setup_source();
        let foo = source.join("foo.jpg");
        fs::write(&foo, b"hello classify").unwrap();
        let hash = hash_of(&foo);
        let meta_before = fs::metadata(&foo).unwrap();

        let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
        assert!(!outcome.was_duplicate());
        let dest = outcome.into_dest();

        assert!(!foo.exists());
        assert_eq!(dest, source.join("keep").join("foo.jpg"));
        assert!(dest.exists());
        // Database row present with Source Folder-relative paths.
        let rec = store.lookup(hash.as_str()).unwrap().expect("row must exist");
        assert_eq!(rec.original_rel, "foo.jpg");
        assert_eq!(rec.final_rel, "keep/foo.jpg");
        assert_eq!(rec.category_folder, "keep");
        assert_eq!(rec.size, meta_before.len() as i64);
        // No legacy txt/toml logs created by the database path.
        assert!(!source.join("keep.txt").exists());
        assert!(legacy_files_in(&source).is_empty());
    }

    #[test]
    fn classify_creates_no_legacy_files_across_categories() {
        let (_dir, source, store) = setup_source();
        for (name, content, category) in [
            ("a.jpg", b"content a".as_slice(), "keep"),
            ("b.jpg", b"content b".as_slice(), "maybe"),
            ("c.jpg", b"content c".as_slice(), "reject"),
        ] {
            let p = source.join(name);
            fs::write(&p, content).unwrap();
            let hash = hash_of(&p);
            classify_file(&store, &p, category, &hash).unwrap();
        }
        assert!(legacy_files_in(&source).is_empty(), "Classification must create no .txt/.toml");
        assert_eq!(store.all_hashes().unwrap().len(), 3);
    }

    #[test]
    fn legacy_txt_hash_does_not_trigger_duplicate_routing() {
        let (_dir, source, store) = setup_source();
        let foo = source.join("foo.jpg");
        fs::write(&foo, b"hello legacy ignored").unwrap();
        let hash = hash_of(&foo);
        // Legacy txt log containing the same hash must be ignored by Duplicate checks.
        fs::write(source.join("keep.txt"), format!("{}\n", hash.as_str())).unwrap();
        fs::write(source.join("organizer.toml"), b"legacy = true\n").unwrap();

        let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
        assert!(!outcome.was_duplicate(), "legacy txt must not count as Duplicate");
        assert_eq!(outcome.dest(), &source.join("keep").join("foo.jpg"));
        assert_eq!(store.all_hashes().unwrap().len(), 1);

        // Second file with the same content now duplicates via the database.
        let bar = source.join("bar.jpg");
        fs::write(&bar, b"hello legacy ignored").unwrap();
        let hash_bar = hash_of(&bar);
        let outcome2 = classify_file(&store, &bar, "keep", &hash_bar).unwrap();
        assert!(outcome2.was_duplicate());
        assert_eq!(outcome2.dest(), &source.join("duplicate").join("bar.jpg"));
    }

    #[test]
    fn duplicate_routes_to_duplicate_with_origin_and_no_growth() {
        let (_dir, source, store) = setup_source();
        let a = source.join("a.jpg");
        fs::write(&a, b"same content").unwrap();
        let hash = hash_of(&a);
        let outcome_a = classify_file(&store, &a, "keep", &hash).unwrap();
        assert!(!outcome_a.was_duplicate());
        assert_eq!(outcome_a.dest(), &source.join("keep").join("a.jpg"));
        assert_eq!(store.all_hashes().unwrap().len(), 1);
        assert_eq!(store.origin(hash.as_str()).unwrap().as_deref(), Some("keep"));

        let b = source.join("b.jpg");
        fs::write(&b, b"same content").unwrap();
        let hash_b = hash_of(&b);
        assert_eq!(hash.as_str(), hash_b.as_str());
        let outcome_b = classify_file(&store, &b, "keep", &hash_b).unwrap();
        assert!(outcome_b.was_duplicate());
        assert_eq!(
            outcome_b.dest(),
            &source.join("duplicate").join("b.jpg"),
            "duplicate hash must route to duplicate/"
        );
        assert!(outcome_b.dest().exists());
        assert!(!source.join("b.jpg").exists());
        // No extra record growth.
        assert_eq!(store.all_hashes().unwrap().len(), 1);
        let rec = store.lookup(hash.as_str()).unwrap().unwrap();
        assert_eq!(rec.original_rel, "a.jpg");
        assert_eq!(rec.category_folder, "keep");
        // Origin still keep for toast.
        assert_eq!(store.origin(hash_b.as_str()).unwrap().as_deref(), Some("keep"));
    }

    #[test]
    fn classify_suffix_on_clash_records_suffixed_final_rel() {
        let (_dir, source, store) = setup_source();
        let dest_dir = source.join("keep");
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(dest_dir.join("foo.jpg"), b"existing").unwrap();

        let foo = source.join("foo.jpg");
        fs::write(&foo, b"new content").unwrap();
        let hash = hash_of(&foo);
        let outcome = classify_file(&store, &foo, "keep", &hash).unwrap();
        assert_eq!(outcome.dest(), &dest_dir.join("foo_1.jpg"));
        let rec = store.lookup(hash.as_str()).unwrap().unwrap();
        assert_eq!(rec.final_rel, "keep/foo_1.jpg");
    }

    #[test]
    fn classify_invalid_hash_leaves_file_in_place() {
        let (_dir, source, store) = setup_source();
        let foo = source.join("foo.jpg");
        fs::write(&foo, b"data").unwrap();
        // Invalid hashes are rejected at the FileHash boundary before Classification.
        assert!(FileHash::new("not-a-hash").is_err());
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
        let hash = hash_of(&foo);
        classify_file(&store, &foo, "keep", &hash).unwrap();
        drop(store);
        let moved = dir.path().join("moved");
        fs::rename(&source, &moved).unwrap();
        let reopened = Store::open(&moved).unwrap();
        let rec = reopened.lookup(hash.as_str()).unwrap().unwrap();
        assert_eq!(rec.original_rel, "foo.jpg");
        assert_eq!(rec.final_rel, "keep/foo.jpg");
        assert!(moved.join(&rec.final_rel).exists());
    }
}
