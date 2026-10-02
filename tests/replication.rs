//! End-to-end behaviour of replication (`sweep --copies N`): the copies, the floor, and
//! the verify-before-delete rule that stops the mover trading the last copy away.
//!
//! Two things are deliberately proven here rather than assumed:
//!
//! * the two-destination test asserts, on the exact directories it uses, that at least one
//!   copy really crossed a mount point (the workspace → `/dev/shm` pair). The second
//!   destination is a second root on that same tmpfs — allowed by the task, and honestly
//!   labelled: distinctness for a floor is by configured root, not by device (see
//!   `src/replication.rs`).
//! * the checksum-mismatch test flips bytes in a *same-size* file at a destination. Sizes
//!   alone cannot tell that copy from a real one, which is exactly the case a size-only
//!   check would get wrong and delete the source over.

mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use just_cache::disk_management::{self, FileEntry};
use just_cache::replication::{self, ReplicaStatus};

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

fn scan(root: &Path) -> Vec<FileEntry> {
    disk_management::list_files_recursive(root).expect("walk should succeed")
}

fn find<'a>(entries: &'a [FileEntry], relative: &str) -> &'a FileEntry {
    entries
        .iter()
        .find(|entry| entry.relative == Path::new(relative))
        .unwrap_or_else(|| panic!("{relative} should have been found"))
}

/// Payload longer than one copy buffer and non-zero, so a zero-filled destination shows.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251 + 1) as u8).collect()
}

/// A sweep that replicates: two copies, no idle gate, no free-space gate (the tmpfs the
/// second destination lives on is far below the default 1 GiB floor).
fn replicated_sweep(watch: &Path, dests: &[&Path]) -> Output {
    let mut command = bin();
    command.arg("--watch").arg(watch);
    for dest in dests {
        command.arg("--dest").arg(dest);
    }
    command
        .args([
            "--copies",
            "2",
            "--min-idle-days",
            "0",
            "--min-observed-accesses",
            "0",
            "--min-free-gb",
            "0",
            "--once",
        ])
        .output()
        .expect("just_cache runs")
}

/// The headline: two distinct destinations both receive a verified copy, the source is
/// only removed afterwards, and the hot path is left as a symlink to the first copy.
#[test]
fn a_two_destination_offload_places_two_verified_copies_then_removes_the_source() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = support::work_dir(&second, "replication-a");
    let dest_b = support::work_dir(&second, "replication-b");
    fs::create_dir_all(watch.join("shows")).unwrap();
    let bytes = payload(128 * 1024);
    fs::write(watch.join("shows/ep1.mkv"), &bytes).unwrap();

    // One copy must genuinely cross a mount point; assert it before trusting the result.
    support::assert_cross_device(&watch, dest_a.path());

    let output = replicated_sweep(&watch, &[dest_a.path(), dest_b.path()]);
    assert_exit(&output, 0);

    let a = dest_a.path().join("shows/ep1.mkv");
    let b = dest_b.path().join("shows/ep1.mkv");
    assert_eq!(fs::read(&a).unwrap(), bytes, "first destination copy");
    assert_eq!(fs::read(&b).unwrap(), bytes, "second destination copy");

    let link = watch.join("shows/ep1.mkv");
    assert!(
        fs::symlink_metadata(&link).unwrap().is_symlink(),
        "the hot path must be a symlink once the floor is met"
    );
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(&a).unwrap(),
        "the link points at the primary (first) copy"
    );

    assert!(
        support::partial_files(dest_a.path()).is_empty()
            && support::partial_files(dest_b.path()).is_empty(),
        "a verified replication must leave no .just_cache-partial-* file"
    );
}

/// Verify-before-delete firing: a same-size but different-content copy at a destination
/// cannot count toward the floor, and the source is kept because the floor was not met.
#[test]
fn a_copy_that_fails_its_checksum_does_not_count_and_the_source_is_kept() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = support::work_dir(&second, "replication-verify");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let bytes = payload(4096);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();

    // Same length, different bytes — the trap a size-only check walks straight into.
    let flipped: Vec<u8> = bytes.iter().map(|byte| byte ^ 0xff).collect();
    fs::create_dir_all(dest_b.join(".")).unwrap();
    fs::write(dest_b.join("clip.bin"), &flipped).unwrap();
    assert_eq!(
        fs::metadata(dest_b.join("clip.bin")).unwrap().len(),
        bytes.len() as u64,
        "the stranger must be the same size for this test to mean anything"
    );

    support::assert_cross_device(&watch, dest_a.path());
    let output = replicated_sweep(&watch, &[dest_a.path(), &dest_b]);
    // The floor was not met, so this is a finding: nonzero, but the data is safe.
    assert_exit(&output, 1);

    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("under-replicated"),
        "the unmet floor must be reported:\n{text}"
    );

    let source = watch.join("clip.bin");
    let source_meta = fs::symlink_metadata(&source).unwrap();
    assert!(
        !source_meta.is_symlink(),
        "a below-floor object must keep its source as a real file"
    );
    assert_eq!(
        fs::read(&source).unwrap(),
        bytes,
        "the source bytes must be untouched"
    );
    assert_eq!(
        fs::read(dest_b.join("clip.bin")).unwrap(),
        flipped,
        "the same-size stranger at the second destination is never clobbered"
    );
    assert_eq!(
        fs::read(dest_a.path().join("clip.bin")).unwrap(),
        bytes,
        "the first copy did verify and is on disk"
    );
}

/// A disk that goes away between copies: the missing root is reported, the copy that
/// already succeeded stays, and — the part that matters — the source is not deleted.
///
/// The library is exercised directly because removing a destination *between* the two
/// copies of one sweep cannot be timed from a test binary; the code path is identical,
/// since `replicate` re-checks each destination root immediately before using it.
#[test]
fn a_destination_removed_between_copies_is_reported_and_the_source_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let gone = tmp.path().join("cold-gone");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    let bytes = payload(2048);
    fs::write(watch.join("a.bin"), &bytes).unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "a.bin");
    let outcome = replication::replicate(entry, &[dest_a.clone(), gone.clone()], 2, 0);

    assert!(
        !outcome.meets_floor(),
        "the vanished disk cannot satisfy the floor"
    );
    assert_eq!(outcome.verified, 1);
    assert!(
        outcome
            .replicas
            .iter()
            .any(|placement| placement.status == ReplicaStatus::Unavailable),
        "the missing root is reported as unavailable, not crashed on: {outcome:?}"
    );
    assert_eq!(fs::read(dest_a.join("a.bin")).unwrap(), bytes);
    assert!(
        watch.join("a.bin").is_file(),
        "the source is the last copy and must not be retired below the floor"
    );
}

/// The floor is recorded per tier and checked against what is really on the disks: delete
/// one replica by hand and the next sync names the disk it is missing from.
#[test]
fn catalog_sync_reports_below_floor_after_a_replica_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    fs::write(watch.join("movie.bin"), b"movie bytes").unwrap();

    // Record the floor with the file still hot: a present file has nothing to replicate
    // yet, so this is a clean baseline.
    let before = catalog_sync(&watch, &[&dest_a, &dest_b], 2);
    assert_exit(&before, 0);

    // Now offload it onto both disks through the real binary.
    let sweep = replicated_sweep(&watch, &[&dest_a, &dest_b]);
    assert_exit(&sweep, 0);
    assert!(fs::symlink_metadata(watch.join("movie.bin"))
        .unwrap()
        .is_symlink());
    assert!(dest_a.join("movie.bin").is_file());
    assert!(dest_b.join("movie.bin").is_file());

    // Healthy after the offload.
    let after = catalog_sync(&watch, &[&dest_a, &dest_b], 2);
    assert_exit(&after, 0);
    assert!(
        stdout(&after).contains("no differences"),
        "two verified copies meet the floor:\n{}",
        stdout(&after)
    );

    // Delete one replica behind the tool's back.
    fs::remove_file(dest_b.join("movie.bin")).unwrap();

    let report = catalog_sync(&watch, &[&dest_a, &dest_b], 2);
    assert_exit(&report, 1);
    let text = stdout(&report);
    assert!(
        text.contains("under-replicated"),
        "the lost replica must be a finding:\n{text}"
    );
    assert!(
        text.contains(&dest_b.canonicalize().unwrap().to_string_lossy().to_string())
            || text.contains("cold-b"),
        "the report must name the disk the copy is missing from:\n{text}"
    );
    // The surviving copy's disk is the one it *is* on, and the location rows are kept.
    assert!(dest_a.join("movie.bin").is_file());
}

/// A floor that the invocation cannot satisfy is refused before anything moves.
#[test]
fn copies_beyond_the_distinct_destinations_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let only = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&only).unwrap();
    fs::write(watch.join("a.bin"), b"payload").unwrap();

    let output = replicated_sweep(&watch, &[&only]);
    assert_exit(&output, 2);
    assert!(
        stderr(&output).contains("--copies"),
        "the refusal must name the floor:\n{}",
        stderr(&output)
    );
    assert!(
        watch.join("a.bin").is_file(),
        "a refused invocation must not have moved anything"
    );
    assert!(!only.join("a.bin").exists());
}

/// The default is unchanged: with `--copies` left alone a sweep still fills one disk,
/// not two.
#[test]
fn the_default_sweep_still_places_exactly_one_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    fs::write(watch.join("a.bin"), b"payload").unwrap();

    let output = bin()
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&dest_a)
        .arg("--dest")
        .arg(&dest_b)
        .args(["--min-idle-days", "0", "--min-free-gb", "0", "--once"])
        .output()
        .unwrap();
    assert_exit(&output, 0);

    assert!(
        dest_a.join("a.bin").is_file(),
        "the first tier took the file"
    );
    assert!(
        !dest_b.join("a.bin").exists(),
        "the second tier must be untouched under the default floor of 1"
    );
    assert!(fs::symlink_metadata(watch.join("a.bin"))
        .unwrap()
        .is_symlink());
}

/// `catalog sync` with a floor: record it per destination tier, then report against it.
fn catalog_sync(watch: &Path, dests: &[&Path], copies: usize) -> Output {
    let mut command = bin();
    command.args(["catalog", "sync", "--watch"]).arg(watch);
    for dest in dests {
        command.arg("--dest").arg(dest);
    }
    command
        .args(["--copies", &copies.to_string()])
        .output()
        .expect("just_cache runs")
}

/// The filesystem-only view of the same state the catalog calls under-replicated: an
/// offloaded object with a copy missing from one of its disks is a `replica-lost` finding
/// that names the disk.
#[test]
fn audit_reports_a_lost_replica_and_names_the_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    fs::write(dest_a.join("movie.bin"), b"movie bytes").unwrap();
    std::os::unix::fs::symlink("../cold-a/movie.bin", watch.join("movie.bin")).unwrap();

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&dest_a)
        .arg("--dest")
        .arg(&dest_b)
        .args(["--copies", "2"])
        .output()
        .unwrap();

    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("replica-lost"),
        "a missing replica must be a finding:\n{text}"
    );
    assert!(
        text.contains("cold-b"),
        "the finding must name the disk the copy is missing from:\n{text}"
    );

    // With the floor left at the default, the one copy is all that is asked for: healthy.
    let default = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&dest_a)
        .arg("--dest")
        .arg(&dest_b)
        .output()
        .unwrap();
    assert_exit(&default, 0);
}
