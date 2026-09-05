use crate::config::{default_config, validate_actions, Action, ValidationError};
use crate::dedup::{is_valid_hash, FileHash};
use rusqlite::{params, Connection, TransactionBehavior};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// File name of the portable Organizer Database inside the Source Folder.
pub const DB_FILENAME: &str = "organizer.db";

/// Suffix artifacts created by SQLite WAL mode that must never enter the Queue.
const DB_ARTIFACT_SUFFIXES: &[&str] = &["-wal", "-shm", "-journal"];

/// Returns the database path for a Source Folder.
pub fn db_path_for(source_folder: &Path) -> PathBuf {
    source_folder.join(DB_FILENAME)
}

/// Returns true if `path` is the Organizer Database or one of its WAL artifacts.
/// Used to keep internal files out of Queue Snapshots (future Classification/Sweep).
pub fn is_internal_db_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == DB_FILENAME {
        return true;
    }
    if name.starts_with(DB_FILENAME) {
        for suffix in DB_ARTIFACT_SUFFIXES {
            if name == format!("{DB_FILENAME}{suffix}") {
                return true;
            }
        }
    }
    false
}

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Validation(Vec<ValidationError>),
    InvalidHash(String),
    InvalidPath(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "database error: {e}"),
            StoreError::Io(e) => write!(f, "io error: {e}"),
            StoreError::Validation(errs) => {
                let msg = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
                write!(f, "invalid actions: {msg}")
            }
            StoreError::InvalidHash(h) => write!(f, "invalid hash: {h}"),
            StoreError::InvalidPath(p) => write!(f, "invalid relative path: {p}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sql(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// One triaged file row, keyed by lower-case sha256.
/// All paths are Source Folder-relative so the folder plus database move together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub hash: String,
    pub original_rel: String,
    pub final_rel: String,
    pub action_folder: String,
    pub size: i64,
    pub mtime: i64,
    pub triaged_at: i64,
}

impl FileRecord {
    /// Validate and normalize. Hash is lower-cased; paths must be relative and non-empty.
    pub fn new(
        hash: &str,
        original_rel: &str,
        final_rel: &str,
        action_folder: &str,
        size: i64,
        mtime: i64,
        triaged_at: i64,
    ) -> Result<Self, StoreError> {
        let validated = FileHash::new(hash).map_err(|_| StoreError::InvalidHash(hash.to_string()))?;
        if !is_valid_hash(validated.as_str()) {
            return Err(StoreError::InvalidHash(hash.to_string()));
        }
        for (label, p) in [("original", original_rel), ("final", final_rel)] {
            if p.is_empty() {
                return Err(StoreError::InvalidPath(format!("{label} path empty")));
            }
            let path = Path::new(p);
            if path.is_absolute() {
                return Err(StoreError::InvalidPath(p.to_string()));
            }
            // Reject parent escapes so relative paths cannot leave the Source Folder.
            if p == ".." || p.starts_with("../") || p.contains("/../") || p.ends_with("/..") {
                return Err(StoreError::InvalidPath(p.to_string()));
            }
        }
        if action_folder.is_empty() {
            return Err(StoreError::InvalidPath("action folder empty".into()));
        }
        Ok(Self {
            hash: validated.to_string(),
            original_rel: original_rel.to_string(),
            final_rel: final_rel.to_string(),
            action_folder: action_folder.to_string(),
            size,
            mtime,
            triaged_at: triaged_at,
        })
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// Portable Organizer Database inside the Source Folder.
///
/// Beside legacy storage (organizer.toml + *.txt): opening a Store never reads
/// or writes legacy files. Each operation opens a short-lived SQLite connection
/// in WAL mode with `busy_timeout` so concurrent openers serialize via
/// `BEGIN IMMEDIATE` instead of corrupting the database.
#[derive(Debug, Clone)]
pub struct Store {
    source_folder: PathBuf,
}

impl Store {
    /// Open (or seed) the Organizer Database in `source_folder`.
    /// Creates the folder if missing, enables WAL, creates schema, seeds defaults.
    /// Never touches `organizer.toml` or `*.txt`.
    pub fn open(source_folder: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(source_folder)?;
        let mut conn = Self::connect(source_folder)?;
        Self::ensure_schema(&conn)?;
        Self::seed_if_empty(&mut conn)?;
        Ok(Self {
            source_folder: source_folder.to_path_buf(),
        })
    }

    pub fn source_folder(&self) -> &Path {
        &self.source_folder
    }

    pub fn db_path(&self) -> PathBuf {
        db_path_for(&self.source_folder)
    }

    fn connect(source_folder: &Path) -> Result<Connection, StoreError> {
        let path = db_path_for(source_folder);
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;",
        )?;
        Ok(conn)
    }

    fn connection(&self) -> Result<Connection, StoreError> {
        Self::connect(&self.source_folder)
    }

    fn ensure_schema(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS actions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                display_name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                folder_name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                shortcut TEXT NOT NULL UNIQUE,
                position INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS files (
                hash TEXT PRIMARY KEY,
                original_rel TEXT NOT NULL,
                final_rel TEXT NOT NULL,
                action_folder TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                triaged_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_files_action ON files(action_folder);
            INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1');
            INSERT OR IGNORE INTO meta (key, value) VALUES ('config_version', '1');",
        )?;
        Ok(())
    }

    fn seed_if_empty(conn: &mut Connection) -> Result<(), StoreError> {
        // Acquire the write lock first so two fresh openers serialize:
        // the second blocks on BEGIN IMMEDIATE, then sees COUNT > 0.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get(0))?;
        if count == 0 {
            let defaults = default_config();
            // Defaults are known-valid; validation here guards against regressions.
            validate_actions(&defaults.actions).map_err(StoreError::Validation)?;
            for (pos, a) in defaults.actions.iter().enumerate() {
                // OR IGNORE keeps a lost race (both saw 0 before locks existed)
                // from corrupting: second opener becomes a no-op reopen.
                tx.execute(
                    "INSERT OR IGNORE INTO actions (display_name, folder_name, shortcut, position) VALUES (?1, ?2, ?3, ?4)",
                    params![a.display_name, a.folder_name, a.shortcut, pos as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn insert_action_row(
        tx: &rusqlite::Transaction<'_>,
        action: &Action,
        position: usize,
    ) -> Result<(), StoreError> {
        tx.execute(
            "INSERT INTO actions (display_name, folder_name, shortcut, position) VALUES (?1, ?2, ?3, ?4)",
            params![
                action.display_name,
                action.folder_name,
                action.shortcut,
                position as i64
            ],
        )?;
        Ok(())
    }

    fn file_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
        Ok(FileRecord {
            hash: row.get(0)?,
            original_rel: row.get(1)?,
            final_rel: row.get(2)?,
            action_folder: row.get(3)?,
            size: row.get(4)?,
            mtime: row.get(5)?,
            triaged_at: row.get(6)?,
        })
    }

    /// Current Actions ordered by position.
    pub fn actions(&self) -> Result<Vec<Action>, StoreError> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT display_name, folder_name, shortcut FROM actions ORDER BY position ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Action {
                display_name: row.get(0)?,
                folder_name: row.get(1)?,
                shortcut: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Replace all Actions atomically after running existing validation.
    /// On validation failure the database is left unchanged.
    pub fn set_actions(&self, actions: &[Action]) -> Result<(), StoreError> {
        validate_actions(actions).map_err(StoreError::Validation)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM actions", [])?;
        for (pos, a) in actions.iter().enumerate() {
            Self::insert_action_row(&tx, a, pos)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// True if `hash` (any case) is already known.
    pub fn contains(&self, hash: &str) -> Result<bool, StoreError> {
        Ok(self.lookup(hash)?.is_some())
    }

    /// Lookup a file row by hash (case-insensitive). Returns None for unknown or invalid hashes.
    pub fn lookup(&self, hash: &str) -> Result<Option<FileRecord>, StoreError> {
        let lower = hash.to_ascii_lowercase();
        if !is_valid_hash(&lower) {
            return Ok(None);
        }
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT hash, original_rel, final_rel, action_folder, size, mtime, triaged_at FROM files WHERE hash = ?1",
        )?;
        let mut rows = stmt.query_map(params![lower], Self::file_record_from_row)?;
        match rows.next() {
            Some(Ok(rec)) => Ok(Some(rec)),
            Some(Err(e)) => Err(StoreError::Sql(e)),
            None => Ok(None),
        }
    }

    /// Insert a triaged hash row. Returns true if inserted, false if the hash
    /// already existed (idempotent, existing row kept). Uses BEGIN IMMEDIATE
    /// so concurrent openers serialize instead of corrupting.
    pub fn insert_file(&self, record: &FileRecord) -> Result<bool, StoreError> {
        // Re-validate to enforce lower-case + hex even if caller built the struct manually.
        let validated = FileHash::new(&record.hash).map_err(|_| StoreError::InvalidHash(record.hash.clone()))?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO files (hash, original_rel, final_rel, action_folder, size, mtime, triaged_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                validated.as_str(),
                record.original_rel,
                record.final_rel,
                record.action_folder,
                record.size,
                record.mtime,
                record.triaged_at
            ],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Remove a hash row (for Undo). Returns true if a row was removed.
    pub fn remove(&self, hash: &str) -> Result<bool, StoreError> {
        let lower = hash.to_ascii_lowercase();
        if !is_valid_hash(&lower) {
            return Err(StoreError::InvalidHash(hash.to_string()));
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute("DELETE FROM files WHERE hash = ?1", params![lower])?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Union of all known lower-case hashes.
    pub fn all_hashes(&self) -> Result<HashSet<String>, StoreError> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT hash FROM files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = HashSet::new();
        for r in rows {
            set.insert(r?);
        }
        Ok(set)
    }

    /// Origin Action folder for a known hash, if any.
    pub fn origin(&self, hash: &str) -> Result<Option<String>, StoreError> {
        Ok(self.lookup(hash)?.map(|r| r.action_folder))
    }

    /// All file rows ordered by hash (for Sweep reporting).
    pub fn list_files(&self) -> Result<Vec<FileRecord>, StoreError> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT hash, original_rel, final_rel, action_folder, size, mtime, triaged_at FROM files ORDER BY hash ASC",
        )?;
        let rows = stmt.query_map([], Self::file_record_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Current journal mode (used to assert WAL in tests).
    pub fn journal_mode(&self) -> Result<String, StoreError> {
        let conn = self.connection()?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        Ok(mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Action;
    use std::fs;
    use tempfile::TempDir;

    fn test_hash(n: u8) -> String {
        // Deterministic 64-hex lowercase hash for tests: repeat byte as hex.
        format!("{:02x}", n).repeat(32)
    }

    fn sample_record(hash: &str) -> FileRecord {
        FileRecord::new(hash, "a.jpg", "keep/a.jpg", "keep", 123, 456, 789).unwrap()
    }

    #[test]
    fn fresh_seeds_defaults_and_touches_no_legacy() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        fs::create_dir_all(&source).unwrap();
        // Pre-existing legacy files must be left alone.
        fs::write(source.join("organizer.toml"), b"legacy").unwrap();
        fs::write(
            source.join("keep.txt"),
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();

        let store = Store::open(&source).unwrap();
        let actions = store.actions().unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].display_name, "Keep");
        assert_eq!(actions[0].folder_name, "keep");
        assert_eq!(actions[0].shortcut, "1");

        // Database exists, legacy untouched.
        assert!(store.db_path().exists());
        assert_eq!(fs::read(source.join("organizer.toml")).unwrap(), b"legacy");
        assert!(fs::read_to_string(source.join("keep.txt")).unwrap().contains("aaaa"));
        // No new legacy files created.
        let entries: Vec<String> = fs::read_dir(&source)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(entries.contains(&"organizer.db".to_string()));
        // Only the one txt we created; store created no additional txt/toml.
        assert_eq!(entries.iter().filter(|n| n.ends_with(".txt")).count(), 1);
        assert_eq!(entries.iter().filter(|n| n.ends_with(".toml")).count(), 1);
    }

    #[test]
    fn reopen_preserves_custom_actions_does_not_reseed() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let custom = vec![
            Action { display_name: "A".into(), folder_name: "a".into(), shortcut: "1".into() },
            Action { display_name: "B".into(), folder_name: "b".into(), shortcut: "2".into() },
        ];
        store.set_actions(&custom).unwrap();
        // Reopen must keep custom, not reseed defaults.
        let store2 = Store::open(&source).unwrap();
        assert_eq!(store2.actions().unwrap(), custom);
    }

    #[test]
    fn set_actions_rejects_invalid_and_leaves_db_unchanged() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let before = store.actions().unwrap();

        // Duplicate shortcut.
        let dup_short = vec![
            Action { display_name: "A".into(), folder_name: "a".into(), shortcut: "1".into() },
            Action { display_name: "B".into(), folder_name: "b".into(), shortcut: "1".into() },
        ];
        assert!(store.set_actions(&dup_short).is_err());
        // Reserved duplicate.
        let reserved = vec![
            Action { display_name: "Dup".into(), folder_name: "duplicate".into(), shortcut: "1".into() },
        ];
        assert!(store.set_actions(&reserved).is_err());
        // Duplicate display case-insensitive.
        let dup_display = vec![
            Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Action { display_name: "keep".into(), folder_name: "keep2".into(), shortcut: "2".into() },
        ];
        assert!(store.set_actions(&dup_display).is_err());
        // Too few / too many / invalid shortcut.
        assert!(store.set_actions(&[]).is_err());
        let mut nine = Vec::new();
        for i in 1..=9 {
            nine.push(Action { display_name: format!("A{i}"), folder_name: format!("a{i}"), shortcut: format!("{i}") });
        }
        assert!(store.set_actions(&nine).is_ok());
        let mut ten = nine.clone();
        ten.push(Action { display_name: "A10".into(), folder_name: "a10".into(), shortcut: "1".into() });
        assert!(store.set_actions(&ten).is_err());
        let bad_short = vec![
            Action { display_name: "A".into(), folder_name: "a".into(), shortcut: "a".into() },
        ];
        assert!(store.set_actions(&bad_short).is_err());

        // Failed validations must not have clobbered the last good state (nine).
        assert_eq!(store.actions().unwrap(), nine);
        // Restore defaults check: before was 3 defaults, now nine — proves writes work when valid.
        assert_ne!(before, nine);
    }

    #[test]
    fn hash_insert_lookup_roundtrip_lowercases_and_idempotent() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let upper = test_hash(0xAB).to_ascii_uppercase();
        let rec = FileRecord::new(&upper, "orig.jpg", "keep/orig.jpg", "keep", 10, 20, 30).unwrap();
        // Normalized to lower-case.
        assert_eq!(rec.hash, rec.hash.to_ascii_lowercase());
        assert!(store.insert_file(&rec).unwrap());
        // Duplicate insert is idempotent, keeps first row.
        let rec2 = FileRecord::new(&upper.to_ascii_lowercase(), "other.jpg", "maybe/other.jpg", "maybe", 99, 99, 99).unwrap();
        assert!(!store.insert_file(&rec2).unwrap());
        let got = store.lookup(&upper).unwrap().unwrap();
        assert_eq!(got.original_rel, "orig.jpg");
        assert_eq!(got.action_folder, "keep");
        // Case-insensitive lookup.
        assert!(store.contains(&upper.to_ascii_lowercase()).unwrap());
        assert!(store.contains(&upper).unwrap());
        // Unknown hash.
        assert!(!store.contains(&test_hash(0x01)).unwrap());
        assert!(store.lookup(&test_hash(0x01)).unwrap().is_none());
        // Union has exactly one.
        assert_eq!(store.all_hashes().unwrap().len(), 1);
        // Origin.
        assert_eq!(store.origin(&upper).unwrap().as_deref(), Some("keep"));
    }

    #[test]
    fn invalid_hash_rejected_and_leaves_db_unchanged() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        assert!(FileRecord::new("not-a-hash", "a.jpg", "keep/a.jpg", "keep", 0, 0, 0).is_err());
        // Direct invalid via lookup returns None, remove errors.
        assert!(store.lookup("not-a-hash").unwrap().is_none());
        assert!(store.remove("not-a-hash").is_err());
        assert!(store.all_hashes().unwrap().is_empty());
    }

    #[test]
    fn relative_paths_survive_folder_move() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let h = test_hash(0x42);
        let rec = FileRecord::new(&h, "a.jpg", "keep/a.jpg", "keep", 5, 6, 7).unwrap();
        store.insert_file(&rec).unwrap();
        // Move the whole Source Folder (folder plus database move together).
        let moved = dir.path().join("moved");
        fs::rename(&source, &moved).unwrap();
        let reopened = Store::open(&moved).unwrap();
        let got = reopened.lookup(&h).unwrap().unwrap();
        assert_eq!(got.original_rel, "a.jpg");
        assert_eq!(got.final_rel, "keep/a.jpg");
        assert_eq!(got.action_folder, "keep");
        // Relative resolution still works at the new location.
        assert_eq!(moved.join(&got.final_rel), moved.join("keep/a.jpg"));
        assert!(reopened.contains(&h).unwrap());
    }

    #[test]
    fn journal_mode_is_wal() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        assert_eq!(store.journal_mode().unwrap().to_ascii_lowercase(), "wal");
    }

    #[test]
    fn concurrent_openers_do_not_corrupt() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        Store::open(&source).unwrap();
        let mut handles = Vec::new();
        for t in 0..8u8 {
            let path = source.clone();
            handles.push(std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                for i in 0..10u8 {
                    let byte = t.wrapping_mul(16).wrapping_add(i);
                    let h = format!("{:02x}", byte).repeat(32);
                    let rec = FileRecord::new(&h, "a.jpg", "keep/a.jpg", "keep", 1, 1, 1).unwrap();
                    // Retry on busy — busy_timeout usually suffices, but be robust.
                    for _ in 0..20 {
                        match store.insert_file(&rec) {
                            Ok(_) => break,
                            Err(StoreError::Sql(rusqlite::Error::SqliteFailure(e, _)))
                                if e.code == rusqlite::ErrorCode::DatabaseBusy =>
                            {
                                std::thread::sleep(std::time::Duration::from_millis(5));
                                continue;
                            }
                            Err(e) => panic!("insert failed: {e:?}"),
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let store = Store::open(&source).unwrap();
        // 8 threads * 10 distinct hashes = 80 rows; integrity check passes.
        assert_eq!(store.all_hashes().unwrap().len(), 80);
        let conn = Connection::open(db_path_for(&source)).unwrap();
        conn.execute_batch("PRAGMA busy_timeout=5000;").unwrap();
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check.to_ascii_lowercase(), "ok");
    }

    #[test]
    fn concurrent_fresh_openers_seed_once_without_corruption() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("fresh");
        // Do not pre-seed: all threads race Store::open on an empty folder.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = source.clone();
            handles.push(std::thread::spawn(move || {
                // Retry on busy during the seeding race.
                for _ in 0..20 {
                    match Store::open(&path) {
                        Ok(_) => break,
                        Err(StoreError::Sql(rusqlite::Error::SqliteFailure(e, _)))
                            if e.code == rusqlite::ErrorCode::DatabaseBusy =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        Err(e) => panic!("fresh open failed: {e:?}"),
                    }
                    break;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let store = Store::open(&source).unwrap();
        assert_eq!(store.actions().unwrap().len(), 3);
        let conn = Connection::open(db_path_for(&source)).unwrap();
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check.to_ascii_lowercase(), "ok");
    }

    #[test]
    fn remove_reverts_row_for_undo() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let h = test_hash(0x77);
        store.insert_file(&sample_record(&h)).unwrap();
        assert!(store.contains(&h).unwrap());
        assert!(store.remove(&h).unwrap());
        assert!(!store.contains(&h).unwrap());
        assert!(!store.remove(&h).unwrap());
    }

    #[test]
    fn is_internal_db_file_matches_artifacts() {
        assert!(is_internal_db_file(Path::new("organizer.db")));
        assert!(is_internal_db_file(Path::new("/tmp/src/organizer.db")));
        assert!(is_internal_db_file(Path::new("/tmp/src/organizer.db-wal")));
        assert!(is_internal_db_file(Path::new("/tmp/src/organizer.db-shm")));
        assert!(!is_internal_db_file(Path::new("photo.jpg")));
        assert!(!is_internal_db_file(Path::new("organizer.toml")));
        assert!(!is_internal_db_file(Path::new("keep.txt")));
    }
}
