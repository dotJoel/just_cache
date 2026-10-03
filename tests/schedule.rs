//! End-to-end behaviour of `just_cache schedule` — the configurable cadence for the scrub
//! and reconcile passes, driven through the binary for the user-visible contract.
//!
//! The cases are the ones the issue calls out:
//!
//! * with nothing configured, nothing runs (today's behaviour is the default);
//! * the next planned run is visible, and it is computed from injected time rather than by
//!   sleeping out a real schedule window;
//! * a scheduled pass that finds something reports once with the house exit code, and a
//!   clean pass is quiet;
//! * a scheduled pass respects the configured `--rate` (it truly paces the read);
//! * a pass below the free-space floor is held back, reported, and left due.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use just_cache::catalog::Catalog;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit; stdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

/// A payload that is non-zero and long enough that a flipped byte changes the digest.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251 + 1) as u8).collect()
}

fn sync(watch: &Path, dest: &Path, catalog: &Path) -> Output {
    bin()
        .args(["catalog", "sync", "--watch"])
        .arg(watch)
        .arg("--dest")
        .arg(dest)
        .arg("--catalog")
        .arg(catalog)
        .output()
        .expect("just_cache runs")
}

fn schedule(catalog: &Path, extra: &[&str]) -> Output {
    let mut command = bin();
    command.arg("schedule").arg("--catalog").arg(catalog);
    command.args(extra);
    command.output().expect("just_cache runs")
}

/// Flip bytes in place without changing the length, so only a checksum can detect it.
fn corrupt_in_place(path: &Path) {
    let mut bytes = fs::read(path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    bytes[mid + 1] ^= 0x0f;
    fs::write(path, &bytes).unwrap();
}

/// The state file the schedule keeps beside the catalog.
fn state_path(catalog: &Path) -> PathBuf {
    catalog.parent().unwrap().join(".just_cache-schedule.state")
}

/// A hot file and its cold mirror, synced, so a scrub has one object with two locations.
fn tree(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let hot = tmp.join("hot");
    let cold = tmp.join("cold");
    let catalog = tmp.join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), payload(8 * 1024)).unwrap();
    fs::write(cold.join("a.bin"), payload(8 * 1024)).unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    (hot, cold, catalog)
}

fn write_schedule(catalog: &Path, text: &str) {
    fs::write(catalog.parent().unwrap().join("schedule.toml"), text).unwrap();
}

#[test]
fn with_nothing_configured_nothing_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = tree(tmp.path());

    let status = schedule(&catalog, &[]);
    assert_exit(&status, 0);
    assert!(
        stdout(&status).contains("no schedule configured"),
        "{}",
        stdout(&status)
    );

    // `--run` with no schedule is not an error and must not create a state file: nothing
    // was configured, so nothing runs.
    let run = schedule(&catalog, &["--run"]);
    assert_exit(&run, 0);
    assert!(
        stdout(&run).contains("no schedule configured"),
        "{}",
        stdout(&run)
    );
    assert!(
        !state_path(&catalog).exists(),
        "an unconfigured schedule must create nothing"
    );

    // An explicitly named config that is not there is a bad invocation, never a silent
    // fallback to "no schedule".
    let missing = schedule(&catalog, &["--config", "/nonexistent/schedule.toml"]);
    assert_exit(&missing, 2);
    assert!(
        stderr(&missing).contains("does not exist"),
        "{}",
        stderr(&missing)
    );
}

#[test]
fn the_next_planned_run_is_visible_and_uses_injected_time() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = tree(tmp.path());
    write_schedule(
        &catalog,
        "[scrub]\nevery = \"1h\"\nrate = 4096\n\n[reconcile]\nevery = \"1h\"\n",
    );

    // Never run: due now, with the configured budget named.
    let status = schedule(&catalog, &["--now", "1000000000"]);
    assert_exit(&status, 0);
    let text = stdout(&status);
    assert!(
        text.contains("scrub: every 1h — due now (never run)"),
        "{text}"
    );
    assert!(text.contains("rate 4096 KiB/s"), "{text}");
    assert!(
        text.contains("reconcile: every 1h — due now (never run)"),
        "{text}"
    );

    // Run the due passes at a fixed instant; both are clean, so the output is quiet.
    let run = schedule(&catalog, &["--run", "--now", "1000000000"]);
    assert_exit(&run, 0);
    assert!(
        stdout(&run).trim().is_empty(),
        "a clean pass must be quiet, got:\n{}",
        stdout(&run)
    );

    // A half hour later neither pass is due, and the next run is exact: last run 1e9 plus
    // the 1h cadence is 1000003600 (2001-09-09T02:46:40Z).
    let later = schedule(&catalog, &["--now", "1000001800"]);
    assert_exit(&later, 0);
    let text = stdout(&later);
    assert!(
        text.contains("scrub: every 1h — next run 2001-09-09T02:46:40Z"),
        "{text}"
    );

    // Two hours on, it is due again.
    let due = schedule(&catalog, &["--now", "1000007200"]);
    assert_exit(&due, 0);
    assert!(
        stdout(&due).contains("due now (last run 2001-09-09T01:46:40Z)"),
        "{}",
        stdout(&due)
    );
}

#[test]
fn a_pass_that_finds_something_reports_once_and_exits_one() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, cold, catalog) = tree(tmp.path());
    corrupt_in_place(&cold.join("a.bin"));
    write_schedule(&catalog, "[scrub]\nevery = \"1h\"\n");

    let first = schedule(&catalog, &["--run"]);
    // A repair is still a finding: bitrot happened, and cron has to see it once.
    assert_exit(&first, 1);
    let text = stdout(&first);
    assert!(text.contains("repaired"), "the repair is reported:\n{text}");

    // The run was recorded, so the immediate next check has nothing due and is quiet.
    let second = schedule(&catalog, &["--run"]);
    assert_exit(&second, 0);
    assert!(
        stdout(&second).trim().is_empty(),
        "a pass that is not due must be quiet, got:\n{}",
        stdout(&second)
    );
}

#[test]
fn a_scheduled_scrub_respects_the_configured_rate() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    // 256 KiB at 512 KiB/s is about half a second of budget. The pass must carry the rate
    // the config names, or a cron-driven scrub would silently be the unthrottled command.
    fs::write(hot.join("a.bin"), payload(256 * 1024)).unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    write_schedule(&catalog, "[scrub]\nevery = \"1h\"\nrate = 512\n");

    let start = Instant::now();
    let run = schedule(&catalog, &["--run"]);
    let elapsed = start.elapsed();
    assert_exit(&run, 0);
    assert!(
        elapsed >= Duration::from_millis(350),
        "a 512 KiB/s budget must pace ~0.5s for 256 KiB, took {elapsed:?}"
    );
}

#[test]
fn a_pass_below_the_free_space_floor_is_held_back_and_stays_due() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = tree(tmp.path());
    // No real filesystem has 100 TiB free, so the floor deterministically holds the pass.
    write_schedule(&catalog, "[scrub]\nevery = \"1h\"\nmin_free_gb = 100000\n");

    let held = schedule(&catalog, &["--run"]);
    assert_exit(&held, 1);
    let text = stdout(&held);
    assert!(text.contains("held back"), "{text}");
    assert!(text.contains("free-space floor"), "{text}");

    // Held back, not run: nothing was verified, and the pass is still due (no state
    // recorded), so it retries on the next cron tick.
    let summary = Catalog::open(&catalog).unwrap().scrub_summary().unwrap();
    assert_eq!(
        summary.never_scrubbed, summary.locations,
        "a held-back pass must not have verified anything"
    );
    let status = schedule(&catalog, &[]);
    assert!(
        stdout(&status).contains("held back now by"),
        "the status view must name the floor before a run: {}",
        stdout(&status)
    );

    // With room (no floor), the same pass runs and records its run.
    write_schedule(&catalog, "[scrub]\nevery = \"1h\"\n");
    let ran = schedule(&catalog, &["--run"]);
    assert_exit(&ran, 0);
    let summary = Catalog::open(&catalog).unwrap().scrub_summary().unwrap();
    assert_eq!(
        summary.never_scrubbed, 0,
        "the pass verified every location"
    );
}

#[test]
fn a_dry_run_does_not_record_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = tree(tmp.path());
    write_schedule(&catalog, "[scrub]\nevery = \"1h\"\n");

    let dry = schedule(&catalog, &["--run", "--dry-run"]);
    assert_exit(&dry, 0);

    // Nothing was done, so the pass must stay due rather than looking "already run".
    let status = schedule(&catalog, &[]);
    assert!(
        stdout(&status).contains("due now (never run)"),
        "a dry run must not record a last-run time: {}",
        stdout(&status)
    );
}

#[test]
fn a_malformed_schedule_names_its_line() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = tree(tmp.path());
    write_schedule(&catalog, "[scrub]\nevery = \"soon\"\n");

    let output = schedule(&catalog, &[]);
    assert_exit(&output, 2);
    let text = stderr(&output);
    assert!(text.contains("line 2"), "{text}");
    assert!(text.contains("scrub"), "{text}");
}

#[test]
fn reconcile_is_scheduled_too() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = tree(tmp.path());
    // A floor was recorded by the sync, but nothing is offloaded, so a reconcile finds
    // nothing to rebuild: it runs, is quiet, and records its run.
    write_schedule(&catalog, "[reconcile]\nevery = \"1h\"\n");

    let before = schedule(&catalog, &[]);
    assert!(
        stdout(&before).contains("reconcile: every 1h — due now (never run)"),
        "{}",
        stdout(&before)
    );

    let run = schedule(&catalog, &["--run"]);
    assert_exit(&run, 0);
    assert!(stdout(&run).trim().is_empty(), "{}", stdout(&run));

    let status = schedule(&catalog, &[]);
    assert!(
        stdout(&status).contains("reconcile: every 1h — next run"),
        "the run must be recorded: {}",
        stdout(&status)
    );
}
