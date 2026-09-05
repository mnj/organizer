use std::path::{Path, PathBuf};

use crate::dedup::compute_sha256;
use crate::queue::build_snapshot;
use crate::store::{missing_reference_message, Store, StoreError, DB_FILENAME};

/// Output format for Sweep report (#26): human table alongside machine-readable json.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

/// One matched Target Folder file: content already known to the reference database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepMatch {
    pub path: PathBuf,
    pub hash: String,
    pub action: String,
    pub triaged_at: i64,
}

/// Read-only Sweep report: matched files plus scan counts. Never modifies files or database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub target: PathBuf,
    pub reference_db: PathBuf,
    pub scanned: usize,
    pub matches: Vec<SweepMatch>,
}

#[derive(Debug)]
pub enum SweepError {
    TargetNotDir(PathBuf),
    TargetUnreadable(PathBuf, String),
    MissingDatabase(PathBuf),
    Database(String),
    HashFailed(PathBuf, String),
}

impl std::fmt::Display for SweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SweepError::TargetNotDir(p) => {
                write!(f, "target folder not a directory: {}", p.display())
            }
            SweepError::TargetUnreadable(p, e) => {
                write!(f, "failed to scan target folder {}: {}", p.display(), e)
            }
            SweepError::MissingDatabase(p) => write!(f, "{}", missing_reference_message(p)),
            SweepError::Database(e) => write!(f, "{e}"),
            SweepError::HashFailed(p, e) => {
                write!(f, "failed to hash {}: {}", p.display(), e)
            }
        }
    }
}

impl std::error::Error for SweepError {}

/// Resolve the reference database file path (distinct from
/// `store::db_path_for`, which derives `<Source Folder>/organizer.db` for
/// Classification: this resolves the Sweep `--db` flag value instead).
/// - None defaults to `./organizer.db` in the current folder.
/// - An existing directory resolves to `<dir>/organizer.db` (so a Source
///   Folder can be passed directly).
/// - Anything else is used as-is, so an explicit file path — with or without
///   an extension — is never rewritten.
pub fn resolve_reference_db_path(db_arg: Option<&Path>) -> PathBuf {
    match db_arg {
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(DB_FILENAME),
        Some(p) => {
            if p.is_dir() {
                return p.join(DB_FILENAME);
            }
            p.to_path_buf()
        }
    }
}

/// Read-only Sweep: scan Target Folder as a flat Supported-Format Snapshot,
/// hash each entry, match sha256 against the reference Organizer Database union.
/// Reports matched path, hash, origin Action and triage time without modifying
/// files or the database. Unmatched files are untouched and unlisted.
pub fn run_sweep_report(target: &Path, db_path: &Path) -> Result<SweepReport, SweepError> {
    if !target.is_dir() {
        return Err(SweepError::TargetNotDir(target.to_path_buf()));
    }
    let store = Store::open_reference_file(db_path).map_err(|e| match &e {
        StoreError::Io(ioe) if ioe.kind() == std::io::ErrorKind::NotFound => {
            SweepError::MissingDatabase(db_path.to_path_buf())
        }
        other => SweepError::Database(other.to_string()),
    })?;
    let snapshot =
        build_snapshot(target).map_err(|e| SweepError::TargetUnreadable(target.to_path_buf(), e.to_string()))?;
    let scanned = snapshot.len();
    let mut matches = Vec::new();
    for path in snapshot {
        let hash = compute_sha256(&path)
            .map(|h| h.to_ascii_lowercase())
            .map_err(|e| SweepError::HashFailed(path.clone(), e.to_string()))?;
        let rec = store
            .lookup(&hash)
            .map_err(|e| SweepError::Database(e.to_string()))?;
        if let Some(rec) = rec {
            matches.push(SweepMatch {
                path,
                hash,
                action: rec.action_folder,
                triaged_at: rec.triaged_at,
            });
        }
    }
    Ok(SweepReport {
        target: target.to_path_buf(),
        reference_db: db_path.to_path_buf(),
        scanned,
        matches,
    })
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn format_table(report: &SweepReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Matched {} of {} files (target: {}, reference: {})\n",
        report.matches.len(),
        report.scanned,
        report.target.display(),
        report.reference_db.display()
    ));
    if report.matches.is_empty() {
        out.push_str("No Duplicates found — unknown files untouched and unlisted.\n");
    } else {
        out.push_str("path\thash\taction\ttriaged_at\n");
        for m in &report.matches {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                m.path.display(),
                m.hash,
                m.action,
                m.triaged_at
            ));
        }
    }
    out
}

fn format_json(report: &SweepReport) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"target\": \"{}\",\n",
        json_escape(&report.target.to_string_lossy())
    ));
    out.push_str(&format!(
        "  \"reference\": \"{}\",\n",
        json_escape(&report.reference_db.to_string_lossy())
    ));
    out.push_str(&format!("  \"scanned\": {},\n", report.scanned));
    out.push_str(&format!("  \"matched\": {},\n", report.matches.len()));
    out.push_str("  \"matches\": [");
    if report.matches.is_empty() {
        out.push_str("]\n");
    } else {
        out.push('\n');
        for (i, m) in report.matches.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!(
                "      \"path\": \"{}\",\n",
                json_escape(&m.path.to_string_lossy())
            ));
            out.push_str(&format!("      \"hash\": \"{}\",\n", json_escape(&m.hash)));
            out.push_str(&format!("      \"action\": \"{}\",\n", json_escape(&m.action)));
            out.push_str(&format!("      \"triaged_at\": {}\n", m.triaged_at));
            if i + 1 == report.matches.len() {
                out.push_str("    }\n");
            } else {
                out.push_str("    },\n");
            }
        }
        out.push_str("  ]\n");
    }
    out.push_str("}\n");
    out
}

/// Format a Sweep report as human table or machine-readable json.
pub fn format_report(report: &SweepReport, format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => format_table(report),
        OutputFormat::Json => format_json(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileRecord;
    use std::fs;
    use tempfile::TempDir;

    fn write_target(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    /// Hash `file` and record it in the reference database as triaged to
    /// `action` at `triaged_at`. Returns the lower-case sha256.
    fn insert_known(
        store: &Store,
        file: &Path,
        name: &str,
        action: &str,
        triaged_at: i64,
    ) -> String {
        let hash = compute_sha256(file).unwrap().to_ascii_lowercase();
        let size = fs::metadata(file).unwrap().len() as i64;
        store
            .insert_file(
                &FileRecord::new(&hash, name, &format!("{action}/{name}"), action, size, 0, triaged_at)
                    .unwrap(),
            )
            .unwrap();
        hash
    }

    #[test]
    fn report_lists_matched_with_path_hash_action_time_and_leaves_files() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let known = write_target(&target, "known.jpg", b"known content");
        let unknown = write_target(&target, "unknown.jpg", b"brand new content");
        let triaged_at = 1_700_000_000i64;
        let known_hash = insert_known(&store, &known, "known.jpg", "keep", triaged_at);

        let report = run_sweep_report(&target, &store.db_path()).unwrap();
        assert_eq!(report.scanned, 2);
        assert_eq!(report.matches.len(), 1);
        let m = &report.matches[0];
        assert_eq!(m.path, known);
        assert_eq!(m.hash, known_hash);
        assert_eq!(m.action, "keep");
        assert_eq!(m.triaged_at, triaged_at);

        // Read-only: files untouched, database has no extra rows.
        assert!(known.exists());
        assert!(unknown.exists());
        assert_eq!(fs::read(&known).unwrap(), b"known content");
        assert_eq!(store.all_hashes().unwrap().len(), 1);
        assert!(!store.contains(&compute_sha256(&unknown).unwrap()).unwrap());
    }

    #[test]
    fn target_scan_is_flat_supported_only_alphabetical_and_excludes_db() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        // Supported top-level (mixed case to prove case-insensitive).
        for (name, content) in [
            ("b.JPG", b"b content".as_slice()),
            ("a.png", b"a content".as_slice()),
            ("10.jpg", b"ten".as_slice()),
            ("2.jpg", b"two".as_slice()),
            ("clip.webm", b"video".as_slice()),
        ] {
            let p = write_target(&target, name, content);
            insert_known(&store, &p, name, "keep", 7);
        }
        // Unsupported + legacy + database artifacts must be excluded.
        for name in [
            "notes.pdf",
            "archive.zip",
            "keep.txt",
            "organizer.toml",
            "organizer.db",
            "organizer.db-wal",
            "organizer.db-shm",
            "organizer.db-journal",
        ] {
            write_target(&target, name, b"ignored");
        }
        // Subdirectory must be ignored (flat only).
        fs::create_dir_all(target.join("subdir")).unwrap();
        write_target(&target.join("subdir"), "inside.jpg", b"nested");

        let snapshot = build_snapshot(&target).unwrap();
        let report = run_sweep_report(&target, &store.db_path()).unwrap();
        assert_eq!(report.scanned, snapshot.len());
        assert_eq!(report.scanned, 5);
        let matched_paths: Vec<PathBuf> = report.matches.iter().map(|m| m.path.clone()).collect();
        assert_eq!(matched_paths, snapshot, "matches must follow alphabetical Snapshot order");
        for m in &report.matches {
            assert!(!m.path.ends_with("notes.pdf"));
            assert!(!m.path.ends_with("organizer.db"));
        }
    }

    #[test]
    fn missing_reference_database_errors_clearly_and_creates_nothing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        write_target(&target, "a.jpg", b"data");
        let missing = tmp.path().join("nope").join("organizer.db");

        let err = run_sweep_report(&target, &missing).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&missing.display().to_string()),
            "error must name the missing path, got: {msg}"
        );
        assert!(
            msg.contains("--db"),
            "error must hint the override flag, got: {msg}"
        );
        assert!(!missing.exists(), "Sweep must not create a missing reference database");
        assert!(target.join("a.jpg").exists(), "target must be untouched");
    }

    #[test]
    fn machine_readable_json_works_alongside_human_table() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let known = write_target(&target, "known.jpg", b"known bytes");
        write_target(&target, "unknown.jpg", b"unknown bytes");
        let known_hash = insert_known(&store, &known, "known.jpg", "keep", 42);

        let report = run_sweep_report(&target, &store.db_path()).unwrap();
        let table = format_report(&report, OutputFormat::Table);
        let json = format_report(&report, OutputFormat::Json);

        // Human table lists the match, omits the unknown.
        assert!(table.contains("known.jpg"), "table must list matched path:\n{table}");
        assert!(table.contains(&known_hash), "table must list hash:\n{table}");
        assert!(table.contains("keep"), "table must list origin Action:\n{table}");
        assert!(table.contains("42"), "table must list triage time:\n{table}");
        assert!(!table.contains("unknown.jpg"), "table must not list unmatched:\n{table}");

        // Machine-readable json carries the same match.
        assert!(json.contains("\"matches\""), "json must carry matches:\n{json}");
        assert!(json.contains("known.jpg"), "json must list matched path:\n{json}");
        assert!(json.contains(&known_hash), "json must list hash:\n{json}");
        assert!(json.contains("keep"), "json must list origin Action:\n{json}");
        assert!(json.contains("42"), "json must list triage time:\n{json}");
        assert!(!json.contains("unknown.jpg"), "json must not list unmatched:\n{json}");
        assert!(json.contains("\"scanned\""), "json must carry scan counts:\n{json}");
        assert!(json.contains("\"matched\""), "json must carry match counts:\n{json}");
    }

    #[test]
    fn resolve_reference_db_path_defaults_to_current_and_handles_dir_vs_file() {
        let tmp = TempDir::new().unwrap();
        // Default resolves to current folder's organizer.db.
        let cur = std::env::current_dir().unwrap().join(DB_FILENAME);
        assert_eq!(resolve_reference_db_path(None), cur);

        // Existing dir resolves inside it.
        let dir = tmp.path().join("src");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(resolve_reference_db_path(Some(&dir)), dir.join(DB_FILENAME));

        // Explicit .db file is used as-is.
        let file = tmp.path().join("custom").join("organizer.db");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"").unwrap();
        assert_eq!(resolve_reference_db_path(Some(&file)), file);

        // A non-directory path is used as-is even without an extension, so
        // explicit file overrides are never rewritten to `<path>/organizer.db`.
        let explicit = tmp.path().join("myref");
        assert_eq!(resolve_reference_db_path(Some(&explicit)), explicit);
    }

    #[test]
    fn sweep_is_read_only_on_reference_and_target() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let known = write_target(&target, "known.jpg", b"dup bytes");
        insert_known(&store, &known, "known.jpg", "keep", 9);
        let before_hashes = store.all_hashes().unwrap();
        let before_actions = store.actions().unwrap();
        let before_bytes = fs::read(&known).unwrap();

        // Warm-up run: SQLite WAL readers materialize the -shm/-wal index on
        // first access even when read-only, so snapshot the file baseline after
        // one Sweep. Everything asserted below is steady-state.
        let warmup = run_sweep_report(&target, &store.db_path()).unwrap();
        assert_eq!(warmup.matches.len(), 1);
        let mut before_files: Vec<String> = fs::read_dir(&source)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        before_files.sort();

        // Filesystem-level read-only: with every reference file chmodded
        // 0o444, Sweep must still succeed — it performs no row writes, and
        // any write attempt would fail against these permissions.
        for e in fs::read_dir(&source).unwrap().flatten() {
            let mut perms = e.metadata().unwrap().permissions();
            perms.set_mode(0o444);
            fs::set_permissions(e.path(), perms).unwrap();
        }

        let report = run_sweep_report(&target, &store.db_path()).unwrap();
        assert_eq!(report.matches.len(), 1);

        // No row growth, no action changes, no file moves, no duplicate/ subfolder.
        let reference = Store::open_reference_file(&store.db_path()).unwrap();
        assert_eq!(reference.all_hashes().unwrap(), before_hashes);
        assert_eq!(reference.actions().unwrap(), before_actions);
        assert_eq!(fs::read(&known).unwrap(), before_bytes);
        assert!(!target.join("duplicate").exists());
        assert!(!source.join("duplicate").exists() || source.join("duplicate").read_dir().map(|mut d| d.next().is_none()).unwrap_or(true));
        // No sidecar files created beside the reference database.
        let mut after_files: Vec<String> = fs::read_dir(&source)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        after_files.sort();
        assert_eq!(after_files, before_files, "Sweep must not create files beside the reference database");
    }
}
