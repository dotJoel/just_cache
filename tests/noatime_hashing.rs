//! The tool never marks its own use (issue #129).
//!
//! The mover's idle signal is atime, and on a `relatime` mount any read refreshes it — so a
//! `catalog sync` that hashed a file used to become that file's "last access", postponing
//! its move by a whole idle window. On a `noatime` mount the refresh is invisible, which is
//! how the #45 pin tests passed on the box they were written on and failed only on CI
//! (#124). These tests therefore do not depend on the mount: every run arms
//! `JUST_CACHE_FAULT=relatime=1`, which stamps atime to now after any internal read-only
//! open that did *not* get `O_NOATIME` — the deterministic form of the pollution. With the
//! fix the stamp never fires; without it (verified by removing the flag from
//! `digest::open_for_hashing` and watching both tests fail) the file looks freshly used.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

const RELATIME: (&str, &str) = ("JUST_CACHE_FAULT", "relatime=1");

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_just_cache"));
    command.env(RELATIME.0, RELATIME.1);
    command
}

fn text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn days_ago(days: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(days * 86_400)
}

fn stamp(path: &Path, when: SystemTime) {
    let times = fs::FileTimes::new().set_accessed(when).set_modified(when);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(times)
        .unwrap();
}

fn accessed(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().accessed().unwrap()
}

/// Still at the stamp, i.e. nothing the tool did counted as an access. A day of slack is
/// far tighter than the 90 days a refresh would move it by.
fn assert_untouched(path: &Path, stamped: SystemTime, after: &str) {
    let now_age = SystemTime::now()
        .duration_since(accessed(path))
        .unwrap_or_default();
    let stamped_age = SystemTime::now().duration_since(stamped).unwrap();
    assert!(
        now_age + Duration::from_secs(86_400) > stamped_age,
        "{after} refreshed {}'s atime: the tool marked its own read as use",
        path.display()
    );
}

/// The acceptance test: sync, then sweep, and the file still moves on the idleness it was
/// stamped with — no re-stamp in between, unlike `tests/pins.rs` before this fix.
#[test]
fn a_sync_does_not_postpone_the_move_it_precedes() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let file = watch.join("film.mkv");
    fs::write(&file, b"idle for ninety days").unwrap();
    let stamped = days_ago(90);
    stamp(&file, stamped);

    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .output()
        .unwrap();
    assert_eq!(sync.status.code(), Some(0), "sync failed: {}", text(&sync));
    assert_untouched(&file, stamped, "catalog sync");

    let sweep = bin()
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .args([
            "--min-idle-days",
            "30",
            "--min-free-gb",
            "0",
            "--once",
            "-v",
        ])
        .output()
        .unwrap();
    assert!(
        fs::symlink_metadata(&file)
            .unwrap()
            .file_type()
            .is_symlink(),
        "a file idle for 90 days must move after a sync: {}",
        text(&sweep)
    );
    assert!(cold.join("film.mkv").is_file(), "{}", text(&sweep));
}

/// The rule covers every internal reader, not just sync: scrub's verify and audit's probe
/// read both the hot file and its stored copy, and neither read is a use.
#[test]
fn scrub_and_audit_do_not_mark_their_reads_as_use() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let hot = watch.join("a.bin");
    let copy = cold.join("a.bin");
    fs::write(&hot, b"payload").unwrap();
    fs::write(&copy, b"payload").unwrap();

    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .unwrap();
    assert_eq!(sync.status.code(), Some(0), "sync failed: {}", text(&sync));

    // Stamp after sync so this test isolates scrub and audit.
    let stamped = days_ago(90);
    stamp(&hot, stamped);
    stamp(&copy, stamped);

    let scrub = bin()
        .arg("scrub")
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .unwrap();
    assert_eq!(
        scrub.status.code(),
        Some(0),
        "scrub failed: {}",
        text(&scrub)
    );
    assert_untouched(&copy, stamped, "scrub");
    assert_untouched(&hot, stamped, "scrub");

    // The walk-based audit, which hashes both sides to prove the copy matches.
    let audit = bin()
        .arg("audit")
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .output()
        .unwrap();
    assert_ne!(
        audit.status.code(),
        Some(101),
        "audit panicked: {}",
        text(&audit)
    );
    assert_untouched(&copy, stamped, "audit");
    assert_untouched(&hot, stamped, "audit");
}
