//! `just_cache` — move cold, rarely used files onto slower disks and leave a symlink
//! behind so every path keeps working.
//!
//! Three subcommands share the same binary: `sweep` (the original mover), `audit`
//! (read-only report, with `--repair`, of structural inconsistency between the watched
//! tree and the cold tiers), and `explain` (read-only: why one path is where it is, and
//! what a sweep would do with it next). The original flat invocation — flags with no
//! subcommand — still runs a sweep, so existing cron entries and scripts keep working
//! unchanged.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use clap::{Args, Parser, Subcommand};

use just_cache::audit::{self, RepairAction};
use just_cache::catalog;
use just_cache::disk_management::{self, FileEntry};
use just_cache::explain::{self, ExplainContext};
use just_cache::file_movement::{self, FileOutcome, MigrationReport, Policy, UsageTracker};
use just_cache::journal::{self, Journal};
use just_cache::opened::{Coverage, Guards, OpenFiles};
use just_cache::restore::{self, RestoreError, RestoreRequest};
use just_cache::scope::{self, Scope};

/// Exit code for findings that were reported and not resolved, so cron can alert
/// without parsing any text.
const EXIT_FINDINGS: u8 = 1;
/// Exit code for a bad invocation or an unreadable tree.
const EXIT_USAGE: u8 = 2;

fn parse_size_arg(text: &str) -> Result<u64, String> {
    scope::parse_size(text).map_err(|err| err.to_string())
}

#[derive(Debug, Parser)]
#[command(
    name = "just_cache",
    version,
    about = "Tiered storage for cold files: move them to slower disks, leave symlinks",
    long_about = None,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    // The original, subcommand-less form. Its `--watch`/`--dest` are deliberately not
    // clap-required: the derive's flattened extraction would enforce them even when a
    // subcommand is used (clap's `subcommand_negates_reqs` only covers clap's own
    // validator, not the generated `from_arg_matches`), so they are validated by hand
    // in `run_sweep` with the same message a missing flag used to produce.
    #[command(flatten)]
    sweep: SweepArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Move cold files onto the cold tiers, leaving symlinks behind.
    Sweep(SweepArgs),
    /// Report structural inconsistency between the watched tree and the cold tiers.
    Audit(AuditArgs),

    /// Manage the catalog: the source of truth for where files live.
    Catalog(CatalogArgs),
    /// Explain why one path is where it is, and what a sweep would do with it next.
    Explain(ExplainArgs),
    /// Bring an offloaded file back to the hot path, verified.
    Restore(RestoreArgs),
}

/// Everything the mover needs. Also the whole CLI when no subcommand is given.
#[derive(Debug, Args)]
struct SweepArgs {
    /// Directory to watch. Files under it (recursively) are considered for migration.
    #[arg(long, value_name = "DIR")]
    watch: Option<PathBuf>,

    /// Cold-storage root, fastest tier first. Repeat for each slower disk.
    ///
    /// Each destination must already exist: a missing path is reported as an error
    /// rather than created, so an unmounted disk can never be silently replaced by a
    /// plain directory on the wrong filesystem.
    #[arg(long, value_name = "DIR", num_args = 1..)]
    dest: Vec<PathBuf>,

    /// Seconds to sleep between sweeps.
    #[arg(long, value_name = "SECS", default_value_t = 3600)]
    interval: u64,

    /// Run a single sweep and exit.
    #[arg(long)]
    once: bool,

    /// Report what would be moved without moving anything.
    #[arg(long)]
    dry_run: bool,

    /// Only consider files whose last use is at least this many days old.
    #[arg(long, value_name = "DAYS", default_value_t = 30.0)]
    min_idle_days: f64,

    /// Treat a file as pinned once it has been accessed this many times during the run.
    /// Zero disables the pin.
    #[arg(long, value_name = "N", default_value_t = 1)]
    min_observed_accesses: u64,

    /// Only manage files matching these globs. Repeatable. Patterns are relative to
    /// `--watch`; a bare name (`media`, `*.part`) matches at any depth, and including a
    /// directory includes everything under it. Without this flag the whole tree is
    /// managed.
    #[arg(long, value_name = "GLOB")]
    include: Vec<String>,

    /// Never manage files matching these globs. Repeatable, same matching rules as
    /// `--include`, and it wins over it.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// Skip files smaller than this. Binary units: K, M, G, T (e.g. 1MiB, 4K).
    #[arg(long, value_name = "SIZE", default_value = "0", value_parser = parse_size_arg)]
    min_size: u64,

    /// Skip files larger than this — a huge file is a multi-hour transfer, not a quiet
    /// reclaim. Binary units, as above.
    #[arg(long, value_name = "SIZE", value_parser = parse_size_arg)]
    max_size: Option<u64>,

    /// Move files that have more than one hard link. Off by default: a hardlinked pair
    /// cannot be moved across filesystems without silently breaking the link.
    #[arg(long)]
    allow_hardlinked: bool,

    /// Maximum files moved per destination per sweep.
    #[arg(long, value_name = "N", default_value_t = 10)]
    limit: usize,

    /// Leave a destination alone unless it has at least this much free space, on top of
    /// room for the file being moved.
    #[arg(long, value_name = "GB", default_value_t = 1.0)]
    min_free_gb: f64,

    /// Print every file considered, including the ones left alone and why.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Print only problems.
    #[arg(short, long)]
    quiet: bool,
}

impl SweepArgs {
    fn policy(&self) -> Policy {
        Policy {
            min_idle: Duration::from_secs_f64(self.min_idle_days.max(0.0) * 86_400.0),
            observed_access_pin: self.min_observed_accesses,
            limit: self.limit,
            dry_run: self.dry_run,
        }
    }

    fn min_free_bytes(&self) -> u64 {
        (self.min_free_gb.max(0.0) * 1_073_741_824.0) as u64
    }
}

#[derive(Debug, Args)]
struct AuditArgs {
    /// Directory to watch. The same tree a sweep moves files out of.
    #[arg(long, value_name = "DIR")]
    watch: PathBuf,

    /// Cold-storage root, fastest tier first. Repeat for each slower disk. Every
    /// destination must already exist, and a symlink is only "expected" when it points
    /// under one of these.
    #[arg(long, value_name = "DIR", required = true, num_args = 1..)]
    dest: Vec<PathBuf>,

    /// Repair what can be repaired without guessing. Nothing is ever deleted until its
    /// content has been hashed and matched against the copy that is kept.
    #[arg(long)]
    repair: bool,

    /// Print a machine-readable JSON document instead of the summary.
    #[arg(long)]
    json: bool,

    /// How many findings the readable summary lists (JSON always has them all).
    #[arg(long, value_name = "N", default_value_t = audit::DEFAULT_EXAMPLES)]
    examples: usize,
}

/// The `catalog` subcommand and its own subcommands.
#[derive(Debug, Args)]
struct CatalogArgs {
    #[command(subcommand)]
    command: CatalogCommand,
}

#[derive(Debug, Subcommand)]
enum CatalogCommand {
    /// Ingest the current state of a tree the mover has already been running against.
    ///
    /// Files moved before the catalog existed are ingested exactly like ones moved
    /// after. Anything that contradicts what the catalog already recorded is reported,
    /// and the catalog is left as it was rather than rewritten to match the tree.
    Sync(CatalogSyncArgs),
}

#[derive(Debug, Args)]
struct CatalogSyncArgs {
    /// Directory to watch. The same tree a sweep moves files out of.
    #[arg(long, value_name = "DIR")]
    watch: PathBuf,

    /// Cold-storage root, fastest tier first. Repeat for each slower disk. Every

    /// destination must already exist and is used both to resolve migrated symlinks and
    /// to find orphaned copies.
    #[arg(long, value_name = "DIR", required = true, num_args = 1..)]
    dest: Vec<PathBuf>,

    /// The catalog file. Defaults to `.just_cache-catalog.sqlite` beside the watch root.
    #[arg(long, value_name = "FILE")]
    catalog: Option<PathBuf>,
}

/// Everything `explain` needs to answer for one path. The flags deliberately mirror
/// `sweep`'s, because the answer is only trustworthy if it is computed with the same
/// knobs a sweep would use — a different `--exclude` here would explain a different tool.
#[derive(Debug, Args)]
struct ExplainArgs {
    /// The path to explain. Absolute, or relative to `--watch`.
    #[arg(value_name = "PATH")]
    path: PathBuf,

    /// Directory to watch. The same tree a sweep moves files out of.
    #[arg(long, value_name = "DIR")]
    watch: PathBuf,

    /// Cold-storage root, fastest tier first. Repeat for each slower disk. Required so the
    /// answer can name where the file would go (or where a migrated one already lives).
    #[arg(long, value_name = "DIR", required = true, num_args = 1..)]
    dest: Vec<PathBuf>,

    /// Only manage files matching these globs — the same `--include` a sweep uses.
    #[arg(long, value_name = "GLOB")]
    include: Vec<String>,

    /// Never manage files matching these globs — the same `--exclude` a sweep uses.
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// Skip files smaller than this. Binary units: K, M, G, T (e.g. 1MiB, 4K).
    #[arg(long, value_name = "SIZE", default_value = "0", value_parser = parse_size_arg)]
    min_size: u64,

    /// Skip files larger than this. Binary units, as above.
    #[arg(long, value_name = "SIZE", value_parser = parse_size_arg)]
    max_size: Option<u64>,

    /// Only consider files whose last use is at least this many days old.
    #[arg(long, value_name = "DAYS", default_value_t = 30.0)]
    min_idle_days: f64,

    /// Treat a file as pinned once accessed this many times during a run. Zero disables.
    #[arg(long, value_name = "N", default_value_t = 1)]
    min_observed_accesses: u64,

    /// Move files that have more than one hard link. Off by default, matching `sweep`.
    #[arg(long)]
    allow_hardlinked: bool,

    /// Leave a destination alone unless it has at least this much free space.
    #[arg(long, value_name = "GB", default_value_t = 1.0)]
    min_free_gb: f64,

    /// Print a machine-readable JSON document instead of the four-line summary.
    #[arg(long)]
    json: bool,
}

impl ExplainArgs {
    fn policy(&self) -> Policy {
        Policy {
            min_idle: Duration::from_secs_f64(self.min_idle_days.max(0.0) * 86_400.0),
            observed_access_pin: self.min_observed_accesses,
            limit: 10,
            // `explain` never moves anything; dry_run in the policy is the mover's switch,
            // and this command does not call the mover at all.
            dry_run: true,
        }
    }

    fn min_free_bytes(&self) -> u64 {
        (self.min_free_gb.max(0.0) * 1_073_741_824.0) as u64
    }
}

#[derive(Debug, Args)]
struct RestoreArgs {
    /// Path to bring back to the hot tier. Must be inside --watch. It may currently be a
    /// working symlink, a broken one, or missing.
    #[arg(value_name = "PATH")]
    path: PathBuf,

    /// Directory to watch. The same tree a sweep moved the file out of.
    #[arg(long, value_name = "DIR")]
    watch: PathBuf,

    /// Cold-storage root, fastest tier first. Repeat for each slower disk. Every
    /// destination must already exist; the cold copy is looked up at the path relative to
    /// the watched root under each one, in order.
    #[arg(long, value_name = "DIR", required = true, num_args = 1..)]
    dest: Vec<PathBuf>,

    /// Remove the cold copy once the restored file has been verified. Verify-before-delete:
    /// the cold bytes are dropped only after the fresh copy checksums clean.
    #[arg(long)]
    remove_copy: bool,

    /// Print only problems.
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Sweep(args)) => run_sweep(args),
        Some(Command::Audit(args)) => run_audit(args),

        Some(Command::Catalog(args)) => run_catalog(args),
        Some(Command::Explain(args)) => run_explain(args),
        Some(Command::Restore(args)) => run_restore(args),
        None => run_sweep(cli.sweep),
    }
}

fn run_sweep(args: SweepArgs) -> ExitCode {
    let watch = match args.watch.clone() {
        Some(watch) => watch,
        None => {
            eprintln!("just_cache: --watch is required");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if args.dest.is_empty() {
        eprintln!("just_cache: --dest is required (repeat it for each slower disk)");
        return ExitCode::from(EXIT_USAGE);
    }
    if let Err(message) = validate_sweep(&watch, &args) {
        eprintln!("just_cache: {message}");
        return ExitCode::FAILURE;
    }

    let policy = args.policy();
    let scope = match Scope::build(&args.include, &args.exclude, args.min_size, args.max_size) {
        Ok(scope) => scope,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };
    if scope.is_empty_window() {
        eprintln!(
            "just_cache: warning: --min-size is above --max-size ({} vs {}), so nothing can qualify",
            scope::human_bytes(scope.min_size()),
            scope
                .max_size()
                .map(scope::human_bytes)
                .unwrap_or_else(|| "unset".to_string())
        );
    }

    // The journal lives beside the tree it describes. A journal that cannot be read stops
    // the sweep rather than being ignored: it may describe a move whose bytes are already
    // on a cold tier, and sweeping on would both lose that knowledge and start new moves
    // through an unguarded window.
    let mut journal = match Journal::in_tree(&watch) {
        Ok(journal) => journal,
        Err(err) => {
            eprintln!("just_cache: {err}");
            eprintln!(
                "just_cache: refusing to sweep until the journal can be read; look at the \
                 line above, or move the file aside if it is beyond saving"
            );
            return ExitCode::FAILURE;
        }
    };

    // What the last run was in the middle of. Run once, before any new work, so a recovered
    // name is visible to this very sweep rather than the one after it.
    match journal::repair(&mut journal, &watch) {
        Ok(report) => {
            if let Some(summary) = report.summary() {
                println!("{summary}");
                for (relative, outcome) in &report.outcomes {
                    // Routine outcomes only with -v; a restored name or anything that needs
                    // attention always, because "a file came back" is not a quiet event.
                    if outcome.is_notable() || args.verbose > 0 {
                        println!("{}", outcome.describe(relative));
                    }
                }
            }
        }
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    }

    let mut tracker = UsageTracker::new();
    let mut pass = 0u64;

    loop {
        pass += 1;
        let mut state = Sweep {
            watch: &watch,
            dests: &args.dest,
            args: &args,
            policy: &policy,
            scope: &scope,
            tracker: &mut tracker,
            journal: &mut journal,
        };
        let report = sweep(&mut state, pass);
        // Compaction is what keeps the journal describing only what is still in flight:
        // finished moves are dropped, and any incomplete record is written back so the
        // next start reads a file the size of the work actually outstanding.
        let _ = journal.compact();
        if args.once {
            return if report.failed() > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
        std::thread::sleep(Duration::from_secs(args.interval.max(1)));
    }
}

fn run_catalog(args: CatalogArgs) -> ExitCode {
    match args.command {
        CatalogCommand::Sync(sync) => run_catalog_sync(sync),
    }
}

/// Ingest the current state of the tree into the catalog.
///
/// Exits non-zero when the tree and the catalog disagree: a difference is a thing a
/// human has to look at, and cron can alert without parsing any text.
fn run_catalog_sync(args: CatalogSyncArgs) -> ExitCode {
    if !args.watch.is_dir() {
        eprintln!(
            "just_cache: --watch {} is not a directory",
            args.watch.display()
        );
        return ExitCode::from(EXIT_USAGE);
    }
    if let Err(message) = validate_paths(&args.watch, &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }

    let catalog_path = args
        .catalog
        .clone()
        .unwrap_or_else(|| catalog::Catalog::default_path(&args.watch));
    let mut catalog = match catalog::Catalog::open(&catalog_path) {
        Ok(catalog) => catalog,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };

    let report = match catalog.sync(&args.watch, &args.dest) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };

    for line in report.summary_lines() {
        println!("{line}");
    }

    if report.has_differences() {
        ExitCode::from(EXIT_FINDINGS)
    } else {
        ExitCode::SUCCESS
    }
}

fn run_audit(args: AuditArgs) -> ExitCode {
    if let Err(message) = validate_paths(&args.watch, &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }

    let report = match audit::audit(&args.watch, &args.dest) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let repairs = if args.repair {
        match audit::repair(&report) {
            Ok(repairs) => Some(repairs),
            Err(err) => {
                eprintln!("just_cache: {err}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    } else {
        None
    };

    if args.json {
        println!("{}", report.to_json(repairs.as_deref()));
    } else {
        for line in report.summary_lines(args.examples) {
            println!("{line}");
        }
        if let Some(repairs) = &repairs {
            let repaired = repairs.iter().filter(|repair| repair.repaired()).count();
            let refused = repairs
                .iter()
                .filter(|repair| matches!(repair.action, RepairAction::Refused { .. }))
                .count();
            // Counted separately rather than as one "left" remainder: refused and
            // not-attempted are different answers to "why not", and adding the refused
            // count to a remainder that already contained it made the line read as if
            // more had happened than there were findings.
            let not_attempted = repairs
                .iter()
                .filter(|repair| matches!(repair.action, RepairAction::NotAttempted { .. }))
                .count();
            println!(
                "repair: {repaired} of {} finding(s) resolved, {refused} refused, {not_attempted} not attempted",
                repairs.len()
            );
            for repair in repairs.iter().filter(|repair| !repair.repaired()) {
                println!(
                    "  not repaired: {} ({})",
                    repair.path.display(),
                    repair.kind.as_str()
                );
            }
        }
    }

    // Read-only audit alerts on any finding; a repair run only alerts on findings it
    // could not resolve, so a cron job that keeps the tree healthy exits zero.
    let unresolved = match &repairs {
        Some(repairs) => repairs.iter().filter(|repair| !repair.repaired()).count(),
        None => report.findings.len(),
    };
    if unresolved > 0 {
        ExitCode::from(EXIT_FINDINGS)
    } else {
        ExitCode::SUCCESS
    }
}

/// Explain one path. Read-only: it stats, it never opens the file for reading (which would
/// bump the very atime it reports), and it moves nothing. Exit codes are the contract:
/// `0` when the engine manages the path, `1` when it would not be moved, `2` on a bad
/// invocation — so `explain` composes in a shell.
fn run_explain(args: ExplainArgs) -> ExitCode {
    if let Err(message) = validate_paths(&args.watch, &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }

    let scope = match Scope::build(&args.include, &args.exclude, args.min_size, args.max_size) {
        Ok(scope) => scope,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if scope.is_empty_window() {
        eprintln!(
            "just_cache: warning: --min-size is above --max-size ({} vs {}), so nothing can qualify",
            scope::human_bytes(scope.min_size()),
            scope
                .max_size()
                .map(scope::human_bytes)
                .unwrap_or_else(|| "unset".to_string())
        );
    }

    let policy = args.policy();
    // The snapshot names the processes holding the file open; unlike a sweep, nothing is
    // taken live after it, so a descriptor opened during this command cannot be caught —
    // which is the honest limit of a one-shot answer, and the guard report says so.
    let guards = Guards::new(OpenFiles::snapshot(), !args.allow_hardlinked);
    let tracker = UsageTracker::new();

    // A relative path is relative to `--watch`, which is the tree it is documented to live
    // under; an absolute path is used as given.
    let path = if args.path.is_absolute() {
        args.path.clone()
    } else {
        args.watch.join(&args.path)
    };
    let context = ExplainContext {
        path: &path,
        watch: &args.watch,
        dests: &args.dest,
        scope: &scope,
        policy: &policy,
        guards: &guards,
        tracker: &tracker,
        min_free: args.min_free_bytes(),
    };
    let explanation = explain::explain(&context);

    if args.json {
        println!("{}", explanation.to_json());
    } else {
        for line in explanation.summary_lines() {
            println!("{line}");
        }
    }
    ExitCode::from(explanation.exit_code())
}

fn run_restore(args: RestoreArgs) -> ExitCode {
    if let Err(message) = validate_paths(&args.watch, &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }

    let request = RestoreRequest {
        path: &args.path,
        watch: &args.watch,
        dests: &args.dest,
        remove_copy: args.remove_copy,
    };
    match restore::restore(&request) {
        Ok(outcome) => {
            if !args.quiet {
                println!("{}", outcome.describe(&args.path));
            }
            ExitCode::SUCCESS
        }
        // A path outside the tree, or an unusable one, is a bad invocation (exit 2, the
        // same code `audit` uses); everything else is the requested restore failing on its
        // own terms (exit 1), so cron can tell the two apart without parsing text.
        Err(err @ RestoreError::OutsideWatch { .. }) => {
            eprintln!("just_cache: {err}");
            ExitCode::from(EXIT_USAGE)
        }
        Err(err) => {
            eprintln!("just_cache: {err}");
            ExitCode::from(EXIT_FINDINGS)
        }
    }
}

fn validate_sweep(watch: &Path, args: &SweepArgs) -> Result<(), String> {
    validate_paths(watch, &args.dest)?;
    if args.min_idle_days < 0.0 {
        return Err("--min-idle-days cannot be negative".to_string());
    }
    if args.interval == 0 && !args.once {
        return Err("--interval must be at least 1 second".to_string());
    }
    Ok(())
}

/// Shared checks for both subcommands: the watched tree exists, and each destination is
/// a real directory that is not the watched tree itself.
fn validate_paths(watch: &Path, dests: &[PathBuf]) -> Result<(), String> {
    if !watch.is_dir() {
        return Err(format!("--watch {} is not a directory", watch.display()));
    }
    let watched = watch.canonicalize().ok();
    for dest in dests {
        if !dest.is_dir() {
            return Err(format!(
                "--dest {} is not an existing directory (mount it first; just_cache will \
                 not create destination roots)",
                dest.display()
            ));
        }
        if dest.canonicalize().ok() == watched {
            return Err(format!(
                "--dest {} is the watched directory, which would do nothing",
                dest.display()
            ));
        }
    }
    Ok(())
}

/// One sweep over the watched tree: fill each cold tier in turn, fastest disk first.
/// Everything one sweep works with, in one place.
///
/// Gathered here because the parameter list grew every time the mover learned something
/// new — the journal was the third such addition, and the fourth would have been the one
/// that made the call sites unreadable.
struct Sweep<'a> {
    watch: &'a Path,
    dests: &'a [PathBuf],
    args: &'a SweepArgs,
    policy: &'a Policy,
    scope: &'a Scope,
    tracker: &'a mut UsageTracker,
    journal: &'a mut Journal,
}

fn sweep(state: &mut Sweep<'_>, pass: u64) -> MigrationReport {
    let watch = state.watch;
    let dests = state.dests;
    let args = state.args;
    let policy = state.policy;
    let scope = state.scope;
    let tracker: &mut UsageTracker = state.tracker;
    let journal: &mut Journal = state.journal;
    let now = SystemTime::now();

    // One snapshot per sweep, not one per candidate: a process-table scan per file would
    // cost more than the copy it is protecting. The guard is deliberately taken *before*
    // the walk so that a file opened mid-sweep is caught by the re-check in the mover.
    let open_files = OpenFiles::snapshot();
    let coverage_note = open_files.coverage_note();
    let guards = Guards::new(open_files, !args.allow_hardlinked);
    match guards.open_files().coverage() {
        // Cannot check at all: say so rather than pretending the guard exists.
        Coverage::Unsupported => {
            if let Some(note) = &coverage_note {
                eprintln!("just_cache: {note}");
            }
        }
        Coverage::OwnProcessesOnly => {
            if args.verbose > 0 {
                if let Some(note) = &coverage_note {
                    println!("  {note}");
                }
            }
        }
        Coverage::Complete => {}
    }

    let entries = match disk_management::list_files_recursive(watch) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return MigrationReport::default();
        }
    };
    // Out-of-scope files are not observed at all: they are not candidates, and tracking
    // them would only grow the map on a long-running process.
    for entry in entries
        .iter()
        .filter(|entry| !entry.is_symlink && scope.allows(entry).is_ok())
    {
        tracker.observe(entry);
    }
    tracker.retain_present(&entries);

    let mut report = MigrationReport::default();
    let min_free = args.min_free_bytes();
    let mut pending = entries.clone();

    for dest in dests {
        // Both sides matter: the guards are live state (a descriptor can open at any
        // moment), and the room check measures allocated bytes, since the copy preserves
        // holes and `size` would refuse moves the tier can afford.
        let mut context = file_movement::MoveContext {
            policy,
            scope,
            guards: &guards,
            journal,
        };
        let tier = file_movement::migrate_least_used(
            &pending,
            tracker,
            &mut context,
            now,
            |entry: &FileEntry| {
                Ok(disk_management::destination_with_room(
                    dest,
                    entry.allocated,
                    min_free,
                ))
            },
        );

        // Whatever this tier took (or would take) is off the table for slower disks.
        let handled: HashSet<PathBuf> = tier.migrated_paths().into_iter().collect();
        let waiting = tier.waiting_for_room();
        pending.retain(|entry| !handled.contains(&entry.path));
        report.records.extend(tier.records);

        if args.verbose > 0 && waiting > 0 {
            let free = disk_management::available_space(dest)
                .map(scope::human_bytes)
                .unwrap_or_else(|| "unknown".to_string());
            println!(
                "  {} is below the free-space floor ({} free); {} file(s) left waiting",
                dest.display(),
                free,
                waiting
            );
        }

        if policy.dry_run {
            // Nothing on disk changed, so a second tier would just re-report the plan.
            break;
        }
    }

    if args.quiet {
        for record in &report.records {
            if matches!(record.outcome, FileOutcome::Failed(_)) {
                file_movement::log_file_movement(record);
            }
        }
    } else if args.verbose > 0 {
        for line in report.lines() {
            println!("  {line}");
        }
    } else {
        for record in &report.records {
            file_movement::log_file_movement(record);
        }
    }

    if !args.quiet {
        println!(
            "pass {pass}: {} files scanned, {} tracked, {} moved, {} linked, {} waiting for room, {} in use ({} open files seen), {} skipped ({} outside scope or size), {} failed, {} onto the cold tiers",
            entries.len(),
            tracker.tracked_paths(),
            report.moved(),
            report.linked_existing(),
            report.waiting_for_room(),
            report.in_use(),
            guards.open_files().len(),
            report.excluded(),
            report.count(|outcome| matches!(outcome, FileOutcome::Skipped(_))),
            report.failed(),
            scope::human_bytes(report.bytes_moved()),
        );
        if policy.dry_run {
            println!("dry run: nothing was moved");
        }
    }

    report
}
