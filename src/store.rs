use crate::config::{default_categories, validate_categories, Category, ValidationError};
use crate::dedup::{is_valid_hash, FileHash};
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
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

/// Single constructor for the missing-reference-database message, shared by
/// `Store::open_reference_file` and Sweep's `MissingDatabase` display so the
/// two wordings cannot drift apart (Duplicated Code fix).
pub fn missing_reference_message(db_path: &Path) -> String {
    format!(
        "reference database not found: {} (default ./{} in current folder; override with --db PATH)",
        db_path.display(),
        DB_FILENAME
    )
}

/// Returns true if `path` is the Organizer Database or one of its WAL artifacts.
/// Used to keep internal files out of the Queue Snapshot (future Classification/Sweep).
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
    UnsupportedSchema { found: String },
    InvalidDatabase { path: PathBuf, reason: String },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sql(e) => write!(f, "database error: {e}"),
            StoreError::Io(e) => write!(f, "io error: {e}"),
            StoreError::Validation(errs) => {
                let msg = errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
                write!(f, "invalid categories: {msg}")
            }
            StoreError::InvalidHash(h) => write!(f, "invalid hash: {h}"),
            StoreError::InvalidPath(p) => write!(f, "invalid relative path: {p}"),
            StoreError::UnsupportedSchema { found } => write!(
                f,
                "unsupported organizer.db schema_version {found:?} (expected \"1\"); move the Source Folder aside or delete organizer.db to reseed"
            ),
            StoreError::InvalidDatabase { path, reason } => write!(
                f,
                "invalid organizer database: {} ({})",
                path.display(),
                reason
            ),
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
    pub category_folder: String,
    pub size: i64,
    pub mtime: i64,
    pub triaged_at: i64,
}

impl FileRecord {
    /// Build a validated row. The hash arrives as the `FileHash` domain type
    /// (already lower-case hex), so callers cannot pass raw strings here;
    /// paths must be relative and non-empty.
    pub fn new(
        hash: &FileHash,
        original_rel: &str,
        final_rel: &str,
        category_folder: &str,
        size: i64,
        mtime: i64,
        triaged_at: i64,
    ) -> Result<Self, StoreError> {
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
        if category_folder.is_empty() {
            return Err(StoreError::InvalidPath("category folder empty".into()));
        }
        Ok(Self {
            hash: hash.to_string(),
            original_rel: original_rel.to_string(),
            final_rel: final_rel.to_string(),
            category_folder: category_folder.to_string(),
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
/// Sole store since #25: opening a Store never reads or writes legacy
/// `organizer.toml` or `*.txt` files. Each operation opens a short-lived
/// SQLite connection in WAL mode with `busy_timeout` so concurrent openers
/// serialize via `BEGIN IMMEDIATE` instead of corrupting the database.
///
/// Record layer only: filesystem moves stay in `mover` (Classification
/// combines move + insert and rolls the row back if the rename fails).
#[derive(Debug, Clone)]
pub struct Store {
    source_folder: PathBuf,
    db_path: PathBuf,
    /// True for Sweep reference handles (#26): every connection opens
    /// `SQLITE_OPEN_READ_ONLY`, so cleaning a target can never change the
    /// reference database content. (WAL `-shm`/`-wal` index files may still
    /// materialize; see `connect_reference_file`.)
    read_only: bool,
}

/// True for transient lock contention that is safe to retry.
fn is_busy(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Sql(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::DatabaseBusy
                || e.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// Run a write operation, retrying transient `BUSY`/`LOCKED` with backoff so
/// callers serialize instead of seeing spurious lock errors under contention.
/// `busy_timeout=5000` already covers most waits; this covers the residual race.
fn retry_on_busy<T>(mut op: impl FnMut() -> Result<T, StoreError>) -> Result<T, StoreError> {
    const ATTEMPTS: usize = 50;
    let mut last: Option<StoreError> = None;
    for attempt in 0..ATTEMPTS {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if is_busy(&e) && attempt + 1 < ATTEMPTS => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("retry loop must have attempted"))
}

/// Read-only handle to a reference Organizer Database for Sweep.
///
/// Separate type (not just a flag) so Sweep code cannot name a write method:
/// `set_categories` / `insert_file` / `remove` simply do not exist here.
/// The wrapped handle additionally opens every connection
/// `SQLITE_OPEN_READ_ONLY`, so even the read path cannot change reference
/// rows. (WAL `-shm`/`-wal` index files may still materialize beside the
/// database; content is what is protected, and the Queue Snapshot excludes
/// those artifacts.)
#[derive(Debug, Clone)]
pub struct ReferenceStore {
    store: Store,
}

impl ReferenceStore {
    pub fn source_folder(&self) -> &Path {
        self.store.source_folder()
    }

    pub fn db_path(&self) -> PathBuf {
        self.store.db_path()
    }

    /// Current Categories ordered by position.
    pub fn categories(&self) -> Result<Vec<Category>, StoreError> {
        self.store.categories()
    }

    /// True if `hash` (any case) is already known.
    pub fn contains(&self, hash: &str) -> Result<bool, StoreError> {
        self.store.contains(hash)
    }

    /// Lookup a file row by hash (case-insensitive). Returns None for unknown or invalid hashes.
    pub fn lookup(&self, hash: &str) -> Result<Option<FileRecord>, StoreError> {
        self.store.lookup(hash)
    }

    /// Union of all known lower-case hashes (Sweep match seam).
    pub fn all_hashes(&self) -> Result<HashSet<String>, StoreError> {
        self.store.all_hashes()
    }

    /// Origin Category folder for a known hash, if any (Sweep report seam).
    pub fn origin(&self, hash: &str) -> Result<Option<String>, StoreError> {
        self.store.origin(hash)
    }

    /// All file rows ordered by hash (Sweep report seam).
    pub fn list_files(&self) -> Result<Vec<FileRecord>, StoreError> {
        self.store.list_files()
    }
}

impl Store {
    /// Open (or seed) the Organizer Database in `source_folder`.
    /// Creates the folder if missing, enables WAL, creates schema, checks the
    /// schema version, seeds defaults.
    /// Never reads or writes legacy `organizer.toml` or `*.txt` (#25).
    pub fn open(source_folder: &Path) -> Result<Self, StoreError> {
        retry_on_busy(|| {
            std::fs::create_dir_all(source_folder)?;
            let mut conn = Self::connect(source_folder)?;
            Self::ensure_schema(&conn)?;
            let db_path = db_path_for(source_folder);
            Self::check_schema_version(&conn, &db_path)?;
            Self::seed_if_empty(&mut conn)?;
            Ok(Self {
                source_folder: source_folder.to_path_buf(),
                db_path,
                read_only: false,
            })
        })
    }

    /// Open an existing reference database for Sweep (#26) without creating,
    /// seeding, or writing rows. `db_path` is the exact `organizer.db` file
    /// (resolved by `sweep::resolve_reference_db_path` from `--db` or its default).
    /// Errors clearly when the file is missing so callers can hint `--db`.
    /// Returns a `ReferenceStore`: the read-only surface is a separate type,
    /// so Sweep code cannot even name a write method — on top of the
    /// `SQLITE_OPEN_READ_ONLY` connections, which prevent any row change
    /// (WAL index sidecars may still materialize; content cannot).
    pub fn open_reference_file(db_path: &Path) -> Result<ReferenceStore, StoreError> {
        if !db_path.is_file() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                missing_reference_message(db_path),
            )));
        }
        let conn = Self::connect_reference_file(db_path)?;
        Self::check_schema_version(&conn, db_path)?;
        let source_folder = db_path
            .parent()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    p.to_path_buf()
                }
            })
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(ReferenceStore {
            store: Self {
                source_folder,
                db_path: db_path.to_path_buf(),
                read_only: true,
            },
        })
    }

    pub fn source_folder(&self) -> &Path {
        &self.source_folder
    }

    pub fn db_path(&self) -> PathBuf {
        self.db_path.clone()
    }

    fn connect(source_folder: &Path) -> Result<Connection, StoreError> {
        Self::connect_file(&db_path_for(source_folder))
    }

    fn connect_file(db_path: &Path) -> Result<Connection, StoreError> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;",
        )?;
        Ok(conn)
    }

    /// Read-only connection for Sweep reference handles: no WAL promotion,
    /// no synchronous change, only a per-connection busy timeout. Combined
    /// with `SQLITE_OPEN_READ_ONLY` the reference database content can never
    /// change while a target is cleaned. Note: WAL `-shm`/`-wal` index files
    /// may still materialize beside it (SQLite reader behavior, not content);
    /// the Queue Snapshot excludes them via `is_internal_db_file`.
    fn connect_reference_file(db_path: &Path) -> Result<Connection, StoreError> {
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        Ok(conn)
    }

    fn connection(&self) -> Result<Connection, StoreError> {
        if self.read_only {
            Self::connect_reference_file(&self.db_path)
        } else {
            Self::connect_file(&self.db_path)
        }
    }

    fn ensure_schema(conn: &Connection) -> Result<(), StoreError> {
        Self::migrate_legacy_action_schema(conn)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS categories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                display_name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                folder_name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                shortcut TEXT NOT NULL UNIQUE,
                position INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS files (
                hash TEXT PRIMARY KEY CHECK (length(hash) = 64 AND hash = lower(hash)),
                original_rel TEXT NOT NULL,
                final_rel TEXT NOT NULL,
                category_folder TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                triaged_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_files_category ON files(category_folder);
            INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1');
            INSERT OR IGNORE INTO meta (key, value) VALUES ('config_version', '1');",
        )?;
        Ok(())
    }

    /// Migrate pre-rename databases to the Category vocabulary without losing rows.
    /// Older opens created an `actions` table and `files.action_folder`; the rename
    /// carries them to `categories` / `category_folder` (copying rows when the new
    /// table already exists but is empty). Fresh databases skip this entirely.
    fn migrate_legacy_action_schema(conn: &Connection) -> Result<(), StoreError> {
        let table_exists = |name: &str| -> Result<bool, StoreError> {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
                params![name],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        };
        if table_exists("actions")? {
            if !table_exists("categories")? {
                conn.execute_batch("ALTER TABLE actions RENAME TO categories;")?;
            } else {
                let legacy_count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get(0))?;
                let current_count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0))?;
                if legacy_count > 0 && current_count == 0 {
                    conn.execute_batch(
                        "INSERT INTO categories (display_name, folder_name, shortcut, position) SELECT display_name, folder_name, shortcut, position FROM actions;",
                    )?;
                }
                conn.execute_batch("DROP TABLE actions;")?;
            }
        }
        if table_exists("files")? {
            let mut stmt = conn.prepare("PRAGMA table_info(files)")?;
            let cols: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<_, _>>()?;
            if cols.iter().any(|c| c == "action_folder")
                && !cols.iter().any(|c| c == "category_folder")
            {
                conn.execute_batch(
                    "ALTER TABLE files RENAME COLUMN action_folder TO category_folder;",
                )?;
            }
        }
        conn.execute_batch("DROP INDEX IF EXISTS idx_files_action;")?;
        Ok(())
    }

    /// Fail clearly on a database from a newer (or older) schema instead of
    /// silently accepting rows we cannot interpret. A file with no `meta`
    /// row (empty or foreign SQLite file) reports `InvalidDatabase` with the
    /// path, so callers can hint `--db` instead of showing rusqlite internals.
    fn check_schema_version(conn: &Connection, db_path: &Path) -> Result<(), StoreError> {
        let found: Result<String, rusqlite::Error> =
            conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| {
                r.get(0)
            });
        match found {
            Ok(v) if v == "1" => Ok(()),
            Ok(found) => Err(StoreError::UnsupportedSchema { found }),
            Err(e) => Err(StoreError::InvalidDatabase {
                path: db_path.to_path_buf(),
                reason: e.to_string(),
            }),
        }
    }

    fn seed_if_empty(conn: &mut Connection) -> Result<(), StoreError> {
        // Acquire the write lock first so two fresh openers serialize:
        // the second blocks on BEGIN IMMEDIATE, then sees COUNT > 0.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0))?;
        if count == 0 {
            let defaults = default_categories();
            // Defaults are known-valid; validation here guards against regressions.
            validate_categories(&defaults).map_err(StoreError::Validation)?;
            for (pos, c) in defaults.iter().enumerate() {
                // OR IGNORE keeps a lost race (both saw 0 before locks existed)
                // from corrupting: second opener becomes a no-op reopen.
                tx.execute(
                    "INSERT OR IGNORE INTO categories (display_name, folder_name, shortcut, position) VALUES (?1, ?2, ?3, ?4)",
                    params![c.display_name, c.folder_name, c.shortcut, pos as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn insert_category_row(
        tx: &rusqlite::Transaction<'_>,
        category: &Category,
        position: usize,
    ) -> Result<(), StoreError> {
        tx.execute(
            "INSERT INTO categories (display_name, folder_name, shortcut, position) VALUES (?1, ?2, ?3, ?4)",
            params![
                category.display_name,
                category.folder_name,
                category.shortcut,
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
            category_folder: row.get(3)?,
            size: row.get(4)?,
            mtime: row.get(5)?,
            triaged_at: row.get(6)?,
        })
    }

    /// Current Categories ordered by position.
    pub fn categories(&self) -> Result<Vec<Category>, StoreError> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT display_name, folder_name, shortcut FROM categories ORDER BY position ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Category {
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

    /// Replace all Categories atomically after running existing validation.
    /// On validation failure the database is left unchanged.
    pub fn set_categories(&self, categories: &[Category]) -> Result<(), StoreError> {
        validate_categories(categories).map_err(StoreError::Validation)?;
        retry_on_busy(|| {
            let mut conn = self.connection()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute("DELETE FROM categories", [])?;
            for (pos, c) in categories.iter().enumerate() {
                Self::insert_category_row(&tx, c, pos)?;
            }
            tx.commit()?;
            Ok(())
        })
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
            "SELECT hash, original_rel, final_rel, category_folder, size, mtime, triaged_at FROM files WHERE hash = ?1",
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
        // The DB CHECK(length(hash) = 64 AND hash = lower(hash)) is defense-in-depth.
        let validated = FileHash::new(&record.hash).map_err(|_| StoreError::InvalidHash(record.hash.clone()))?;
        let hash = validated.as_str().to_string();
        let original_rel = record.original_rel.clone();
        let final_rel = record.final_rel.clone();
        let category_folder = record.category_folder.clone();
        let (size, mtime, triaged_at) = (record.size, record.mtime, record.triaged_at);
        retry_on_busy(|| {
            let mut conn = self.connection()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = tx.execute(
                "INSERT OR IGNORE INTO files (hash, original_rel, final_rel, category_folder, size, mtime, triaged_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    hash,
                    original_rel,
                    final_rel,
                    category_folder,
                    size,
                    mtime,
                    triaged_at
                ],
            )?;
            tx.commit()?;
            Ok(changed == 1)
        })
    }

    /// Remove a hash row (Undo #24 seam). Returns true if a row was removed.
    pub fn remove(&self, hash: &str) -> Result<bool, StoreError> {
        let lower = hash.to_ascii_lowercase();
        if !is_valid_hash(&lower) {
            return Err(StoreError::InvalidHash(hash.to_string()));
        }
        retry_on_busy(|| {
            let mut conn = self.connection()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = tx.execute("DELETE FROM files WHERE hash = ?1", params![lower])?;
            tx.commit()?;
            Ok(changed == 1)
        })
    }

    /// Union of all known lower-case hashes (Sweep #26-28 match seam).
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

    /// Origin Category folder for a known hash, if any (Sweep report + Classification toast seam).
    pub fn origin(&self, hash: &str) -> Result<Option<String>, StoreError> {
        Ok(self.lookup(hash)?.map(|r| r.category_folder))
    }

    /// All file rows ordered by hash (Sweep report seam).
    pub fn list_files(&self) -> Result<Vec<FileRecord>, StoreError> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT hash, original_rel, final_rel, category_folder, size, mtime, triaged_at FROM files ORDER BY hash ASC",
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
    use crate::config::Category;
    use std::fs;
    use tempfile::TempDir;

    fn test_hash(n: u8) -> String {
        // Deterministic 64-hex lowercase hash for tests: repeat byte as hex.
        format!("{:02x}", n).repeat(32)
    }

    fn sample_record(hash: &str) -> FileRecord {
        FileRecord::new(
            &FileHash::new(hash).unwrap(),
            "a.jpg",
            "keep/a.jpg",
            "keep",
            123,
            456,
            789,
        )
        .unwrap()
    }

    #[test]
    fn fresh_seeds_defaults_and_ignores_legacy() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        fs::create_dir_all(&source).unwrap();
        // Pre-existing legacy files must be left alone and never read.
        fs::write(source.join("organizer.toml"), b"legacy").unwrap();
        fs::write(
            source.join("keep.txt"),
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        )
        .unwrap();

        let store = Store::open(&source).unwrap();
        let categories = store.categories().unwrap();
        assert_eq!(categories.len(), 3);
        assert_eq!(categories[0].display_name, "Keep");
        assert_eq!(categories[0].folder_name, "keep");
        assert_eq!(categories[0].shortcut, "1");

        // Database exists, legacy untouched and unread: the txt hash is NOT known.
        assert!(store.db_path().exists());
        assert_eq!(fs::read(source.join("organizer.toml")).unwrap(), b"legacy");
        assert!(fs::read_to_string(source.join("keep.txt")).unwrap().contains("aaaa"));
        assert!(
            !store
                .contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap(),
            "legacy txt hashes must be ignored by Duplicate checks (#25)"
        );
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
    fn legacy_action_schema_migrates_without_losing_rows() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let hash = test_hash(0x9E);
        // Hand-build a pre-rename database: `actions` table + `files.action_folder`.
        {
            let conn = Connection::open(db_path_for(&source)).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '1'), ('config_version', '1');
                 CREATE TABLE actions (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     display_name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                     folder_name TEXT NOT NULL UNIQUE COLLATE NOCASE,
                     shortcut TEXT NOT NULL UNIQUE,
                     position INTEGER NOT NULL
                 );
                 INSERT INTO actions (display_name, folder_name, shortcut, position) VALUES ('Night', 'night', '4', 0);
                 CREATE TABLE files (
                     hash TEXT PRIMARY KEY,
                     original_rel TEXT NOT NULL,
                     final_rel TEXT NOT NULL,
                     action_folder TEXT NOT NULL,
                     size INTEGER NOT NULL,
                     mtime INTEGER NOT NULL,
                     triaged_at INTEGER NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO files (hash, original_rel, final_rel, action_folder, size, mtime, triaged_at) VALUES (?1, 'n.jpg', 'night/n.jpg', 'night', 3, 4, 5)",
                rusqlite::params![hash],
            )
            .unwrap();
        }
        let store = Store::open(&source).unwrap();
        // Legacy rows carried over, not reseeded or dropped.
        let cats = store.categories().unwrap();
        assert_eq!(cats.len(), 1);
        assert_eq!(cats[0].folder_name, "night");
        let rec = store.lookup(&hash).unwrap().expect("file row must survive");
        assert_eq!(rec.category_folder, "night");
        assert_eq!(rec.final_rel, "night/n.jpg");
        assert!(store.contains(&hash).unwrap());
        assert_eq!(store.origin(&hash).unwrap().as_deref(), Some("night"));
        // Legacy objects gone; reopen is stable and idempotent.
        let conn = Connection::open(db_path_for(&source)).unwrap();
        let legacy_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'actions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_tables, 0);
        let reopened = Store::open(&source).unwrap();
        assert_eq!(reopened.categories().unwrap(), cats);
        assert!(reopened.contains(&hash).unwrap());
    }

    #[test]
    fn reopen_preserves_custom_categories_does_not_reseed() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let custom = vec![
            Category { display_name: "A".into(), folder_name: "a".into(), shortcut: "1".into() },
            Category { display_name: "B".into(), folder_name: "b".into(), shortcut: "2".into() },
        ];
        store.set_categories(&custom).unwrap();
        // Reopen must keep custom, not reseed defaults.
        let store2 = Store::open(&source).unwrap();
        assert_eq!(store2.categories().unwrap(), custom);
    }

    #[test]
    fn set_categories_rejects_invalid_and_leaves_db_unchanged() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let before = store.categories().unwrap();

        // Duplicate shortcut.
        let dup_short = vec![
            Category { display_name: "A".into(), folder_name: "a".into(), shortcut: "1".into() },
            Category { display_name: "B".into(), folder_name: "b".into(), shortcut: "1".into() },
        ];
        assert!(store.set_categories(&dup_short).is_err());
        // Reserved duplicate.
        let reserved = vec![
            Category { display_name: "Dup".into(), folder_name: "duplicate".into(), shortcut: "1".into() },
        ];
        assert!(store.set_categories(&reserved).is_err());
        // Duplicate display case-insensitive.
        let dup_display = vec![
            Category { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Category { display_name: "keep".into(), folder_name: "keep2".into(), shortcut: "2".into() },
        ];
        assert!(store.set_categories(&dup_display).is_err());
        // Too few / too many / invalid shortcut.
        assert!(store.set_categories(&[]).is_err());
        let mut nine = Vec::new();
        for i in 1..=9 {
            nine.push(Category { display_name: format!("A{i}"), folder_name: format!("a{i}"), shortcut: format!("{i}") });
        }
        assert!(store.set_categories(&nine).is_ok());
        let mut ten = nine.clone();
        ten.push(Category { display_name: "A10".into(), folder_name: "a10".into(), shortcut: "1".into() });
        assert!(store.set_categories(&ten).is_err());
        let bad_short = vec![
            Category { display_name: "A".into(), folder_name: "a".into(), shortcut: "a".into() },
        ];
        assert!(store.set_categories(&bad_short).is_err());

        // Failed validations must not have clobbered the last good state (nine).
        assert_eq!(store.categories().unwrap(), nine);
        // Restore defaults check: before was 3 defaults, now nine — proves writes work when valid.
        assert_ne!(before, nine);
    }

    #[test]
    fn hash_insert_lookup_roundtrip_lowercases_and_idempotent() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let upper = test_hash(0xAB).to_ascii_uppercase();
        let upper_hash = FileHash::new(&upper).unwrap();
        let rec = FileRecord::new(&upper_hash, "orig.jpg", "keep/orig.jpg", "keep", 10, 20, 30).unwrap();
        // Normalized to lower-case.
        assert_eq!(rec.hash, rec.hash.to_ascii_lowercase());
        assert!(store.insert_file(&rec).unwrap());
        // Duplicate insert is idempotent, keeps first row.
        let lower_hash = FileHash::new(&upper.to_ascii_lowercase()).unwrap();
        let rec2 = FileRecord::new(&lower_hash, "other.jpg", "maybe/other.jpg", "maybe", 99, 99, 99).unwrap();
        assert!(!store.insert_file(&rec2).unwrap());
        let got = store.lookup(&upper).unwrap().unwrap();
        assert_eq!(got.original_rel, "orig.jpg");
        assert_eq!(got.category_folder, "keep");
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
        // Raw strings can no longer reach FileRecord: the domain type rejects them.
        assert!(FileHash::new("not-a-hash").is_err());
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
        let hash = FileHash::new(&h).unwrap();
        let rec = FileRecord::new(&hash, "a.jpg", "keep/a.jpg", "keep", 5, 6, 7).unwrap();
        store.insert_file(&rec).unwrap();
        // Move the whole Source Folder (folder plus database move together).
        let moved = dir.path().join("moved");
        fs::rename(&source, &moved).unwrap();
        let reopened = Store::open(&moved).unwrap();
        let got = reopened.lookup(&h).unwrap().unwrap();
        assert_eq!(got.original_rel, "a.jpg");
        assert_eq!(got.final_rel, "keep/a.jpg");
        assert_eq!(got.category_folder, "keep");
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
    fn open_reference_file_exposes_reads_only_and_writes_no_rows() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let h = test_hash(0x5A);
        store.insert_file(&sample_record(&h)).unwrap();
        let db = store.db_path();
        let reference = Store::open_reference_file(&db).unwrap();

        // Reads work on the reference handle. Writes are impossible by
        // construction: ReferenceStore has no set_categories/insert_file/remove
        // methods, so a Sweep caller cannot even name one (this fails to
        // compile if a write method is ever added to the read surface).
        let read_all = |reference: &ReferenceStore| {
            assert_eq!(reference.categories().unwrap().len(), 3);
            assert!(reference.contains(&h).unwrap());
            let rec = reference.lookup(&h).unwrap().unwrap();
            assert_eq!(rec.category_folder, "keep");
            assert_eq!(reference.origin(&h).unwrap().as_deref(), Some("keep"));
            assert_eq!(reference.list_files().unwrap().len(), 1);
            assert_eq!(reference.all_hashes().unwrap().len(), 1);
        };
        read_all(&reference);

        // No rows gained or lost, and no real files created beside the
        // reference database. (WAL `-shm`/`-wal` index files may materialize
        // on reads — SQLite reader behavior, not content — so only
        // non-sidecar entries are compared.)
        let is_sidecar = |name: &str| {
            name == "organizer.db-wal"
                || name == "organizer.db-shm"
                || name == "organizer.db-journal"
        };
        let mut real_files: Vec<String> = fs::read_dir(&source)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !is_sidecar(n))
            .collect();
        real_files.sort();
        assert_eq!(real_files, vec!["organizer.db".to_string()]);
        read_all(&reference);
        assert_eq!(Store::open(&source).unwrap().all_hashes().unwrap().len(), 1);
        let mut real_after: Vec<String> = fs::read_dir(&source)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !is_sidecar(n))
            .collect();
        real_after.sort();
        assert_eq!(real_after, real_files);
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
                    let hash = FileHash::new(&h).unwrap();
                    let rec = FileRecord::new(&hash, "a.jpg", "keep/a.jpg", "keep", 1, 1, 1).unwrap();
                    // No caller retry: Store retries transient BUSY internally.
                    store.insert_file(&rec).unwrap();
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
        // No caller retry: Store::open retries transient BUSY internally.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = source.clone();
            handles.push(std::thread::spawn(move || {
                Store::open(&path).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let store = Store::open(&source).unwrap();
        assert_eq!(store.categories().unwrap().len(), 3);
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

    #[test]
    fn fresh_schema_enforces_lowercase_hash_defense_in_depth() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        Store::open(&source).unwrap();
        let conn = Connection::open(db_path_for(&source)).unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'files'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("lower(hash)"),
            "files table must carry a lower-case CHECK, got: {sql}"
        );
        // Raw upper-case SQL bypassing FileRecord::new must fail at the DB level.
        let upper = test_hash(0xAB).to_ascii_uppercase();
        let res = conn.execute(
            "INSERT INTO files (hash, original_rel, final_rel, category_folder, size, mtime, triaged_at) VALUES (?1, 'a.jpg', 'keep/a.jpg', 'keep', 1, 1, 1)",
            rusqlite::params![upper],
        );
        assert!(res.is_err(), "upper-case hash must violate CHECK");
    }

    #[test]
    fn invalid_reference_file_errors_clearly_without_hinting_missing() {
        let dir = TempDir::new().unwrap();
        // Empty file: valid SQLite container, but no organizer schema.
        let empty = dir.path().join("empty.db");
        fs::write(&empty, b"").unwrap();
        // Foreign content: not a database at all.
        let foreign = dir.path().join("foreign.db");
        fs::write(&foreign, b"definitely not sqlite").unwrap();
        for bad in [&empty, &foreign] {
            match Store::open_reference_file(bad) {
                Err(StoreError::InvalidDatabase { path, reason }) => {
                    assert_eq!(path, bad.clone());
                    assert!(!reason.is_empty(), "reason must name the cause");
                }
                other => panic!("expected InvalidDatabase for {}, got {other:?}", bad.display()),
            }
        }
    }

    #[test]
    fn unsupported_schema_version_errors_clearly() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        Store::open(&source).unwrap();
        // Simulate a newer schema by bumping the version row.
        let conn = Connection::open(db_path_for(&source)).unwrap();
        conn.execute(
            "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        drop(conn);
        match Store::open(&source) {
            Err(StoreError::UnsupportedSchema { found }) => assert_eq!(found, "999"),
            other => panic!("expected UnsupportedSchema, got {other:?}"),
        }
    }

    #[test]
    fn store_level_rollback_on_validation_failure_leaves_db_unchanged() {
        // Store-seam half of "transactional rollback on move failure": a rejected
        // write must not leave a partial row. The filesystem half (rename + row
        // insert + row revert) belongs to Classification (#24).
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("source");
        let store = Store::open(&source).unwrap();
        let before_categories = store.categories().unwrap();
        let before_hashes = store.all_hashes().unwrap();
        // Invalid categories + invalid hash both fail before touching the DB.
        assert!(store
            .set_categories(&[Category {
                display_name: "Dup".into(),
                folder_name: "duplicate".into(),
                shortcut: "1".into(),
            }])
            .is_err());
        assert!(FileHash::new("bad").is_err());
        assert_eq!(store.categories().unwrap(), before_categories);
        assert_eq!(store.all_hashes().unwrap(), before_hashes);
    }
}
