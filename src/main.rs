//! `just_cache` — move cold, rarely used files onto slower disks and leave a symlink
//! behind so every path keeps working.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use clap::Parser;

use just_cache::disk_management;
use just_cache::file_movement::{self, FileOutcome, MigrationReport, Policy, UsageTracker};
use just_cache::opened::{Coverage, Guards, OpenFiles};
use just_cache::scope::{self, Scope};

fn parse_size_arg(text: &str) -> Result<u64, String> {
    scope::parse_size(text).map_err(|err| err.to_string())
}

/// Move cold files from a watched tree onto slower disks, leaving symlinks behind.
#[derive(Debug, Parser)]
#[command(name = "just_cache", version, about, long_about = None)]
struct Cli {
    /// Directory to watch. Files under it (recursively) are considered for migration.
    #[arg(long, value_name = "DIR")]
    watch: PathBuf,

    /// Cold-storage root, fastest tier first. Repeat for each slower disk.
    ///
    /// Each destination must already exist: a missing path is reported as an error
    /// rather than created, so an unmounted disk can never be silently replaced by a
    /// plain directory on the wrong filesystem.
    #[arg(long, value_name = "DIR", required = true, num_args = 1..)]
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

impl Cli {
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

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(message) = validate(&cli) {
        eprintln!("just_cache: {message}");
        return ExitCode::FAILURE;
    }

    let policy = cli.policy();
    let scope = match Scope::build(&cli.include, &cli.exclude, cli.min_size, cli.max_size) {
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

    let mut tracker = UsageTracker::new();
    let mut pass = 0u64;

    loop {
        pass += 1;
        let report = sweep(&cli, &policy, &scope, &mut tracker, pass);
        if cli.once {
            return if report.failed() > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
        std::thread::sleep(Duration::from_secs(cli.interval.max(1)));
    }
}

fn validate(cli: &Cli) -> Result<(), String> {
    if !cli.watch.is_dir() {
        return Err(format!(
            "--watch {} is not a directory",
            cli.watch.display()
        ));
    }
    let watched = cli.watch.canonicalize().ok();
    for dest in &cli.dest {
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
    if cli.min_idle_days < 0.0 {
        return Err("--min-idle-days cannot be negative".to_string());
    }
    if cli.interval == 0 && !cli.once {
        return Err("--interval must be at least 1 second".to_string());
    }
    Ok(())
}

/// One sweep over the watched tree: fill each cold tier in turn, fastest disk first.
fn sweep(
    cli: &Cli,
    policy: &Policy,
    scope: &Scope,
    tracker: &mut UsageTracker,
    pass: u64,
) -> MigrationReport {
    let now = SystemTime::now();

    // One snapshot per sweep, not one per candidate: a process-table scan per file would
    // cost more than the copy it is protecting. The guard is deliberately taken *before*
    // the walk so that a file opened mid-sweep is caught by the re-check in the mover.
    let open_files = OpenFiles::snapshot();
    let coverage_note = open_files.coverage_note();
    let guards = Guards::new(open_files, !cli.allow_hardlinked);
    match guards.open_files().coverage() {
        // Cannot check at all: say so rather than pretending the guard exists.
        Coverage::Unsupported => {
            if let Some(note) = &coverage_note {
                eprintln!("just_cache: {note}");
            }
        }
        Coverage::OwnProcessesOnly => {
            if cli.verbose > 0 {
                if let Some(note) = &coverage_note {
                    println!("  {note}");
                }
            }
        }
        Coverage::Complete => {}
    }

    let entries = match disk_management::list_files_recursive(&cli.watch) {
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
    let min_free = cli.min_free_bytes();
    let mut pending = entries.clone();

    for dest in &cli.dest {
        // Both sides matter: the guards are live state (a descriptor can open at any
        // moment), and the room check measures allocated bytes, since the copy preserves
        // holes and `size` would refuse moves the tier can afford.
        let tier = file_movement::migrate_least_used(
            &pending,
            tracker,
            policy,
            scope,
            &guards,
            now,
            |entry| Ok(destination_with_room(dest, entry.allocated, min_free)),
        );

        // Whatever this tier took (or would take) is off the table for slower disks.
        let handled: HashSet<PathBuf> = tier.migrated_paths().into_iter().collect();
        let waiting = tier.waiting_for_room();
        pending.retain(|entry| !handled.contains(&entry.path));
        report.records.extend(tier.records);

        if cli.verbose > 0 && waiting > 0 {
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

    if cli.quiet {
        for record in &report.records {
            if matches!(record.outcome, FileOutcome::Failed(_)) {
                file_movement::log_file_movement(record);
            }
        }
    } else if cli.verbose > 0 {
        for line in report.lines() {
            println!("  {line}");
        }
    } else {
        for record in &report.records {
            file_movement::log_file_movement(record);
        }
    }

    if !cli.quiet {
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

/// The destination to use for a file, or `None` when the tier has no room left.
///
/// The floor keeps a sweep from filling the disk it is moving onto; a tier with no
/// room is not an error, the file simply waits for the next sweep.
/// Whether this tier can hold the bytes about to be written to it.
///
/// `needed` is the file's *allocated* size, not its apparent length: the copy preserves
/// holes, so a 1 GiB sparse file needs kilobytes of room, and measuring it by `len()` would
/// refuse a move the tier can easily afford. The risk of being wrong the other way (a
/// destination that cannot preserve holes, where the fallback copy writes every byte) is
/// covered downstream: a copy that runs out of space removes its partial file, reports a
/// failure and leaves the source untouched, so the worst case is a failed move rather than
/// a lost file.
fn destination_with_room(dest: &Path, needed: u64, min_free: u64) -> Option<PathBuf> {
    match disk_management::available_space(dest) {
        Some(free) if free >= min_free.saturating_add(needed) => Some(dest.to_path_buf()),
        Some(_) => None,
        // Free space unknown on this filesystem: try the tier instead of stalling.
        None => Some(dest.to_path_buf()),
    }
}
