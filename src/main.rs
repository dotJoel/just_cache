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

use just_cache::audit::{self, AuditSource, RepairAction};
use just_cache::catalog;
use just_cache::disk_management;
use just_cache::explain::{self, ExplainContext};
use just_cache::file_movement::{self, FileOutcome, MigrationReport, Policy, UsageTracker};
use just_cache::journal::{self, Journal};
use just_cache::locate::{self, LocateRequest};
use just_cache::opened::{Coverage, Guards, OpenFiles};
use just_cache::policy::{Lifecycle, RuleSet};
use just_cache::reconcile;
use just_cache::restore::{self, RestoreError, RestoreRequest};
use just_cache::scope::{self, Scope};
use just_cache::scrub::{self, ScrubRequest};
use just_cache::tiers::TierSet;

/// Exit code for findings that were reported and not resolved, so cron can alert
/// without parsing any text.
const EXIT_FINDINGS: u8 = 1;
/// Exit code for a bad invocation or an unreadable tree.
const EXIT_USAGE: u8 = 2;

fn parse_size_arg(text: &str) -> Result<u64, String> {
    scope::parse_size(text).map_err(|err| err.to_string())
}

/// Parse `--read-budget`. A zero budget is refused rather than accepted as "read nothing":
/// the same reason `--rate 0` is, a switch that silently no-ops is a mistyped test.
fn parse_read_budget(text: &str) -> Result<u64, String> {
    let bytes = parse_size_arg(text)?;
    if bytes == 0 {
        return Err("--read-budget must be at least 1 byte".to_string());
    }
    Ok(bytes)
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
    /// Where an object lives — every copy, its tier, and which one is the tier of record —
    /// by namespace path or by content digest.
    Locate(LocateArgs),
    /// Bring an offloaded file back to the hot path, verified.
    Restore(RestoreArgs),
    /// Read every stored copy back and verify it against the catalog, repairing rot.
    Scrub(ScrubArgs),
    /// Rebuild copies that are missing from a destination root (a disk re-added after a
    /// sweep), from a surviving sibling that verifies against the recorded checksum.
    Reconcile(ReconcileArgs),
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

    /// Tier configuration (`tiers.toml`): names each tier and describes it by kind, path,
    /// volatility, recall and copy floor. Defaults to `tiers.toml` beside the watch root,
    /// consulted only when it is already there — never created. An explicitly named file
    /// that is missing or malformed is an error, not a silent fallback.
    #[arg(long, value_name = "FILE")]
    tiers: Option<PathBuf>,

    /// Lifecycle policy (`policy.toml`): rules that decide, per file, which tier it
    /// belongs on. Each rule has a `match` glob and a `down = { after_idle, from, to }`
    /// transition; a rule can `pin` patterns to protect, and an `up = { on_access }` rule
    /// names the promotion. Defaults to `policy.toml` beside the watch root, consulted
    /// only when it is already there — never created. With no file the sweep's decision is
    /// the flag-driven `--min-idle-days` one, exactly as before.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,

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

    /// Durability floor: place this many copies, on this many distinct `--dest` roots,
    /// before removing the source. Default 1 keeps the original single-copy behaviour;
    /// N>1 opts into replication (same-host disk failure, not off-host backup).
    #[arg(long, value_name = "N", default_value_t = 1)]
    copies: usize,

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
    fn policy(&self) -> Result<Policy, String> {
        Ok(Policy {
            min_idle: min_idle_duration(self.min_idle_days)?,
            observed_access_pin: self.min_observed_accesses,
            limit: self.limit,
            dry_run: self.dry_run,
        })
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

    /// Tier configuration (`tiers.toml`). Defaults to `tiers.toml` beside the watch root,
    /// consulted only when it is already there — never created. A configured tier is named
    /// in the output instead of its path; a volatile tier is refused as a `--dest`.
    #[arg(long, value_name = "FILE")]
    tiers: Option<PathBuf>,

    /// The catalog file. Defaults to `.just_cache-catalog.sqlite` beside the watch root.
    ///
    /// When a catalog exists, audit answers from it plus one filesystem pass over the
    /// tree instead of walking both sides; without one it falls back to the walk-based
    /// audit. Naming a file that does not exist is an error rather than a silent
    /// fallback, because the operator asked for a specific source of truth and quietly
    /// walking instead would answer from a different one.
    ///
    /// The same file also supplies the scrub-state section reported below the findings:
    /// how many copies were verified, never scrubbed, or marked damaged. That section is
    /// informational and changes neither the findings nor the exit code.
    #[arg(long, value_name = "FILE")]
    catalog: Option<PathBuf>,

    /// Repair what can be repaired without guessing. Nothing is ever deleted until its
    /// content has been hashed and matched against the copy that is kept.
    ///
    /// In catalog mode this never touches the tree or the rows: a catalog/file
    /// disagreement is a finding a human has to interpret, so repair marks it for resync
    /// instead.
    #[arg(long)]
    repair: bool,

    /// Copy floor to audit against: an offloaded object present on fewer than this many
    /// distinct `--dest` roots is a `replica-lost` finding. Default 1 audits the flat
    /// single-copy layout only.
    #[arg(long, value_name = "N", default_value_t = 1)]
    copies: usize,

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

    /// Tier configuration (`tiers.toml`). Defaults to `tiers.toml` beside the watch root,
    /// consulted only when it is already there — never created. A configured tier's
    /// `copies` is recorded as its floor, and a volatile tier is refused as a `--dest`.
    #[arg(long, value_name = "FILE")]
    tiers: Option<PathBuf>,

    /// The catalog file. Defaults to `.just_cache-catalog.sqlite` beside the watch root.
    #[arg(long, value_name = "FILE")]
    catalog: Option<PathBuf>,

    /// Record this as the copy floor for every `--dest` tier, and report objects that
    /// hold fewer verified copies than it. Recorded once per tier, not guessed from how
    /// many copies happen to exist. Default 1.
    #[arg(long, value_name = "N", default_value_t = 1)]
    copies: usize,
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

    /// Tier configuration (`tiers.toml`). Defaults to `tiers.toml` beside the watch root,
    /// consulted only when it is already there — never created. A volatile tier is refused
    /// as a `--dest` (a destination is a home; a volatile tier is a mirror, §2.1).
    #[arg(long, value_name = "FILE")]
    tiers: Option<PathBuf>,

    /// Lifecycle policy (`policy.toml`): the rules that decide where a file belongs, and
    /// the answer to "which rule fired or which exclusion stopped it". Defaults to
    /// `policy.toml` beside the watch root, consulted only when it is already there.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,

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

    /// The catalog file to consult for "where does this object live". Defaults to
    /// `.just_cache-catalog.sqlite` beside the watch root, and is consulted only when that
    /// file already exists — `explain` never creates a catalog. When none exists the
    /// filesystem is the only source, exactly as before.
    #[arg(long, value_name = "FILE")]
    catalog: Option<PathBuf>,
}

impl ExplainArgs {
    fn policy(&self) -> Result<Policy, String> {
        Ok(Policy {
            min_idle: min_idle_duration(self.min_idle_days)?,
            observed_access_pin: self.min_observed_accesses,
            limit: 10,
            // `explain` never moves anything; dry_run in the policy is the mover's switch,
            // and this command does not call the mover at all.
            dry_run: true,
        })
    }

    fn min_free_bytes(&self) -> u64 {
        (self.min_free_gb.max(0.0) * 1_073_741_824.0) as u64
    }
}

/// Where an object lives, by namespace path or by content digest.
#[derive(Debug, Args)]
struct LocateArgs {
    /// A namespace path (relative to the watched tree, the same string `catalog sync`
    /// ingested), or a full or prefixed BLAKE3 hex id. A query of at least eight hex
    /// characters is read as a digest prefix; anything else is read as a path.
    #[arg(value_name = "QUERY")]
    query: String,

    /// The catalog file to read. It must already exist: `locate` never creates one, since
    /// a catalog conjured empty at query time would answer "nothing found" for a tree
    /// that is fine.
    #[arg(long, value_name = "FILE")]
    catalog: PathBuf,

    /// Tier configuration (`tiers.toml`). Names each copy's tier and states its recall
    /// class. Defaults to `tiers.toml` in the catalog's directory, consulted only when it
    /// is already there — never created. An explicitly named file that is missing or
    /// malformed is an error, not a silent fallback to path-as-tier-name.
    #[arg(long, value_name = "FILE")]
    tiers: Option<PathBuf>,

    /// Print a machine-readable JSON document instead of the summary.
    #[arg(long)]
    json: bool,
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

    /// Tier configuration (`tiers.toml`). Defaults to `tiers.toml` beside the watch root,
    /// consulted only when it is already there — never created. A volatile tier is refused
    /// as a `--dest` (a volatile copy is a mirror, never data of record).
    #[arg(long, value_name = "FILE")]
    tiers: Option<PathBuf>,

    /// Remove the cold copy once the restored file has been verified. Verify-before-delete:
    /// the cold bytes are dropped only after the fresh copy checksums clean.
    #[arg(long)]
    remove_copy: bool,

    /// The catalog to verify the restored bytes against. Defaults to
    /// `.just_cache-catalog.sqlite` beside the watch root, and is consulted only when that
    /// file already exists — `restore` never creates a catalog. With a catalog, the
    /// restored bytes are checked against the object's recorded checksum; with none,
    /// against the cold copy, exactly as before.
    #[arg(long, value_name = "FILE")]
    catalog: Option<PathBuf>,

    /// Print only problems.
    #[arg(short, long)]
    quiet: bool,
}

/// Everything `scrub` needs. Unlike restore/sweep it names no `--watch`/`--dest`: the
/// catalog already knows every tier root and every key within it, which is the point of
/// having made the catalog the source of truth.
#[derive(Debug, Args)]
struct ScrubArgs {
    /// The catalog file to scrub.
    #[arg(long, value_name = "FILE")]
    catalog: PathBuf,

    /// Cap read throughput at this many KiB/s, so a scrub of a busy media tier does not
    /// contend with playback. Omit for unlimited.
    #[arg(long, value_name = "KIB")]
    rate: Option<u64>,

    /// Report what scrub would repair, changing nothing: no repair is written and no
    /// last-verified state is recorded, so a dry run is safe to repeat.
    #[arg(long)]
    dry_run: bool,

    /// Print only problems.
    #[arg(short, long)]
    quiet: bool,
}

/// Everything `reconcile` needs. Like `scrub` it names no `--watch`/`--dest`: the catalog
/// holds every tier root and the floor recorded per tier, which is the whole point of
/// having made the catalog the source of truth.
#[derive(Debug, Args)]
struct ReconcileArgs {
    /// The catalog file to reconcile. It must already exist: the object identity — the
    /// recorded checksum a rebuilt copy is proved against — lives there, and a catalog
    /// conjured empty at rebuild time would have nothing to rebuild *from*.
    #[arg(long, value_name = "FILE")]
    catalog: PathBuf,

    /// Report what would be rebuilt, changing nothing: no copy is written and no row is
    /// recorded (or damage-marked), so a dry run is safe to repeat.
    #[arg(long)]
    dry_run: bool,

    /// Leave a destination alone unless it has at least this much free space, on top of
    /// room for the rebuilt copy — the same floor `--min-free-gb` applies to a replicated
    /// sweep. A tier below it is refused and named before any byte is read.
    #[arg(long, value_name = "GB", default_value_t = 1.0)]
    min_free_gb: f64,

    /// Cap the total bytes one reconcile pass may read, as `512`, `64KiB` or `2GiB`.
    ///
    /// A rebuild reads its source twice — once to prove it matches the recorded checksum,
    /// once to copy it — so this bounds how much of a busy tier one pass touches. An object
    /// the budget cannot admit is deferred and reported, never silently skipped; because it
    /// is left unchanged, the next pass resumes at it. Omit for unlimited.
    #[arg(long, value_name = "SIZE", value_parser = parse_read_budget)]
    read_budget: Option<u64>,

    /// Print only problems.
    #[arg(short, long)]
    quiet: bool,
}

impl ReconcileArgs {
    fn min_free_bytes(&self) -> u64 {
        (self.min_free_gb.max(0.0) * 1_073_741_824.0) as u64
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Sweep(args)) => run_sweep(args),
        Some(Command::Audit(args)) => run_audit(args),

        Some(Command::Catalog(args)) => run_catalog(args),
        Some(Command::Explain(args)) => run_explain(args),
        Some(Command::Locate(args)) => run_locate(args),
        Some(Command::Restore(args)) => run_restore(args),
        Some(Command::Scrub(args)) => run_scrub(args),
        Some(Command::Reconcile(args)) => run_reconcile(args),
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
    if args.copies == 0 {
        eprintln!("just_cache: --copies must be at least 1");
        return ExitCode::from(EXIT_USAGE);
    }
    // A floor the invocation cannot satisfy is refused up front rather than half-met:
    // replicating onto fewer disks than asked, and then calling the object offloaded,
    // would be the tool lying about durability. Distinctness is by canonical root, so
    // `--dest /a --dest /a/` counts once.
    let distinct_dests = distinct_paths(&args.dest);
    if distinct_dests < args.copies {
        eprintln!(
            "just_cache: --copies {} needs {} distinct --dest roots, but only {} distinct \
             {} given; refusing rather than placing fewer copies than the floor",
            args.copies,
            args.copies,
            distinct_dests,
            if distinct_dests == 1 {
                "root is"
            } else {
                "roots are"
            }
        );
        return ExitCode::from(EXIT_USAGE);
    }
    if let Err(message) = validate_sweep(&watch, &args) {
        eprintln!("just_cache: {message}");
        return ExitCode::FAILURE;
    }
    // Tiers are loaded (and checked) before anything moves. A volatile tier in `--dest`
    // is a usage error, not a warning: §2.1 forbids a cache being a home, and the error
    // names the tier so the fix is obvious.
    let tiers = match load_tiers(args.tiers.as_deref(), &watch) {
        Ok(tiers) => tiers,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if let Err(message) = refuse_volatile_dests(tiers.as_ref(), &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }
    // The lifecycle policy is loaded and validated before anything moves. A rule that
    // names an unconfigured tier, a volatile cache overlay, an impossible from/to pair or
    // an unparseable duration is a usage error naming the rule — never a silent skip.
    let rules = match load_policy(args.policy.as_deref(), &watch) {
        Ok(rules) => rules,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let lifecycle = match build_lifecycle(&rules, &tiers) {
        Ok(lifecycle) => lifecycle,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let dest_tiers = destination_tiers(tiers.as_ref(), &args.dest);

    let policy = match args.policy() {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::FAILURE;
        }
    };
    let scope = match Scope::build(&args.include, &args.exclude, args.min_size, args.max_size) {
        Ok(scope) => scope,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };
    // A pattern that cannot match anything is reported before the sweep starts: an
    // `--exclude` that protects nothing is exactly when the user most needs to know.
    for warning in scope.warnings() {
        eprintln!("just_cache: warning: {warning}");
    }
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
    match journal::repair(&mut journal, &watch, &args.dest) {
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
            lifecycle: lifecycle.as_ref(),
            dest_tiers: &dest_tiers,
        };
        let report = sweep(&mut state, pass);
        // Compaction is what keeps the journal describing only what is still in flight:
        // finished moves are dropped, and any incomplete record is written back so the
        // next start reads a file the size of the work actually outstanding. A compaction
        // that fails — a symlink planted at the temp name, say — must be seen, not
        // swallowed: silently continuing would leave the journal describing the whole run
        // and never report the refusal.
        if let Err(err) = journal.compact() {
            eprintln!("just_cache: {err}");
            if args.once {
                return ExitCode::FAILURE;
            }
        }
        if args.once {
            return if report.failed() > 0 || report.under_replicated() > 0 {
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
    let tiers = match load_tiers(args.tiers.as_deref(), &args.watch) {
        Ok(tiers) => tiers,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if let Err(message) = refuse_volatile_dests(tiers.as_ref(), &args.dest) {
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

    // The copy floor is a per-tier property (#20), and a configured tier now carries its
    // own: `copies` in `tiers.toml` is the floor for that disk. Recorded once per tier,
    // not guessed from how many locations happen to exist — and the explicit `--copies`
    // below still wins when the operator gives one.
    if let Some(tiers) = &tiers {
        for dest in &args.dest {
            if let Some(tier) = tiers.tier_for_root(dest) {
                if let Err(err) = catalog.set_tier_floor(&tier_key(dest), tier.copies) {
                    eprintln!("just_cache: {err}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    if args.copies > 1 {
        for dest in &args.dest {
            if let Err(err) = catalog.set_tier_floor(&tier_key(dest), args.copies) {
                eprintln!("just_cache: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

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
    let tiers = match load_tiers(args.tiers.as_deref(), &args.watch) {
        Ok(tiers) => tiers,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if let Err(message) = refuse_volatile_dests(tiers.as_ref(), &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }

    // The catalog is the arbiter when it exists; the walk-based audit is the bootstrap and
    // the fallback. The file is never created here (invariant 9: only `catalog sync`
    // creates it), so `is_file` is checked before opening. An explicitly named catalog
    // that is missing is an error rather than a silent switch to a different source of
    // truth — the operator asked for one and quietly walking instead would answer from
    // another.
    let catalog_path = args
        .catalog
        .clone()
        .unwrap_or_else(|| catalog::Catalog::default_path(&args.watch));

    let report = if catalog_path.is_file() {
        let catalog = match catalog::Catalog::open(&catalog_path) {
            Ok(catalog) => catalog,
            Err(err) => {
                eprintln!("just_cache: {err}");
                return ExitCode::from(EXIT_USAGE);
            }
        };
        match audit::catalog_audit(&catalog, &args.watch, &args.dest) {
            Ok(report) => report,
            Err(err) => {
                eprintln!("just_cache: {err}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    } else if args.catalog.is_some() {
        eprintln!(
            "just_cache: --catalog {} does not exist (create it with `just_cache catalog sync`)",
            catalog_path.display()
        );
        return ExitCode::from(EXIT_USAGE);
    } else {
        match audit::audit_with_copies(&args.watch, &args.dest, args.copies) {
            Ok(report) => report,
            Err(err) => {
                eprintln!("just_cache: {err}");
                return ExitCode::from(EXIT_USAGE);
            }
        }
    };

    // The configured tiers ride along in the report so the summary names them, and a
    // finding can print a tier's name instead of its path. `None` changes nothing.
    //
    // The scrub-state counts ride along too, read once here and carried by the report so
    // the readable line and the JSON object are the same numbers from the same query.
    // Without a catalog there is nothing to count: `None` makes the JSON key `null` rather
    // than a zeroed object, which would read as "every copy verified" when no copy was
    // read at all. A failure to read the summary is reported and also leaves it absent.
    let scrub = if catalog_path.is_file() {
        match catalog::Catalog::open(&catalog_path) {
            Ok(catalog) => match catalog.scrub_summary() {
                Ok(summary) => Some(audit::ScrubSection {
                    locations: summary.locations,
                    verified: summary.verified,
                    never_scrubbed: summary.never_scrubbed,
                    damaged: summary.damaged,
                }),
                Err(err) => {
                    eprintln!("just_cache: {err}");
                    None
                }
            },
            Err(err) => {
                eprintln!("just_cache: {err}");
                None
            }
        }
    } else {
        None
    };
    let report = report.with_tiers(tiers).with_scrub(scrub);

    // Walk mode repairs (checksum-verify a duplicate, re-point a dangling link) are
    // unchanged. Catalog mode never mutates: it marks each finding for resync, because a
    // disagreement between the catalog and the tree is a human's call.
    let repairs = if args.repair {
        match &report.source {
            AuditSource::Walk => match audit::repair(&report) {
                Ok(repairs) => Some(repairs),
                Err(err) => {
                    eprintln!("just_cache: {err}");
                    return ExitCode::from(EXIT_USAGE);
                }
            },
            AuditSource::Catalog { .. } => Some(audit::catalog_repair(&report)),
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
            let marked = repairs
                .iter()
                .filter(|repair| matches!(repair.action, RepairAction::MarkedForResync { .. }))
                .count();
            if marked > 0 {
                // Catalog mode: say plainly that nothing was changed, so the operator does
                // not read a "repair" run as a fix.
                println!(
                    "repair: catalog mode marked {marked} finding(s) for resync; nothing on disk or in the catalog was changed"
                );
            } else {
                println!(
                    "repair: {repaired} of {} finding(s) resolved, {refused} refused, {not_attempted} not attempted",
                    repairs.len()
                );
            }
            for repair in repairs.iter().filter(|repair| !repair.repaired()) {
                println!(
                    "  not repaired: {} ({})",
                    repair.path.display(),
                    repair.kind.as_str()
                );
            }
        }
    }

    // Scrub state is a separate signal from the structural verdicts above, so it changes
    // neither the findings nor the exit code: a "never scrubbed" copy is a gap in
    // verification, not an inconsistency between the tree and its tiers. Under `--json` it
    // is the report's `scrub` object, part of the document printed above; on the terminal
    // the same counts print here.
    if !args.json {
        if let Some(scrub) = &report.scrub {
            for line in scrub.summary_lines() {
                println!("{line}");
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
    let tiers = match load_tiers(args.tiers.as_deref(), &args.watch) {
        Ok(tiers) => tiers,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if let Err(message) = refuse_volatile_dests(tiers.as_ref(), &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }
    // The same policy/tier pairing a sweep uses, so `explain` answers with the rules that
    // would actually decide — a different policy here would explain a different tool.
    let rules = match load_policy(args.policy.as_deref(), &args.watch) {
        Ok(rules) => rules,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let lifecycle = match build_lifecycle(&rules, &tiers) {
        Ok(lifecycle) => lifecycle,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let scope = match Scope::build(&args.include, &args.exclude, args.min_size, args.max_size) {
        Ok(scope) => scope,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    for warning in scope.warnings() {
        eprintln!("just_cache: warning: {warning}");
    }
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

    let policy = match args.policy() {
        Ok(policy) => policy,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // Which catalog to consult, if any. An explicit `--catalog` must exist: naming a file
    // that is not there is a bad invocation, not a quiet fallback. The default beside the
    // watch root is consulted only when it is already present — a plain sweep must never
    // create a catalog (invariant 9), and neither must `explain`.
    let catalog_path = match &args.catalog {
        Some(path) => {
            if !path.is_file() {
                eprintln!(
                    "just_cache: --catalog {} does not exist; run `catalog sync` first",
                    path.display()
                );
                return ExitCode::from(EXIT_USAGE);
            }
            Some(path.clone())
        }
        None => {
            let default = catalog::Catalog::default_path(&args.watch);
            default.is_file().then_some(default)
        }
    };
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
        catalog: catalog_path,
        lifecycle: lifecycle.as_ref(),
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

/// Where an object lives. The catalog is the source of truth (§3), so this asks it and
/// never walks a tree — which is what keeps the answer available when a tier is unmounted.
/// Exit codes are the contract: `0` found, `1` nothing found, `2` a bad invocation or an
/// unusable catalog.
fn run_locate(args: LocateArgs) -> ExitCode {
    // `locate` names no watch root, so the default config is `tiers.toml` beside the
    // catalog — which is beside the watch root for the default catalog path. An explicit
    // `--tiers` must exist and parse; the default is consulted only when already there.
    let base = args
        .catalog
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tiers = match load_tiers(args.tiers.as_deref(), base) {
        Ok(tiers) => tiers,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let request = LocateRequest {
        query: &args.query,
        catalog: &args.catalog,
        tiers: tiers.as_ref(),
    };
    let report = match locate::locate(&request) {
        Ok(report) => report,
        // A missing file or a catalog that cannot be read is a bad invocation, not
        // "nothing found": exit 1 is reserved for a query that genuinely matched nothing,
        // so a script cannot mistake a broken catalog for an empty one.
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    if args.json {
        println!("{}", report.to_json());
    } else {
        for line in report.summary_lines() {
            println!("{line}");
        }
    }
    ExitCode::from(report.exit_code())
}

fn run_restore(args: RestoreArgs) -> ExitCode {
    if let Err(message) = validate_paths(&args.watch, &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }
    let tiers = match load_tiers(args.tiers.as_deref(), &args.watch) {
        Ok(tiers) => tiers,
        Err(message) => {
            eprintln!("just_cache: {message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    if let Err(message) = refuse_volatile_dests(tiers.as_ref(), &args.dest) {
        eprintln!("just_cache: {message}");
        return ExitCode::from(EXIT_USAGE);
    }

    // The same open-if-present rule `audit` and `explain` use. An explicitly named
    // catalog must exist: naming a file that is not there is a bad invocation, not a
    // quiet fallback. The default beside the watch root is consulted only when it is
    // already present — restore must never create a catalog (invariant 9), and one
    // conjured empty here would answer "not in the catalog" for a tree that is fine.
    let catalog_path = match &args.catalog {
        Some(path) => {
            if !path.is_file() {
                eprintln!(
                    "just_cache: --catalog {} does not exist (create it with `just_cache catalog sync`)",
                    path.display()
                );
                return ExitCode::from(EXIT_USAGE);
            }
            Some(path.clone())
        }
        None => {
            let default = catalog::Catalog::default_path(&args.watch);
            default.is_file().then_some(default)
        }
    };
    let catalog = match &catalog_path {
        Some(path) => match catalog::Catalog::open(path) {
            Ok(catalog) => Some(catalog),
            Err(err) => {
                eprintln!("just_cache: {err}");
                return ExitCode::from(EXIT_USAGE);
            }
        },
        None => None,
    };

    let request = RestoreRequest {
        path: &args.path,
        watch: &args.watch,
        dests: &args.dest,
        remove_copy: args.remove_copy,
        catalog: catalog.as_ref(),
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
        Err(err @ RestoreError::OutsideWatch { .. })
        | Err(err @ RestoreError::ParentComponent { .. }) => {
            eprintln!("just_cache: {err}");
            ExitCode::from(EXIT_USAGE)
        }
        Err(err) => {
            eprintln!("just_cache: {err}");
            ExitCode::from(EXIT_FINDINGS)
        }
    }
}

/// Read every stored copy back and verify it against the catalog.
///
/// Exit contract: `0` when every copy verified or was repaired, `1` when corruption,
/// missing bytes or an unmounted tier were seen, `2` on a bad invocation. Unlike
/// `audit --repair` (which goes quiet once a finding is resolved), a scrub that *repaired*
/// something still exits `1`: bitrot is evidence about the tier, and the operator has to
/// be able to see that it happened even though it was fixed.
fn run_scrub(args: ScrubArgs) -> ExitCode {
    if !args.catalog.is_file() {
        eprintln!(
            "just_cache: --catalog {} is not an existing file",
            args.catalog.display()
        );
        return ExitCode::from(EXIT_USAGE);
    }
    if args.rate == Some(0) {
        eprintln!("just_cache: --rate must be at least 1 KiB/s");
        return ExitCode::from(EXIT_USAGE);
    }

    let catalog = match catalog::Catalog::open(&args.catalog) {
        Ok(catalog) => catalog,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };
    let request = ScrubRequest {
        catalog: &catalog,
        rate_kib_per_sec: args.rate,
        dry_run: args.dry_run,
    };
    let report = match scrub::scrub(&request) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };

    if args.quiet {
        for line in report.finding_lines() {
            println!("{line}");
        }
    } else {
        for line in report.summary_lines() {
            println!("{line}");
        }
    }

    if report.has_findings() {
        ExitCode::from(EXIT_FINDINGS)
    } else {
        ExitCode::SUCCESS
    }
}

/// Rebuild the copies a re-added disk is missing, from a sibling that verifies.
///
/// Exit contract: `0` when every recorded floor was already met (nothing to rebuild),
/// `1` when anything was rebuilt/adopted or could not be (a disk being out is evidence the
/// operator has to see, exactly as a scrub that repaired something still exits `1`), `2`
/// on a bad invocation — a missing catalog file is a usage error, because the recorded
/// checksum a rebuild is proved against has to come from somewhere.
fn run_reconcile(args: ReconcileArgs) -> ExitCode {
    if !args.catalog.is_file() {
        eprintln!(
            "just_cache: --catalog {} does not exist (create it with `just_cache catalog sync`)",
            args.catalog.display()
        );
        return ExitCode::from(EXIT_USAGE);
    }

    let catalog = match catalog::Catalog::open(&args.catalog) {
        Ok(catalog) => catalog,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };
    let request = reconcile::ReconcileRequest {
        catalog: &catalog,
        dry_run: args.dry_run,
        min_free: args.min_free_bytes(),
        read_budget: args.read_budget,
    };
    let report = match reconcile::reconcile(&request) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("just_cache: {err}");
            return ExitCode::FAILURE;
        }
    };

    if args.quiet {
        for line in report.finding_lines() {
            println!("{line}");
        }
    } else {
        for line in report.summary_lines() {
            println!("{line}");
        }
    }

    if report.has_findings() {
        ExitCode::from(EXIT_FINDINGS)
    } else {
        ExitCode::SUCCESS
    }
}

fn validate_sweep(watch: &Path, args: &SweepArgs) -> Result<(), String> {
    validate_paths(watch, &args.dest)?;
    min_idle_duration(args.min_idle_days)?;
    if args.interval == 0 && !args.once {
        return Err("--interval must be at least 1 second".to_string());
    }
    Ok(())
}

/// The largest `--min-idle-days` that makes sense for a tiering tool. Anything past this
/// (a millennium) is a typo rather than a policy, and a finite value past `Duration`'s
/// range would otherwise reach the conversion below and abort the run instead of being
/// refused like every other bad input.
const MAX_MIN_IDLE_DAYS: f64 = 365_000.0;

/// Convert `--min-idle-days` into the `Duration` the policy gates on, refusing every value
/// `f64` will happily parse but the gate cannot honour.
///
/// `NaN` is the dangerous one. `NaN < 0.0` is false, so a bare "not negative" check lets it
/// through, and `NaN.max(0.0)` is `0.0`, so the idle gate collapses to `Duration::ZERO` —
/// the flag meant to protect files still in use would silently disable the protection.
/// `inf`, and a finite value whose seconds overflow `Duration`, would panic inside
/// `from_secs_f64`; `try_from_secs_f64` keeps that a reported error instead.
fn min_idle_duration(days: f64) -> Result<Duration, String> {
    if !days.is_finite() {
        return Err(format!(
            "--min-idle-days must be a finite number of days, got {days}"
        ));
    }
    if days < 0.0 {
        return Err("--min-idle-days cannot be negative".to_string());
    }
    if days > MAX_MIN_IDLE_DAYS {
        return Err(format!(
            "--min-idle-days {days} is above the {MAX_MIN_IDLE_DAYS}-day maximum"
        ));
    }
    Duration::try_from_secs_f64(days * 86_400.0)
        .map_err(|err| format!("--min-idle-days {days} is not a usable duration: {err}"))
}

/// Number of distinct destination roots, keyed on canonical path so two names for one
/// disk do not masquerade as two disks.
fn distinct_paths(dests: &[PathBuf]) -> usize {
    let mut seen = HashSet::new();
    for dest in dests {
        let key = dest.canonicalize().unwrap_or_else(|_| dest.clone());
        seen.insert(key);
    }
    seen.len()
}

/// The tier key for a destination root: its canonical path, matching what the catalog's
/// observe pass uses so a floor and a location name the same disk.
fn tier_key(dest: &Path) -> String {
    dest.canonicalize()
        .unwrap_or_else(|_| dest.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Load the tier config for an invocation: `--tiers` when named, otherwise `tiers.toml`
/// beside `beside` when it is already there.
///
/// An explicitly named file that is missing or malformed is an error, never a silent
/// fallback to path-as-tier-name: the operator asked for *that* file, and answering from
/// paths instead would answer a different question. The default is only read when it is
/// present — a command must never create a config it was not asked to (invariant 9).
fn load_tiers(explicit: Option<&Path>, beside: &Path) -> Result<Option<TierSet>, String> {
    match explicit {
        Some(path) => TierSet::load(path).map(Some).map_err(|err| err.to_string()),
        None => TierSet::load_beside(beside).map_err(|err| err.to_string()),
    }
}

/// Load the lifecycle policy for an invocation: `--policy` when named, otherwise
/// `policy.toml` beside `beside` when it is already there — the same open-if-present rule
/// `tiers.toml` uses, so a command never creates a config it was not asked to.
fn load_policy(explicit: Option<&Path>, beside: &Path) -> Result<Option<RuleSet>, String> {
    match explicit {
        Some(path) => RuleSet::load(path).map(Some).map_err(|err| err.to_string()),
        None => RuleSet::load_beside(beside).map_err(|err| err.to_string()),
    }
}

/// Build the lifecycle engine from a policy and a tier config, refusing a rule that names
/// a tier the config does not describe.
///
/// A `policy.toml` with rules but no `tiers.toml` is a usage error: a rule governs tiers,
/// and every tier it names is therefore unconfigured. A policy with no rules is a no-op,
/// not an error — a file that should not have been created answers "no rule matches".
fn build_lifecycle<'a>(
    rules: &'a Option<RuleSet>,
    tiers: &'a Option<TierSet>,
) -> Result<Option<Lifecycle<'a>>, String> {
    let Some(rules) = rules else {
        return Ok(None);
    };
    let Some(tiers) = tiers else {
        if rules.is_empty() {
            return Ok(None);
        }
        return Err(format!(
            "--policy {} has rules but no tier config; add a tiers.toml beside the watch \
             root (or pass --tiers), because a rule names tiers",
            rules.path().display()
        ));
    };
    rules.validate_tiers(tiers).map_err(|err| err.to_string())?;
    Ok(Some(Lifecycle::new(rules, tiers)))
}

/// The `(configured tier name, destination root)` pairs for the destinations given. A
/// destination the config does not name gets no tier, so no rule can target it.
fn destination_tiers(tiers: Option<&TierSet>, dests: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let Some(tiers) = tiers else {
        return Vec::new();
    };
    dests
        .iter()
        .filter_map(|dest| {
            tiers
                .tier_for_root(dest)
                .map(|tier| (tier.name.clone(), dest.clone()))
        })
        .collect()
}

/// Refuse a destination root the config marks volatile.
///
/// §2.1 is a hard rule: a volatile tier is a promotion target, never the place a file
/// lives. A `--dest` is exactly the place a file lives, so a sweep told to move bytes into
/// a volatile tier is told to make a cache the home — refused up front rather than obeyed.
fn refuse_volatile_dests(tiers: Option<&TierSet>, dests: &[PathBuf]) -> Result<(), String> {
    let Some(tiers) = tiers else {
        return Ok(());
    };
    for dest in dests {
        if let Some(tier) = tiers.tier_for_root(dest) {
            if !tier.is_home() {
                return Err(format!(
                    "--dest {} is the volatile tier `{}` ({}): a volatile tier is a mirror, \
                     never a home (docs/design.md §2.1)",
                    dest.display(),
                    tier.name,
                    tier.path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Shared checks for both subcommands: the watched tree exists, and each destination is
/// a real directory that is neither the watched tree nor nested inside it.
///
/// Equality alone is not enough: a `--dest` *beneath* the watch (`--watch /data --dest
/// /data/cold`) is not the watched tree, so it used to pass, and the sweep would discover
/// and re-move the copies it had just written deeper on every pass — each pass tiering the
/// previous pass's output. The reverse nesting (`--watch` under a `--dest` root) is the same
/// hazard seen from the other side, so both directions are refused. `Path::starts_with`
/// compares whole components, so `/data` matches `/data/cold` but not `/database`.
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
        // Both roots exist (checked above), so these resolve; the literal fallback keeps a
        // permission oddity from turning a comparison into a false mismatch.
        let canonical_dest = dest.canonicalize().unwrap_or_else(|_| dest.clone());
        if let Some(watched) = watched.as_ref() {
            if &canonical_dest == watched {
                return Err(format!(
                    "--dest {} is the watched directory, which would do nothing",
                    dest.display()
                ));
            }
            if canonical_dest.starts_with(watched) {
                return Err(format!(
                    "--dest {} is inside the watched tree {}; a destination under the watch \
                     would re-tier the tool's own copies on every pass",
                    dest.display(),
                    watch.display()
                ));
            }
            if watched.starts_with(&canonical_dest) {
                return Err(format!(
                    "--watch {} is inside the --dest root {}; the watched tree must not \
                     contain the destination it tiers into",
                    watch.display(),
                    dest.display()
                ));
            }
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
    /// The lifecycle rules, when `policy.toml` governs this sweep.
    lifecycle: Option<&'a Lifecycle<'a>>,
    /// `(tier name, destination root)` pairs, so a rule's `to` tier can be routed to.
    dest_tiers: &'a [(String, PathBuf)],
}

fn sweep(state: &mut Sweep<'_>, pass: u64) -> MigrationReport {
    let watch = state.watch;
    let dests = state.dests;
    let args = state.args;
    let policy = state.policy;
    let scope = state.scope;
    let lifecycle = state.lifecycle;
    let dest_tiers = state.dest_tiers;
    let tracker: &mut UsageTracker = state.tracker;
    let journal: &mut Journal = state.journal;
    let now = SystemTime::now();

    // The sweep snapshot is a cheap prefilter, not the final word: it keeps the walk from
    // selecting files that were already open without a process-table scan per file in the
    // tree. A file opened *after* this snapshot is caught by the mover's per-candidate
    // re-check (`Guards::recheck`), which re-scans /proc immediately before bytes move.
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

    if args.copies > 1 {
        // Replication path: every candidate is tried against the destinations in order
        // until `copies` copies have verified, rather than filling one tier before moving
        // to the next. Nothing here can leave an object below its floor and still remove
        // the source — `migrate_replicated` refuses to retire a file it could not copy.
        let mut context = file_movement::MoveContext {
            policy,
            scope,
            guards: &guards,
            journal,
            lifecycle,
            dest_tiers,
        };
        let replicated = file_movement::migrate_replicated(
            &pending,
            tracker,
            &mut context,
            now,
            dests,
            args.copies,
            min_free,
        );
        report.records.extend(replicated.records);
        report.replication = replicated.replication;
        pending.clear();
    } else if lifecycle.is_some() {
        // Rule path: a `policy.toml` decides both whether a file is cold and which tier it
        // belongs on, so candidates are not poured into the fastest disk with room — each
        // is routed to the tier its rule names.
        let mut context = file_movement::MoveContext {
            policy,
            scope,
            guards: &guards,
            journal,
            lifecycle,
            dest_tiers,
        };
        let by_rules = file_movement::migrate_least_used(
            &pending,
            tracker,
            &mut context,
            now,
            |candidate: &file_movement::Candidate| {
                let dest = candidate.tier.as_deref().and_then(|name| {
                    dest_tiers
                        .iter()
                        .find(|(tier, _)| tier == name)
                        .map(|(_, path)| path)
                });
                match dest {
                    Some(dest) => Ok(disk_management::destination_with_room(
                        dest,
                        candidate.entry.allocated,
                        min_free,
                    )),
                    None => Ok(None),
                }
            },
        );
        report.records.extend(by_rules.records);
        report.replication.extend(by_rules.replication);
        pending.clear();
    } else {
        for dest in dests {
            // Both sides matter: the guards are live state (a descriptor can open at any
            // moment), and the room check measures allocated bytes, since the copy
            // preserves holes and `size` would refuse moves the tier can afford.
            let mut context = file_movement::MoveContext {
                policy,
                scope,
                guards: &guards,
                journal,
                lifecycle: None,
                dest_tiers,
            };
            let tier = file_movement::migrate_least_used(
                &pending,
                tracker,
                &mut context,
                now,
                |candidate: &file_movement::Candidate| {
                    Ok(disk_management::destination_with_room(
                        dest,
                        candidate.entry.allocated,
                        min_free,
                    ))
                },
            );

            // Whatever this tier took (or would take) is off the table for slower disks.
            let handled: HashSet<PathBuf> = tier.migrated_paths().into_iter().collect();
            let waiting = tier.waiting_for_room();
            pending.retain(|entry| !handled.contains(&entry.path));
            report.records.extend(tier.records);
            report.replication.extend(tier.replication);

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
    }

    // Record the replicas in the catalog, when one already exists. The sweep may not
    // create one (invariant 9 — that is `catalog sync`'s job), so this is deliberately an
    // open-if-present: with no catalog there is nothing to record into, and the copies
    // are still real and still counted by the next sync. A copy is only recorded once a
    // digest vouched for it, which is why `record_replicas` filters on `is_valid`.
    if args.copies > 1 {
        if let Ok(Some(existing_catalog)) =
            catalog::Catalog::open_existing(catalog::Catalog::default_path(watch))
        {
            record_replicas(&existing_catalog, &report.replication);
        }
    }

    // Record which rule decided each transition (§5). A sweep never creates a catalog
    // (invariant 9), so it updates only an existing catalog row and warns if a move cannot
    // be persisted. The per-file report still names the rule; dry-run records nothing,
    // because nothing transitioned.
    if lifecycle.is_some() {
        match catalog::Catalog::open_existing(catalog::Catalog::default_path(watch)) {
            Ok(Some(existing_catalog)) => record_lifecycle_rules(&existing_catalog, watch, &report),
            Ok(None)
                if report.records.iter().any(|record| {
                    matches!(
                        record.outcome,
                        FileOutcome::Moved | FileOutcome::LinkedExisting
                    ) && record.rule.is_some()
                }) =>
            {
                eprintln!(
                    "just_cache: policy-governed moves completed without an existing catalog; \
                     the sweep does not create one, so run `catalog sync` to persist lifecycle.rule"
                );
            }
            Ok(None) => {}
            Err(err) => eprintln!(
                "just_cache: policy-governed moves completed, but could not open the existing \
                 catalog to record lifecycle.rule: {err}"
            ),
        }
    }

    if args.quiet {
        for record in &report.records {
            if matches!(
                record.outcome,
                FileOutcome::Failed(_) | FileOutcome::UnderReplicated { .. }
            ) {
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
            "pass {pass}: {} files scanned, {} tracked, {} moved, {} linked, {} waiting for room, {} in use ({} open files seen), {} skipped ({} outside scope or size), {} under-replicated, {} failed, {} onto the cold tiers",
            entries.len(),
            tracker.tracked_paths(),
            report.moved(),
            report.linked_existing(),
            report.waiting_for_room(),
            report.in_use(),
            guards.open_files().len(),
            report.excluded(),
            report.count(|outcome| matches!(outcome, FileOutcome::Skipped(_))),
            report.under_replicated(),
            report.failed(),
            scope::human_bytes(report.bytes_moved()),
        );
        if policy.dry_run {
            println!("dry run: nothing was moved");
        }
    }

    report
}

/// Decode a hex object id back to the bytes the catalog's `object.id` column holds.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

/// Write the rule that decided each transition into an existing catalog (§5). A record
/// whose path the catalog has not ingested yet is skipped: a sweep must not invent a name,
/// and the next `catalog sync` brings it in.
fn record_lifecycle_rules(catalog: &catalog::Catalog, watch: &Path, report: &MigrationReport) {
    for record in &report.records {
        let Some(rule) = record.rule.as_deref() else {
            continue;
        };
        // Only real transitions: a dry run plans and moves nothing, so it records nothing.
        if !matches!(
            record.outcome,
            FileOutcome::Moved | FileOutcome::LinkedExisting
        ) {
            continue;
        }
        let Ok(relative) = record.path.strip_prefix(watch) else {
            continue;
        };
        match catalog.record_lifecycle_rule(&relative.to_string_lossy(), rule) {
            Ok(true) => {}
            Ok(false) => eprintln!(
                "just_cache: moved {} by rule `{rule}`, but it is not in the catalog; run \
                 `catalog sync` before the next policy sweep to record lifecycle.rule",
                record.path.display()
            ),
            Err(err) => eprintln!(
                "just_cache: moved {} by rule `{rule}`, but could not record lifecycle.rule: {err}",
                record.path.display()
            ),
        }
    }
}

/// Write the verified copies of one sweep into an existing catalog, if the objects are
/// already known. A copy the mover could not verify (`is_valid` false) is not recorded:
/// it is unknown, and the catalog must not be told it is good.
fn record_replicas(catalog: &catalog::Catalog, details: &[file_movement::ReplicationDetail]) {
    for detail in details {
        let Some(object) = detail.object.as_deref().and_then(decode_hex) else {
            continue;
        };
        for placement in &detail.placements {
            if !placement.is_valid() {
                continue;
            }
            let tier = placement
                .dest_root
                .canonicalize()
                .unwrap_or_else(|_| placement.dest_root.clone());
            let key = placement
                .path
                .strip_prefix(&placement.dest_root)
                .unwrap_or(&placement.path);
            let _ = catalog.record_replica(
                &object,
                &tier.to_string_lossy(),
                &key.to_string_lossy(),
                true,
                Some(&object),
            );
        }
    }
}
