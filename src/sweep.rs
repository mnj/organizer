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
    pub category: String,
    pub triaged_at: i64,
}

/// One per-file failure: hash during scan, or apply (trash/perm-delete).
/// The run continues with remaining files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyError {
    pub path: PathBuf,
    pub message: String,
}

/// Read-only Sweep report: matched files plus scan counts. Never modifies files or database.
/// Unreadable files are recorded in `hash_errors` and skipped; other files still match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub target: PathBuf,
    pub reference_db: PathBuf,
    pub scanned: usize,
    pub matches: Vec<SweepMatch>,
    pub hash_errors: Vec<ApplyError>,
}

#[derive(Debug)]
pub enum SweepError {
    TargetNotDir(PathBuf),
    TargetUnreadable(PathBuf, String),
    MissingDatabase(PathBuf),
    Database(String),
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

/// Normalize raw `--categories` values: split commas, trim, lower-case, drop empties.
/// Repeatable (`--categories keep --categories maybe`) and comma-separated
/// (`--categories keep,maybe`) both feed the same filter. Empty input yields an
/// empty set; callers treat absent `--categories` as `None` (all Categories).
pub fn parse_category_args(raw: &[String]) -> HashSet<String> {
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

/// True when `token` (already lower-cased) names this Category by folder or
/// display name. Single shared predicate for the flag and toggle paths so
/// both filters agree on what a token selects.
fn category_matches_token(c: &crate::config::Category, token: &str) -> bool {
    c.folder_name.to_ascii_lowercase() == token || c.display_name.to_ascii_lowercase() == token
}

/// Resolve normalized category tokens against known Categories to folder allow-list.
/// Each token matching a folder or display name (case-insensitive) maps to its
/// folder lower-cased; unknown tokens are kept as-is so they match nothing
/// rather than silently widening the filter.
pub fn resolve_category_filter(raw: &[String], categories: &[crate::config::Category]) -> HashSet<String> {
    let tokens = parse_category_args(raw);
    let mut out = HashSet::new();
    for token in tokens {
        let mut mapped: Option<String> = None;
        for c in categories {
            if category_matches_token(c, &token) {
                mapped = Some(c.folder_name.to_ascii_lowercase());
                break;
            }
        }
        out.insert(mapped.unwrap_or(token));
    }
    out
}

/// CLI `--categories` resolution: absent or empty selection means default-all
/// (`None`); otherwise the resolved folder allow-list. An explicit selection
/// that resolves to nothing (e.g. `--categories ""`) also means all, so only
/// a non-empty selection narrows the Sweep.
pub fn resolve_cli_category_filter(
    raw: &[String],
    categories: &[crate::config::Category],
) -> Option<HashSet<String>> {
    if raw.is_empty() {
        return None;
    }
    let resolved = resolve_category_filter(raw, categories);
    if resolved.is_empty() {
        None
    } else {
        Some(resolved)
    }
}

/// Human-readable numbered list of database Categories for the interactive toggle
/// prompt (#27). Sourced from the reference Organizer Database via
/// `Store::categories()`, so CLI selection stays discoverable without editing the
/// database. Order follows stored position.
pub fn format_toggle_list(categories: &[crate::config::Category]) -> String {
    let mut out = String::new();
    for (i, c) in categories.iter().enumerate() {
        out.push_str(&format!("  {} [x] {} ({})\n", i + 1, c.display_name, c.folder_name));
    }
    out
}

/// Parse an interactive toggle line into a category allow-list.
/// - Empty or `all` (case-insensitive) yields `None` (default: all Categories).
/// - `none` yields `Some(empty)` (match nothing).
/// - Otherwise comma/whitespace-separated numbers (1-based position) or
///   folder/display names (case-insensitive) select those Categories.
///   Unknown names are kept as-is so they match nothing — identical to the
///   `--categories` flag (a typo narrows instead of widening).
/// Returns folder lower-cased allow-list for `run_sweep_report_with_filter`.
pub fn parse_toggle_selection(
    input: &str,
    categories: &[crate::config::Category],
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
            if n >= 1 && n <= categories.len() {
                out.insert(categories[n - 1].folder_name.to_ascii_lowercase());
            }
            continue;
        }
        let mut matched = false;
        for c in categories {
            if category_matches_token(c, &lower) {
                out.insert(c.folder_name.to_ascii_lowercase());
                matched = true;
                break;
            }
        }
        if !matched {
            out.insert(lower);
        }
    }
    Some(out)
}

/// Line-prompt fallback for the category filter: prints database Categories and
/// reads one stdin line, feeding the same category filter as `--categories`.
/// Returns `None` for all. The primary interactive screen is the ratatui
/// fullscreen checklist (`sweep_tui::run_categories_tui`); pure parsing lives in
/// `parse_toggle_selection` so behavior is testable without stdin.
pub fn prompt_category_filter(categories: &[crate::config::Category]) -> Option<HashSet<String>> {
    use std::io::{self, Write};

    eprintln!("Reference Categories (default all):");
    eprint!("{}", format_toggle_list(categories));
    eprintln!("Enter numbers/names to match (comma-separated), 'all', 'none', or empty for all:");
    let _ = io::stderr().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(_) => parse_toggle_selection(&line, categories),
        Err(_) => None,
    }
}

/// Read-only Sweep: scan Target Folder as a flat Supported-Format Snapshot,
/// hash each entry, match sha256 against the reference Organizer Database union.
/// Reports matched path, hash, origin Category and triage time without modifying
/// files or the database. Unmatched files are untouched and unlisted.
pub fn run_sweep_report(target: &Path, db_path: &Path) -> Result<SweepReport, SweepError> {
    run_sweep_report_with_filter(target, db_path, None)
}

/// Read-only Sweep with an origin-Category filter (#27).
/// - `None` matches hashes from all origin Categories (default).
/// - `Some(set)` matches only hashes whose origin Category folder (case-insensitive)
///   is in `set`; other known hashes are left alone and unreported.
/// Unknown-hash files remain untouched and unlisted regardless of filter.
pub fn run_sweep_report_with_filter(
    target: &Path,
    db_path: &Path,
    categories: Option<&HashSet<String>>,
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
    // Normalized lower-case allow-list; None means all origin Categories.
    let allowed: Option<HashSet<String>> = categories.map(|set| {
        set.iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    });
    let mut matches = Vec::new();
    let mut hash_errors = Vec::new();
    for path in snapshot {
        let hash = match compute_sha256(&path).map(|h| h.to_ascii_lowercase()) {
            Ok(h) => h,
            Err(e) => {
                hash_errors.push(ApplyError {
                    path,
                    message: e.to_string(),
                });
                continue;
            }
        };
        let rec = store
            .lookup(&hash)
            .map_err(|e| SweepError::Database(e.to_string()))?;
        if let Some(rec) = rec {
            if let Some(ref allow) = allowed {
                if !allow.contains(&rec.category_folder.to_ascii_lowercase()) {
                    continue;
                }
            }
            matches.push(SweepMatch {
                path,
                hash,
                category: rec.category_folder,
                triaged_at: rec.triaged_at,
            });
        }
    }
    Ok(SweepReport {
        target: target.to_path_buf(),
        reference_db: db_path.to_path_buf(),
        scanned,
        matches,
        hash_errors,
    })
}

/// Sweep Action for apply (#28): `report` lists only, restorable OS trash and
/// irreversible permanent delete are gated behind an explicit execute flag.
/// `Report` (default) never touches the filesystem beyond hashing/reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Action {
    Report,
    Trash,
    PermDelete,
}

/// Outcome of a Sweep run with an Action: the read-only match report plus
/// what the execute gate did. `executed` is true only when a destructive
/// Action ran with `execute == true`; otherwise `removed`/`errors` are
/// empty and every Target Folder file is untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepOutcome {
    pub report: SweepReport,
    pub action: Action,
    pub executed: bool,
    pub removed: Vec<PathBuf>,
    pub errors: Vec<ApplyError>,
}

impl SweepOutcome {
    /// True when a destructive Action was selected but not executed:
    /// the report lists what `--execute` would apply.
    pub fn is_dry_run(&self) -> bool {
        !self.executed && self.action != Action::Report
    }

    /// Per-file failures from scan (hash) plus apply (trash/perm-delete).
    pub fn file_error_count(&self) -> usize {
        self.errors.len() + self.report.hash_errors.len()
    }
}

impl Action {
    /// Machine/CLI spelling: `report`, `trash`, `perm-delete`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Report => "report",
            Action::Trash => "trash",
            Action::PermDelete => "perm-delete",
        }
    }
}

/// One-line outcome headline (#28): dry-runs name the `--execute` gate,
/// trash outcomes say restorable, perm-delete says irreversible. Executed
/// counts read "done of matched" so partial failures never pose as full
/// success; per-file failures list after the headline in `apply_summary`
/// and under `errors` in outcome JSON.
fn outcome_headline(outcome: &SweepOutcome) -> String {
    let matched = outcome.report.matches.len();
    if outcome.action == Action::Report {
        return "Report only — nothing modified.".to_string();
    }
    if outcome.is_dry_run() {
        if outcome.action == Action::Trash {
            return format!(
                "Dry-run: would move {matched} file(s) to OS trash (restorable) — pass --execute to apply."
            );
        }
        debug_assert_eq!(outcome.action, Action::PermDelete);
        return format!(
            "Dry-run: would permanently delete {matched} file(s) (irreversible) — pass --execute to apply."
        );
    }
    let done = outcome.removed.len();
    if outcome.action == Action::Trash {
        format!("Trashed {done} of {matched} file(s) — restorable from OS trash.")
    } else {
        debug_assert_eq!(outcome.action, Action::PermDelete);
        format!("Permanently deleted {done} of {matched} file(s) — irreversible.")
    }
}

/// Human outcome text appended after the match table: the headline plus any
/// per-file apply failures.
fn apply_summary(outcome: &SweepOutcome) -> String {
    let mut out = outcome_headline(outcome);
    out.push('\n');
    if !outcome.errors.is_empty() {
        out.push_str(&format!(
            "Errors: {} file(s) could not be applied:\n",
            outcome.errors.len()
        ));
        for e in &outcome.errors {
            out.push_str(&format!("  {}: {}\n", e.path.display(), e.message));
        }
    }
    out
}

/// Sweep with an Action and an explicit execute gate (#28).
/// Always scans and matches exactly like `run_sweep_report_with_filter`;
/// without `execute`, destructive Actions only report and change nothing.
/// The reference database is only ever opened read-only; no Classification
/// or moves to Category subfolders occur.
pub fn run_sweep_with_action(
    target: &Path,
    db_path: &Path,
    categories: Option<&HashSet<String>>,
    action: Action,
    execute: bool,
) -> Result<SweepOutcome, SweepError> {
    let report = run_sweep_report_with_filter(target, db_path, categories)?;
    if action == Action::Report || !execute {
        return Ok(SweepOutcome {
            report,
            action,
            executed: false,
            removed: Vec::new(),
            errors: Vec::new(),
        });
    }
    // Destructive execute path lands in the next slice (trash/perm-delete).
    let mut removed = Vec::new();
    let mut errors = Vec::new();
    for m in &report.matches {
        let res = match action {
            Action::Trash => trash::delete(&m.path).map_err(|e| e.to_string()),
            Action::PermDelete => {
                std::fs::remove_file(&m.path).map_err(|e| e.to_string())
            }
            Action::Report => Ok(()),
        };
        match res {
            Ok(()) => removed.push(m.path.clone()),
            Err(message) => errors.push(ApplyError { path: m.path.clone(), message }),
        }
    }
    Ok(SweepOutcome {
        report,
        action,
        executed: true,
        removed,
        errors,
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

/// Shared per-file error array (`hash_errors` / apply `errors`) so the
/// machine-readable shape cannot drift between report and outcome JSON.
fn push_file_errors_json(out: &mut String, field: &str, errors: &[ApplyError]) {
    out.push_str(&format!("  \"{field}\": ["));
    if errors.is_empty() {
        out.push_str("],\n");
        return;
    }
    out.push('\n');
    for (i, e) in errors.iter().enumerate() {
        out.push_str(&format!(
            "    {{\"path\": \"{}\", \"message\": \"{}\"}}{}\n",
            json_escape(&e.path.to_string_lossy()),
            json_escape(&e.message),
            if i + 1 == errors.len() { "" } else { "," }
        ));
    }
    out.push_str("  ],\n");
}

/// Shared `matches` array serializer for report and outcome JSON so the
/// machine-readable match shape cannot drift between the two.
fn push_matches_json(out: &mut String, matches: &[SweepMatch]) {
    out.push_str("  \"matches\": [");
    if matches.is_empty() {
        out.push_str("]\n");
    } else {
        out.push('\n');
        for (i, m) in matches.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!(
                "      \"path\": \"{}\",\n",
                json_escape(&m.path.to_string_lossy())
            ));
            out.push_str(&format!("      \"hash\": \"{}\",\n", json_escape(&m.hash)));
            out.push_str(&format!("      \"category\": \"{}\",\n", json_escape(&m.category)));
            out.push_str(&format!("      \"triaged_at\": {}\n", m.triaged_at));
            if i + 1 == matches.len() {
                out.push_str("    }\n");
            } else {
                out.push_str("    },\n");
            }
        }
        out.push_str("  ]\n");
    }
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
        out.push_str("path\thash\tcategory\ttriaged_at\n");
        for m in &report.matches {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                m.path.display(),
                m.hash,
                m.category,
                m.triaged_at
            ));
        }
    }
    if !report.hash_errors.is_empty() {
        out.push_str(&format!(
            "Hash errors: {} file(s) skipped:\n",
            report.hash_errors.len()
        ));
        for e in &report.hash_errors {
            out.push_str(&format!("  {}: {}\n", e.path.display(), e.message));
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
    push_file_errors_json(&mut out, "hash_errors", &report.hash_errors);
    push_matches_json(&mut out, &report.matches);
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

/// Format a Sweep outcome (report plus Action result) as human table or
/// machine-readable json. Table appends the restorable/irreversible summary;
/// json carries `action`, `executed`, `summary` (the same
/// restorable/irreversible headline), `removed` and `errors` alongside the
/// report fields so scripts can follow up.
pub fn format_outcome(outcome: &SweepOutcome, format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => format!("{}{}", format_table(&outcome.report), apply_summary(outcome)),
        OutputFormat::Json => {
            let mut out = String::new();
            out.push_str("{\n");
            out.push_str(&format!(
                "  \"target\": \"{}\",\n",
                json_escape(&outcome.report.target.to_string_lossy())
            ));
            out.push_str(&format!(
                "  \"reference\": \"{}\",\n",
                json_escape(&outcome.report.reference_db.to_string_lossy())
            ));
            out.push_str(&format!("  \"scanned\": {},\n", outcome.report.scanned));
            out.push_str(&format!("  \"matched\": {},\n", outcome.report.matches.len()));
            push_file_errors_json(&mut out, "hash_errors", &outcome.report.hash_errors);
            out.push_str(&format!(
                "  \"action\": \"{}\",\n",
                outcome.action.as_str()
            ));
            out.push_str(&format!("  \"executed\": {},\n", outcome.executed));
            out.push_str(&format!(
                "  \"summary\": \"{}\",\n",
                json_escape(&outcome_headline(outcome))
            ));
            out.push_str("  \"removed\": [");
            if outcome.removed.is_empty() {
                out.push_str("],\n");
            } else {
                out.push('\n');
                for (i, p) in outcome.removed.iter().enumerate() {
                    out.push_str(&format!(
                        "    \"{}\"{}",
                        json_escape(&p.to_string_lossy()),
                        if i + 1 == outcome.removed.len() { "\n" } else { ",\n" }
                    ));
                }
                out.push_str("  ],\n");
            }
            push_file_errors_json(&mut out, "errors", &outcome.errors);
            push_matches_json(&mut out, &outcome.report.matches);
            out.push_str("}\n");
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::FileHash;
    use crate::store::FileRecord;
    use std::fs;
    use tempfile::TempDir;

    fn write_target(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    /// Hash `file` and record it in the reference database as triaged to
    /// `category` at `triaged_at`. Returns the lower-case sha256.
    fn insert_known(
        store: &Store,
        file: &Path,
        name: &str,
        category: &str,
        triaged_at: i64,
    ) -> String {
        let hash = compute_sha256(file).unwrap().to_ascii_lowercase();
        let validated = FileHash::new(&hash).unwrap();
        let size = fs::metadata(file).unwrap().len() as i64;
        store
            .insert_file(
                &FileRecord::new(&validated, name, &format!("{category}/{name}"), category, size, 0, triaged_at)
                    .unwrap(),
            )
            .unwrap();
        hash
    }

    #[test]
    fn report_lists_matched_with_path_hash_category_time_and_leaves_files() {
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
        assert_eq!(m.category, "keep");
        assert_eq!(m.triaged_at, triaged_at);

        // Read-only: files untouched, database has no extra rows.
        assert!(known.exists());
        assert!(unknown.exists());
        assert_eq!(fs::read(&known).unwrap(), b"known content");
        assert_eq!(store.all_hashes().unwrap().len(), 1);
        assert!(!store.contains(&compute_sha256(&unknown).unwrap()).unwrap());
        assert!(report.hash_errors.is_empty());
    }

    #[test]
    fn hash_failure_skips_that_file_and_continues_with_the_rest() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let known = write_target(&target, "known.jpg", b"known content");
        let locked = write_target(&target, "locked.jpg", b"locked content");
        insert_known(&store, &known, "known.jpg", "keep", 11);
        // Same bytes as a known hash so this *would* match if hashing succeeded.
        insert_known(&store, &locked, "locked.jpg", "keep", 12);

        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&locked, perms).unwrap();

        let outcome = run_sweep_with_action(
            &target,
            &store.db_path(),
            None,
            Action::PermDelete,
            true,
        );
        // Restore so TempDir cleanup and later asserts can read the file.
        let mut restore = fs::metadata(&locked)
            .map(|m| m.permissions())
            .unwrap_or_else(|_| fs::Permissions::from_mode(0o644));
        restore.set_mode(0o644);
        let _ = fs::set_permissions(&locked, restore);

        let outcome = outcome.unwrap();
        assert_eq!(outcome.report.scanned, 2, "unreadable files still count as scanned");
        assert_eq!(
            outcome.report.matches.len(),
            1,
            "readable match must still be reported"
        );
        assert_eq!(outcome.report.matches[0].path, known);
        assert_eq!(outcome.report.hash_errors.len(), 1);
        assert_eq!(outcome.report.hash_errors[0].path, locked);
        assert_eq!(outcome.file_error_count(), 1);
        assert_eq!(outcome.removed, vec![known.clone()]);
        assert!(!known.exists(), "readable match is still applied");
        assert!(locked.exists(), "unhashed file must not be applied");

        let table = format_outcome(&outcome, OutputFormat::Table);
        assert!(
            table.contains("Hash errors: 1 file(s) skipped:"),
            "table must list hash failures:\n{table}"
        );
        assert!(table.contains("locked.jpg"), "table must name the skipped file:\n{table}");
        let json = format_outcome(&outcome, OutputFormat::Json);
        assert!(json.contains("\"hash_errors\""), "json must carry hash_errors:\n{json}");
        assert!(json.contains("locked.jpg"), "json must name the skipped file:\n{json}");
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
        assert!(table.contains("keep"), "table must list origin Category:\n{table}");
        assert!(table.contains("42"), "table must list triage time:\n{table}");
        assert!(!table.contains("unknown.jpg"), "table must not list unmatched:\n{table}");

        // Machine-readable json carries the same match.
        assert!(json.contains("\"matches\""), "json must carry matches:\n{json}");
        assert!(json.contains("known.jpg"), "json must list matched path:\n{json}");
        assert!(json.contains(&known_hash), "json must list hash:\n{json}");
        assert!(json.contains("keep"), "json must list origin Category:\n{json}");
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
        let before_categories = store.categories().unwrap();
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

        // No row growth, no category changes, no file moves, no duplicate/ subfolder.
        let reference = Store::open_reference_file(&store.db_path()).unwrap();
        assert_eq!(reference.all_hashes().unwrap(), before_hashes);
        assert_eq!(reference.categories().unwrap(), before_categories);
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
    fn filtered_run_matches_only_included_categories_and_leaves_others_unreported() {
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
        assert_eq!(report.matches[0].category, "keep");

        // Excluded origin left alone and unreported; unknown untouched.
        assert!(maybe_file.exists());
        assert!(unknown.exists());
        assert_eq!(fs::read(&maybe_file).unwrap(), b"maybe content");
        assert!(!report.matches.iter().any(|m| m.path == maybe_file));
        assert!(!report.matches.iter().any(|m| m.path == unknown));
    }

    #[test]
    fn category_args_split_commas_trim_and_resolve_display_alias_case_insensitive() {
        use crate::config::Category;

        let categories = vec![
            Category { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Category { display_name: "Top Picks".into(), folder_name: "top_picks".into(), shortcut: "2".into() },
        ];

        // Repeatable + comma-separated, trimmed, lower-cased, empties dropped.
        let raw = vec!["keep, KEEP ".to_string(), "  ".to_string(), "Top Picks".to_string()];
        let tokens = parse_category_args(&raw);
        assert!(tokens.contains("keep"));
        assert!(tokens.contains("top picks"));
        assert_eq!(tokens.len(), 2);

        // Display-name alias resolves to folder; unknown stays as-is (matches nothing).
        let resolved = resolve_category_filter(&["KEEP".to_string()], &categories);
        assert!(resolved.contains("keep"));
        let resolved_display = resolve_category_filter(&["top picks".to_string()], &categories);
        assert!(resolved_display.contains("top_picks"));
        let resolved_unknown = resolve_category_filter(&["nope".to_string()], &categories);
        assert!(resolved_unknown.contains("nope"));
    }

    #[test]
    fn toggle_selection_parses_numbers_names_all_none_and_lists_categories() {
        use crate::config::Category;

        let categories = vec![
            Category { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Category { display_name: "Maybe".into(), folder_name: "maybe".into(), shortcut: "2".into() },
            Category { display_name: "Reject".into(), folder_name: "reject".into(), shortcut: "3".into() },
        ];

        // Toggle list is sourced from database Categories.
        let list = format_toggle_list(&categories);
        assert!(list.contains("Keep"));
        assert!(list.contains("keep"));
        assert!(list.contains("Maybe"));

        // Empty / all means default-all (None).
        assert_eq!(parse_toggle_selection("", &categories), None);
        assert_eq!(parse_toggle_selection("all", &categories), None);
        assert_eq!(parse_toggle_selection("ALL", &categories), None);

        // None means match nothing.
        assert_eq!(
            parse_toggle_selection("none", &categories),
            Some(std::collections::HashSet::new())
        );

        // Numbers select by position.
        let sel = parse_toggle_selection("1,3", &categories).unwrap();
        assert!(sel.contains("keep"));
        assert!(sel.contains("reject"));
        assert!(!sel.contains("maybe"));

        // Names select case-insensitively, folder or display.
        let sel = parse_toggle_selection("KEEP, maybe", &categories).unwrap();
        assert!(sel.contains("keep"));
        assert!(sel.contains("maybe"));

        // Unknown names are kept as-is (match nothing), identical to the flag.
        let sel = parse_toggle_selection("nope", &categories).unwrap();
        assert_eq!(sel, std::collections::HashSet::from(["nope".to_string()]));
        let via_toggle = parse_toggle_selection("keep,nope", &categories).unwrap();
        let via_flag = resolve_category_filter(&["keep,nope".to_string()], &categories);
        assert_eq!(via_toggle, via_flag, "toggle line and flag must agree on unknowns");
    }

    #[test]
    fn cli_category_filter_defaults_all_on_absent_or_empty() {
        use crate::config::Category;

        let categories = vec![
            Category { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
        ];

        // Absent flag means default-all.
        assert_eq!(resolve_cli_category_filter(&[], &categories), None);
        // An explicit selection that resolves to nothing also means all —
        // only a non-empty selection narrows the Sweep.
        assert_eq!(resolve_cli_category_filter(&["".to_string()], &categories), None);
        assert_eq!(resolve_cli_category_filter(&["  ".to_string()], &categories), None);
        // Non-empty selections pass through, unknowns kept (match nothing).
        assert_eq!(
            resolve_cli_category_filter(&["keep".to_string()], &categories),
            Some(std::collections::HashSet::from(["keep".to_string()]))
        );
        assert_eq!(
            resolve_cli_category_filter(&["nope".to_string()], &categories),
            Some(std::collections::HashSet::from(["nope".to_string()]))
        );
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

        // Default (None) matches hashes from all origin Categories.
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
    fn toggle_selection_feeds_same_filter_as_categories_flag() {
        use crate::config::Category;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();
        // Custom Categories prove toggles are sourced from the database, not defaults.
        let custom = vec![
            Category { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Category { display_name: "Archive".into(), folder_name: "archive".into(), shortcut: "2".into() },
        ];
        store.set_categories(&custom).unwrap();
        let db_categories = store.categories().unwrap();
        assert_eq!(db_categories, custom);

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let keep_file = write_target(&target, "keep.jpg", b"keep v");
        let arch_file = write_target(&target, "arch.jpg", b"arch v");
        insert_known(&store, &keep_file, "keep.jpg", "keep", 31);
        insert_known(&store, &arch_file, "arch.jpg", "archive", 32);

        // Same selection via number toggle and via --categories flag value.
        let via_toggle = parse_toggle_selection("1", &db_categories).unwrap();
        let via_arg = resolve_category_filter(&["keep".to_string()], &db_categories);
        assert_eq!(via_toggle, via_arg);

        let from_toggle =
            run_sweep_report_with_filter(&target, &store.db_path(), Some(&via_toggle)).unwrap();
        let from_arg =
            run_sweep_report_with_filter(&target, &store.db_path(), Some(&via_arg)).unwrap();
        assert_eq!(from_toggle.matches, from_arg.matches);
        assert_eq!(from_toggle.matches.len(), 1);
        assert_eq!(from_toggle.matches[0].category, "keep");
    }

    #[test]
    fn dry_run_with_trash_or_delete_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let known = write_target(&target, "known.jpg", b"known content");
        let unknown = write_target(&target, "unknown.jpg", b"brand new");
        insert_known(&store, &known, "known.jpg", "keep", 11);

        for action in [Action::Trash, Action::PermDelete] {
            let outcome =
                run_sweep_with_action(&target, &store.db_path(), None, action, false)
                    .unwrap();
            assert_eq!(outcome.report.matches.len(), 1);
            assert!(!outcome.executed, "dry-run must not execute {action:?}");
            assert!(outcome.removed.is_empty());
            assert!(outcome.errors.is_empty());
            assert!(known.exists(), "dry-run must leave matched file: {action:?}");
            assert!(unknown.exists());
            assert_eq!(fs::read(&known).unwrap(), b"known content");
            assert_eq!(store.all_hashes().unwrap().len(), 1);
        }
    }

    #[test]
    fn trash_execute_removes_only_matched_and_keeps_database() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let known = write_target(&target, "known.jpg", b"known content");
        let unknown = write_target(&target, "unknown.jpg", b"brand new");
        insert_known(&store, &known, "known.jpg", "keep", 11);
        let before_hashes = store.all_hashes().unwrap();

        let outcome =
            run_sweep_with_action(&target, &store.db_path(), None, Action::Trash, true)
                .unwrap();
        assert!(outcome.executed);
        assert_eq!(outcome.report.matches.len(), 1);
        assert_eq!(outcome.removed, vec![known.clone()]);
        assert!(outcome.errors.is_empty());
        assert!(!known.exists(), "trash must remove matched file from target");
        assert!(unknown.exists(), "unmatched files are never modified");
        assert_eq!(fs::read(&unknown).unwrap(), b"brand new");
        // Reference database gains no rows, loses none; no Category moves.
        assert_eq!(store.all_hashes().unwrap(), before_hashes);
        assert!(!target.join("duplicate").exists());
        for entry in fs::read_dir(&target).unwrap().flatten() {
            assert_ne!(entry.path(), target.join("keep"));
        }
    }

    #[test]
    fn perm_delete_execute_honors_category_filter_and_removes_irreversibly() {
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
        let before_hashes = store.all_hashes().unwrap();

        let mut only_keep = HashSet::new();
        only_keep.insert("keep".to_string());
        let outcome = run_sweep_with_action(
            &target,
            &store.db_path(),
            Some(&only_keep),
            Action::PermDelete,
            true,
        )
        .unwrap();
        assert!(outcome.executed);
        assert_eq!(outcome.report.matches.len(), 1);
        assert_eq!(outcome.report.matches[0].category, "keep");
        assert_eq!(outcome.removed, vec![keep_file.clone()]);
        assert!(outcome.errors.is_empty());
        assert!(!keep_file.exists(), "perm-delete removes matched file irreversibly");
        assert!(maybe_file.exists(), "filtered-out origins are left alone");
        assert!(unknown.exists(), "unknown-hash files are never modified");
        assert_eq!(store.all_hashes().unwrap(), before_hashes);
    }

    #[test]
    fn outcome_messaging_states_restorable_vs_irreversible_and_execute_gate() {
        use std::collections::HashSet;

        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let store = Store::open(&source).unwrap();

        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();
        let known = write_target(&target, "known.jpg", b"known content");
        insert_known(&store, &known, "known.jpg", "keep", 11);

        // Dry-run trash: restorable wording plus the execute hint, files untouched.
        let dry_trash =
            run_sweep_with_action(&target, &store.db_path(), None, Action::Trash, false)
                .unwrap();
        assert!(dry_trash.is_dry_run());
        let dry_table = format_outcome(&dry_trash, OutputFormat::Table);
        assert!(dry_table.contains("known.jpg"), "dry-run still lists matches:\n{dry_table}");
        for needle in ["Dry-run", "--execute", "trash", "restorable"] {
            assert!(dry_table.contains(needle), "dry-run trash must say {needle:?}:\n{dry_table}");
        }
        assert!(known.exists());

        // Dry-run perm-delete: irreversible wording plus the execute hint.
        let dry_del = run_sweep_with_action(
            &target,
            &store.db_path(),
            None,
            Action::PermDelete,
            false,
        )
        .unwrap();
        let dry_del_table = format_outcome(&dry_del, OutputFormat::Table);
        for needle in ["Dry-run", "--execute", "irreversible"] {
            assert!(
                dry_del_table.contains(needle),
                "dry-run perm-delete must say {needle:?}:\n{dry_del_table}"
            );
        }
        assert!(known.exists());

        // Executed trash: restorable outcome, never irreversible wording.
        let done_trash =
            run_sweep_with_action(&target, &store.db_path(), None, Action::Trash, true)
                .unwrap();
        assert!(!done_trash.is_dry_run());
        let done_table = format_outcome(&done_trash, OutputFormat::Table);
        assert!(done_table.contains("Trashed 1 of 1"), "trash outcome counts done of matched:\n{done_table}");
        assert!(done_table.contains("restorable"), "trash outcome is restorable:\n{done_table}");
        assert!(!done_table.to_ascii_lowercase().contains("irreversible"));
        assert!(!known.exists());

        // Executed perm-delete on a fresh match: irreversible outcome.
        let known2 = write_target(&target, "known2.jpg", b"known content");
        let done_del = run_sweep_with_action(
            &target,
            &store.db_path(),
            Some(&HashSet::from(["keep".to_string()])),
            Action::PermDelete,
            true,
        )
        .unwrap();
        assert_eq!(done_del.removed, vec![known2.clone()]);
        let done_del_table = format_outcome(&done_del, OutputFormat::Table);
        for needle in ["Permanently deleted 1 of 1", "irreversible"] {
            assert!(
                done_del_table.contains(needle),
                "perm-delete outcome must say {needle:?}:\n{done_del_table}"
            );
        }
        assert!(!known2.exists());

        // Machine-readable outcome carries action and execute state.
        let json = format_outcome(&done_del, OutputFormat::Json);
        for needle in ["\"action\"", "\"executed\"", "\"summary\"", "perm-delete", "known2.jpg", "irreversible"] {
            assert!(json.contains(needle), "json outcome must carry {needle:?}:\n{json}");
        }
    }

    #[test]
    fn partial_apply_headline_reports_done_of_matched_and_lists_failures() {
        let target = PathBuf::from("/tmp/target");
        let made = |name: &str| SweepMatch {
            path: target.join(name),
            hash: "ab".repeat(32),
            category: "keep".to_string(),
            triaged_at: 7,
        };
        let first = made("a.jpg");
        let second = made("b.jpg");
        let outcome = SweepOutcome {
            report: SweepReport {
                target: target.clone(),
                reference_db: PathBuf::from("/tmp/source/organizer.db"),
                scanned: 2,
                matches: vec![first.clone(), second.clone()],
                hash_errors: Vec::new(),
            },
            action: Action::Trash,
            executed: true,
            removed: vec![first.path.clone()],
            errors: vec![ApplyError {
                path: second.path.clone(),
                message: "busy".to_string(),
            }],
        };
        let table = format_outcome(&outcome, OutputFormat::Table);
        assert!(
            table.contains("Trashed 1 of 2 file(s) — restorable from OS trash."),
            "headline must not pose a partial apply as full success:\n{table}"
        );
        assert!(
            table.contains("Errors: 1 file(s) could not be applied:"),
            "failures must list after the headline:\n{table}"
        );
        assert!(table.contains("b.jpg"), "failed path must be named:\n{table}");
        let json = format_outcome(&outcome, OutputFormat::Json);
        assert!(
            json.contains("Trashed 1 of 2 file(s) — restorable from OS trash."),
            "json summary must carry the same headline:\n{json}"
        );
    }
}
