use clap::Parser;
use gtk4::glib;
use organizer_lib::store::Store;
use organizer_lib::sweep::{
    format_outcome, resolve_cli_category_filter, resolve_reference_db_path,
    run_sweep_with_action, Action, OutputFormat, SweepError,
};
use organizer_lib::sweep_tui::{
    category_options, count_hashes_per_category, run_categories_tui, TuiOutcome,
};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "organizer", version, about = "File-triaging desktop app")]
struct Args {
    #[arg(value_name = "SOURCE_FOLDER")]
    source_folder: Option<PathBuf>,

    /// Headless bundling probe for local smoke (no window): prints
    /// GLYCIN_DATA_DIR/XDG_DATA_DIRS/GST_PLUGIN_SYSTEM_PATH, bwrap version,
    /// sandbox status and gtk4paintablesink availability, then exits.
    /// Used by packaging/smoke.sh (local pre-push checks; CI builds the
    /// AppImage only — see .github/workflows/appimage.yml).
    #[arg(long = "self-test-sandbox", hide = true)]
    self_test_sandbox: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Sweep a Target Folder against a reference Organizer Database.
    /// Scans a flat Supported-Format Snapshot, matches sha256, reports matched
    /// path, hash, origin Category and triage time. Default is a read-only
    /// dry-run report; `--action trash|perm-delete` with `--execute` applies
    /// the Action to matched Target Folder files only. The reference
    /// database is never modified; unmatched files are untouched and unlisted,
    /// and no Classification or moves to Category subfolders occur.
    // Canonical name follows the domain vocabulary (Sweep); `cleanup` stays a
    // visible alias for the ADR 0007 contract (`organizer cleanup <TARGET>`).
    #[command(visible_alias = "cleanup")]
    Sweep {
        /// Target Folder to scan (flat, Supported Format only).
        #[arg(value_name = "TARGET_FOLDER")]
        target: PathBuf,

        /// Path to the reference organizer.db file or its Source Folder.
        /// Defaults to ./organizer.db in the current folder.
        // `--reference-db` / `--reference` are accepted because the issue
        // text calls it the "reference database" while ADR 0007 calls it `--db`.
        #[arg(long, value_name = "DB_PATH", aliases = ["reference-db", "reference"])]
        db: Option<PathBuf>,

        /// Output format: human table or machine-readable json.
        #[arg(long, value_enum, default_value = "table")]
        format: OutputFormat,

        /// Only match hashes from these origin Categories (folder or display
        /// name, repeatable and comma-separated, case-insensitive; empty
        /// means all).
        /// Skips the interactive toggle screen; scripting escape hatch with
        /// identical semantics to TUI toggles.
        #[arg(long, value_name = "CATEGORY")]
        categories: Vec<String>,

        /// Skip the interactive toggle screen and match all origin Categories.
        /// For scripts; non-TTY runs already skip the TUI by default.
        #[arg(long)]
        non_interactive: bool,

        /// What to do with matched Target Folder files: `report` (default)
        /// only lists them, `trash` moves them to OS trash (restorable),
        /// `perm-delete` removes them irreversibly. Without `--execute` the
        /// run only reports and changes nothing, whatever is selected here.
        #[arg(long, value_enum, default_value = "report")]
        action: Action,

        /// Actually apply `--action trash|perm-delete` to matched files.
        /// Without it the run is a dry-run report and changes nothing.
        #[arg(long)]
        execute: bool,
    },
}

/// CLI Sweep entry point (#26 dry-run, #27 category filter, #28 apply).
/// Resolves `--db` (defaulting to the current folder), then picks the category
/// filter: `--categories` wins (no TUI), else the fullscreen ratatui checklist
/// when interactive, else default-all for scripts/non-TTY. Runs the Sweep
/// with the `--action` Action: without `--execute` every Action
/// only reports and changes nothing; with `--execute`, `trash` moves matched
/// Target Folder files to OS trash (restorable) and `perm-delete` removes
/// them irreversibly. Prints table/json plus the restorable/irreversible
/// outcome summary.
/// Returns a process exit code: 0 on success (including empty matches),
/// 1 on error (missing database, bad target, database failure, any per-file
/// hash or apply failure, TUI failure), 130 when the user cancels the
/// toggle screen (128 + SIGINT convention for user-aborted, so scripts can
/// tell cancel apart from failure). A single unreadable file does not abort
/// the Sweep: it is listed as a hash error and remaining files still match.
/// The reference database is never modified; unmatched files are untouched;
/// no Classification or moves to Category subfolders occur.
fn run_sweep_cli(
    target: PathBuf,
    db_raw: Option<PathBuf>,
    categories_raw: Vec<String>,
    non_interactive: bool,
    format: OutputFormat,
    action: Action,
    execute: bool,
) -> i32 {
    use std::io::IsTerminal;
    let db_path = resolve_reference_db_path(db_raw.as_deref());
    // Load reference Categories first: missing database errors clearly before any TUI.
    let reference = match Store::open_reference_file(&db_path) {
        Ok(s) => s,
        Err(e) => {
            // Same clear wording as the runner seam (#26): name the missing
            // path and hint --db, never create the file.
            let msg = match &e {
                organizer_lib::store::StoreError::Io(ioe)
                    if ioe.kind() == std::io::ErrorKind::NotFound =>
                {
                    SweepError::MissingDatabase(db_path.clone()).to_string()
                }
                other => SweepError::Database(other.to_string()).to_string(),
            };
            eprintln!("organizer: {msg}");
            return 1;
        }
    };
    let db_categories = match reference.categories() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("organizer: database error: {e}");
            return 1;
        }
    };
    // Filter selection: --categories bypasses the TUI entirely; absent or
    // empty means default-all. Only a non-empty selection narrows the Sweep.
    let category_filter: Option<std::collections::HashSet<String>> =
        if !categories_raw.is_empty() {
            resolve_cli_category_filter(&categories_raw, &db_categories)
        } else if non_interactive {
            None
        } else if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            if db_categories.is_empty() {
                None
            } else {
                // Per-row known-hash badges, grouped from the reference union.
                let counts = match reference.list_files() {
                    Ok(rows) => count_hashes_per_category(&rows),
                    Err(_) => Default::default(),
                };
                let options = category_options(&db_categories, &counts);
                match run_categories_tui(&options, &target, &db_path) {
                    Ok(TuiOutcome::Confirmed(filter)) => filter,
                    Ok(TuiOutcome::Cancelled) => {
                        eprintln!("organizer: sweep cancelled — no files touched.");
                        return 130;
                    }
                    Err(e) => {
                        // Unconfirmed selection must not widen to all origins:
                        // abort without touching files or the database.
                        eprintln!("organizer: toggle screen failed ({e}) — sweep aborted, no files touched.");
                        return 1;
                    }
                }
            }
        } else {
            // Scripts / piped output: no prompt, default-all.
            None
        };
    match run_sweep_with_action(&target, &db_path, category_filter.as_ref(), action, execute) {
        Ok(outcome) => {
            print!("{}", format_outcome(&outcome, format));
            let n = outcome.file_error_count();
            if n == 0 {
                0
            } else {
                eprintln!(
                    "organizer: sweep finished with {n} error(s) — report above lists failures."
                );
                1
            }
        }
        Err(e) => {
            eprintln!("organizer: {e}");
            1
        }
    }
}

/// Headless probe behind `--self-test-sandbox` (spec #21 CI smoke).
/// Runs before any GTK init so it works with no display.
fn run_self_test_sandbox() -> i32 {
    println!("organizer {}", env!("CARGO_PKG_VERSION"));
    for key in [
        "GLYCIN_DATA_DIR",
        "XDG_DATA_DIRS",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_SCANNER",
        "PATH",
    ] {
        println!(
            "{}={}",
            key,
            std::env::var(key).unwrap_or_else(|_| "(unset)".into())
        );
    }
    match std::process::Command::new("bwrap").arg("--version").output() {
        Ok(out) if out.status.success() => {
            print!("bwrap {}", String::from_utf8_lossy(&out.stdout));
        }
        _ => println!("bwrap missing"),
    }
    match organizer_lib::preview::ensure_sandbox_bwrap() {
        Ok(()) => println!("sandbox: Bwrap ok"),
        Err(e) => println!("sandbox: unavailable ({e})"),
    }
    match organizer_lib::video::ensure_init() {
        Ok(()) => {
            if gstreamer::ElementFactory::find(organizer_lib::video::PAINTABLE_SINK).is_some() {
                println!("gtk4paintablesink: available");
            } else {
                println!("gtk4paintablesink: missing (bundle libgstgtk4.so)");
            }
        }
        Err(e) => println!("gstreamer: unavailable ({e})"),
    }
    // Loader conf visibility (what glycin would scan).
    let data_dir =
        std::env::var("GLYCIN_DATA_DIR").unwrap_or_else(|_| "/usr/share".into());
    let mut found = false;
    for base in [data_dir, std::env::var("XDG_DATA_DIRS").unwrap_or_default()] {
        for part in base.split(':') {
            let conf = std::path::Path::new(part)
                .join("glycin-loaders")
                .join("2+")
                .join("conf.d");
            if conf.is_dir() {
                println!("glycin conf.d: {}", conf.display());
                found = true;
            }
        }
    }
    if !found {
        println!("glycin conf.d: not found");
    }
    0
}

fn main() -> glib::ExitCode {
    let args = Args::parse();
    if args.self_test_sandbox {
        std::process::exit(run_self_test_sandbox());
    }
    if let Some(Commands::Sweep {
        target,
        db,
        format,
        categories,
        non_interactive,
        action,
        execute,
    }) = args.command
    {
        std::process::exit(run_sweep_cli(
            target,
            db,
            categories,
            non_interactive,
            format,
            action,
            execute,
        ));
    }
    organizer_lib::app::run(args.source_folder)
}
