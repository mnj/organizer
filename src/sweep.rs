use std::collections::HashSet;
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

/// Normalize raw `--include` values: split commas, trim, lower-case, drop empties.
/// Repeatable (`--include keep --include maybe`) and comma-separated
/// (`--include keep,maybe`) both feed the same filter. Empty input yields an
/// empty set; callers treat absent `--include` as `None` (all Actions).
pub fn parse_include_args(raw: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    for entry in raw {
        for part in entry.split(',') {
            let token = part.trim().to_ascii_lowercase();
            if !token.is_empty() {
                out.insert(token);
            }
        }
    }
    out
}

/// Resolve normalized include tokens against known Actions to folder allow-list.
/// Each token matching a folder or display name (case-insensitive) maps to its
/// folder lower-cased; unknown tokens are kept as-is so they match nothing
/// rather than silently widening the filter.
pub fn resolve_include_filter(raw: &[String], actions: &[crate::config::Action]) -> HashSet<String> {
    let tokens = parse_include_args(raw);
    let mut out = HashSet::new();
    for token in tokens {
        let mut mapped: Option<String> = None;
        for a in actions {
            if a.folder_name.to_ascii_lowercase() == token
                || a.display_name.to_ascii_lowercase() == token
            {
                mapped = Some(a.folder_name.to_ascii_lowercase());
                break;
            }
        }
        out.insert(mapped.unwrap_or(token));
    }
    out
}

/// Human-readable numbered list of database Actions for the interactive toggle
/// prompt (#27). Sourced from the reference Organizer Database via
/// `Store::actions()`, so CLI selection stays discoverable without editing the
/// database. Order follows stored position.
pub fn format_toggle_list(actions: &[crate::config::Action]) -> String {
    let mut out = String::new();
    for (i, a) in actions.iter().enumerate() {
        out.push_str(&format!("  {} [x] {} ({})\n", i + 1, a.display_name, a.folder_name));
    }
    out
}

/// Parse an interactive toggle line into an include allow-list.
/// - Empty or `all` (case-insensitive) yields `None` (default: all Actions).
/// - `none` yields `Some(empty)` (match nothing).
/// - Otherwise comma/whitespace-separated numbers (1-based position) or
///   folder/display names (case-insensitive) select those Actions; unknown
///   tokens are ignored so a typo narrows rather than widens.
/// Returns folder lower-cased allow-list for `run_sweep_report_with_filter`.
pub fn parse_toggle_selection(
    input: &str,
    actions: &[crate::config::Action],
) -> Option<HashSet<String>> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
        return None;
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Some(HashSet::new());
    }
    let mut out = HashSet::new();
    // Split on commas and whitespace: "1, 3" and "keep maybe" both work.
    let normalized = trimmed.replace(',', " ");
    for token in normalized.split_whitespace() {
        let lower = token.trim().to_ascii_lowercase();
        if lower.is_empty() {
            continue;
        }
        // Numeric position first.
        if let Ok(n) = lower.parse::<usize>() {
            if n >= 1 && n <= actions.len() {
                out.insert(actions[n - 1].folder_name.to_ascii_lowercase());
            }
            continue;
        }
        for a in actions {
            if a.folder_name.to_ascii_lowercase() == lower
                || a.display_name.to_ascii_lowercase() == lower
            {
                out.insert(a.folder_name.to_ascii_lowercase());
                break;
            }
        }
    }
    Some(out)
}

/// Line-prompt fallback for the include filter: prints database Actions and
/// reads one stdin line, feeding the same include filter as `--include`.
/// Returns `None` for all. The primary interactive screen is the ratatui
/// fullscreen checklist (`sweep_tui::run_include_tui`); pure parsing lives in
/// `parse_toggle_selection` so behavior is testable without stdin.
pub fn prompt_include_filter(actions: &[crate::config::Action]) -> Option<HashSet<String>> {
    use std::io::{self, Write};

    eprintln!("Reference Actions (default all):");
    eprint!("{}", format_toggle_list(actions));
    eprintln!("Enter numbers/names to include (comma-separated), 'all', 'none', or empty for all:");
    let _ = io::stderr().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(_) => parse_toggle_selection(&line, actions),
        Err(_) => None,
    }
}

/// Read-only Sweep: scan Target Folder as a flat Supported-Format Snapshot,
/// hash each entry, match sha256 against the reference Organizer Database union.
/// Reports matched path, hash, origin Action and triage time without modifying
/// files or the database. Unmatched files are untouched and unlisted.
pub fn run_sweep_report(target: &Path, db_path: &Path) -> Result<SweepReport, SweepError> {
    run_sweep_report_with_filter(target, db_path, None)
}

/// Read-only Sweep with an origin-Action include filter (#27).
/// - `None` matches hashes from all origin Actions (default).
/// - `Some(set)` matches only hashes whose origin Action folder (case-insensitive)
///   is in `set`; other known hashes are left alone and unreported.
/// Unknown-hash files remain untouched and unlisted regardless of filter.
pub fn run_sweep_report_with_filter(
    target: &Path,
    db_path: &Path,
    include: Option<&HashSet<String>>,
) -> Result<SweepReport, SweepError> {
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
    // Normalized lower-case allow-list; None means all origin Actions.
    let allowed: Option<HashSet<String>> = include.map(|set| {
        set.iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let mut matches = Vec::new();
    for path in snapshot {
        let hash = compute_sha256(&path)
            .map(|h| h.to_ascii_lowercase())
            .map_err(|e| SweepError::HashFailed(path.clone(), e.to_string()))?;
        let rec = store
            .lookup(&hash)
            .map_err(|e| SweepError::Database(e.to_string()))?;
        if let Some(rec) = rec {
            if let Some(ref allow) = allowed {
                if !allow.contains(&rec.action_folder.to_ascii_lowercase()) {
                    continue;
                }
            }
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

    #[test]
    fn filtered_run_matches_only_included_actions_and_leaves_others_unreported() {
        use std::collections::HashSet;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let keep_file = write_target(&target, "keep.jpg", b"keep content");
        let maybe_file = write_target(&target, "maybe.jpg", b"maybe content");
        let unknown = write_target(&target, "unknown.jpg", b"brand new");
        insert_known(&store, &keep_file, "keep.jpg", "keep", 11);
        insert_known(&store, &maybe_file, "maybe.jpg", "maybe", 12);

        let mut only_keep = HashSet::new();
        only_keep.insert("keep".to_string());
        let report =
            run_sweep_report_with_filter(&target, &store.db_path(), Some(&only_keep)).unwrap();
        assert_eq!(report.scanned, 3);
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].path, keep_file);
        assert_eq!(report.matches[0].action, "keep");

        // Excluded origin left alone and unreported; unknown untouched.
        assert!(maybe_file.exists());
        assert!(unknown.exists());
        assert_eq!(fs::read(&maybe_file).unwrap(), b"maybe content");
        assert!(!report.matches.iter().any(|m| m.path == maybe_file));
        assert!(!report.matches.iter().any(|m| m.path == unknown));
    }

    #[test]
    fn include_args_split_commas_trim_and_resolve_display_alias_case_insensitive() {
        use crate::config::Action;

        let actions = vec![
            Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Action { display_name: "Top Picks".into(), folder_name: "top_picks".into(), shortcut: "2".into() },
        ];

        // Repeatable + comma-separated, trimmed, lower-cased, empties dropped.
        let raw = vec!["keep, KEEP ".to_string(), "  ".to_string(), "Top Picks".to_string()];
        let tokens = parse_include_args(&raw);
        assert!(tokens.contains("keep"));
        assert!(tokens.contains("top picks"));
        assert_eq!(tokens.len(), 2);

        // Display-name alias resolves to folder; unknown stays as-is (matches nothing).
        let resolved = resolve_include_filter(&["KEEP".to_string()], &actions);
        assert!(resolved.contains("keep"));
        let resolved_display = resolve_include_filter(&["top picks".to_string()], &actions);
        assert!(resolved_display.contains("top_picks"));
        let resolved_unknown = resolve_include_filter(&["nope".to_string()], &actions);
        assert!(resolved_unknown.contains("nope"));
    }

    #[test]
    fn toggle_selection_parses_numbers_names_all_none_and_lists_actions() {
        use crate::config::Action;

        let actions = vec![
            Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Action { display_name: "Maybe".into(), folder_name: "maybe".into(), shortcut: "2".into() },
            Action { display_name: "Reject".into(), folder_name: "reject".into(), shortcut: "3".into() },
        ];

        // Toggle list is sourced from database Actions.
        let list = format_toggle_list(&actions);
        assert!(list.contains("Keep"));
        assert!(list.contains("keep"));
        assert!(list.contains("Maybe"));

        // Empty / all means default-all (None).
        assert_eq!(parse_toggle_selection("", &actions), None);
        assert_eq!(parse_toggle_selection("all", &actions), None);
        assert_eq!(parse_toggle_selection("ALL", &actions), None);

        // None means match nothing.
        assert_eq!(
            parse_toggle_selection("none", &actions),
            Some(std::collections::HashSet::new())
        );

        // Numbers select by position.
        let sel = parse_toggle_selection("1,3", &actions).unwrap();
        assert!(sel.contains("keep"));
        assert!(sel.contains("reject"));
        assert!(!sel.contains("maybe"));

        // Names select case-insensitively, folder or display.
        let sel = parse_toggle_selection("KEEP, maybe", &actions).unwrap();
        assert!(sel.contains("keep"));
        assert!(sel.contains("maybe"));
    }

    #[test]
    fn default_filter_matches_all_and_unknown_untouched_under_any_filter() {
        use std::collections::HashSet;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let keep_file = write_target(&target, "keep.jpg", b"keep bytes");
        let maybe_file = write_target(&target, "maybe.jpg", b"maybe bytes");
        let unknown = write_target(&target, "unknown.jpg", b"new bytes");
        insert_known(&store, &keep_file, "keep.jpg", "keep", 21);
        insert_known(&store, &maybe_file, "maybe.jpg", "maybe", 22);
        let unknown_hash = compute_sha256(&unknown).unwrap().to_ascii_lowercase();

        // Default (None) matches hashes from all origin Actions.
        let all = run_sweep_report_with_filter(&target, &store.db_path(), None).unwrap();
        assert_eq!(all.scanned, 3);
        assert_eq!(all.matches.len(), 2);

        // Filtered or not, unknown-hash files remain untouched and unlisted.
        for filter in [
            None,
            Some(HashSet::from(["keep".to_string()])),
            Some(HashSet::from(["maybe".to_string()])),
            Some(HashSet::new()),
        ] {
            let report =
                run_sweep_report_with_filter(&target, &store.db_path(), filter.as_ref()).unwrap();
            assert!(
                !report.matches.iter().any(|m| m.path == unknown),
                "unknown must stay unlisted under filter {filter:?}"
            );
            assert!(
                !store.contains(&unknown_hash).unwrap(),
                "unknown must not gain a database row"
            );
            assert!(unknown.exists());
            assert_eq!(fs::read(&unknown).unwrap(), b"new bytes");
        }
    }

    #[test]
    fn toggle_selection_feeds_same_filter_as_include_flag() {
        use crate::config::Action;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();
        // Custom Actions prove toggles are sourced from the database, not defaults.
        let custom = vec![
            Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Action { display_name: "Archive".into(), folder_name: "archive".into(), shortcut: "2".into() },
        ];
        store.set_actions(&custom).unwrap();
        let db_actions = store.actions().unwrap();
        assert_eq!(db_actions, custom);

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let keep_file = write_target(&target, "keep.jpg", b"keep v");
        let arch_file = write_target(&target, "arch.jpg", b"arch v");
        insert_known(&store, &keep_file, "keep.jpg", "keep", 31);
        insert_known(&store, &arch_file, "arch.jpg", "archive", 32);

        // Same selection via number toggle and via --include flag value.
        let via_toggle = parse_toggle_selection("1", &db_actions).unwrap();
        let via_flag = resolve_include_filter(&["keep".to_string()], &db_actions);
        assert_eq!(via_toggle, via_flag);

        let from_toggle =
            run_sweep_report_with_filter(&target, &store.db_path(), Some(&via_toggle)).unwrap();
        let from_flag =
            run_sweep_report_with_filter(&target, &store.db_path(), Some(&via_flag)).unwrap();
        assert_eq!(from_toggle.matches, from_flag.matches);
        assert_eq!(from_toggle.matches.len(), 1);
        assert_eq!(from_toggle.matches[0].action, "keep");
    }
}
