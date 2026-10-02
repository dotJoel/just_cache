//! End-to-end behaviour of `just_cache reconcile` (issue #24): a disk that was out during
//! a sweep comes back, and the copies it is missing are rebuilt from a surviving sibling —
//! or honestly reported when there is none.
//!
//! Driven through the binary for the user-visible contract (what is rebuilt, what is left
//! alone, and the exit codes), against real trees. The headline case crosses a mount point
//! (`/dev/shm` ↔ the workspace temp filesystem) so the rebuild genuinely takes the
//! cross-device copy path rather than a `rename`; the rest exercise the refusals, which are
//! where a rebuild could do harm.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::{Catalog, CATALOG_NAME};
use just_cache::restore::build_verified_copy;

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

/// `catalog sync` with a floor, so the tier table records what replication promises.
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

/// A replicated offload through the real binary: two copies, no idle or free-space gate.
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

fn reconcile(catalog: &Path, extra: &[&str]) -> Output {
    let mut command = bin();
    command.arg("reconcile").arg("--catalog").arg(catalog);
    command.args(extra);
    command.output().expect("just_cache runs")
}

/// The headline: a copy lost from one disk is rebuilt from its sibling, and a following
/// `catalog sync` agrees the floor is met again — the pass actually closes the gap.
#[test]
fn a_copy_lost_from_one_disk_is_rebuilt_from_its_sibling() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    // The re-added disk lives on a genuinely different filesystem, so the rebuild copies
    // across a mount point and takes the cross-device path.
    let dest_b = support::work_dir(&second, "reconcile-b");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    let catalog = watch.join(CATALOG_NAME);
    let bytes = payload(128 * 1024);
    fs::write(watch.join("shows/ep1.mkv"), &bytes).unwrap();

    // Record the floor, offload with replication, then sync so the catalog reflects the
    // moved state (the operator's normal sequence).
    assert_exit(&catalog_sync(&watch, &[&dest_a, dest_b.path()], 2), 0);
    assert_exit(&replicated_sweep(&watch, &[&dest_a, dest_b.path()]), 0);
    assert_exit(&catalog_sync(&watch, &[&dest_a, dest_b.path()], 2), 0);

    let lost = dest_b.path().join("shows/ep1.mkv");
    let sibling = dest_a.join("shows/ep1.mkv");
    assert!(lost.is_file() && sibling.is_file());
    support::assert_cross_device(&sibling, &lost);

    // The disk comes back empty: the copy that was on it is gone.
    fs::remove_file(&lost).unwrap();

    let output = reconcile(&catalog, &[]);
    // A rebuild is still a finding: a disk was out, and cron has to see it once.
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("rebuilt") && text.contains(&sibling.display().to_string()),
        "the rebuild and its source must be reported:\n{text}"
    );

    assert_eq!(
        fs::read(&lost).unwrap(),
        bytes,
        "the missing copy must be back, byte for byte"
    );
    assert_eq!(
        fs::read(&sibling).unwrap(),
        bytes,
        "the source is untouched"
    );
    assert!(
        support::partial_files(dest_b.path()).is_empty(),
        "a verified rebuild must leave no .just_cache-partial-* file"
    );

    // The rebuilt location is recorded verified, with the object's real checksum — that is
    // the catalog being told the truth about the bytes a digest vouched for.
    let catalog = Catalog::open(&catalog).unwrap();
    let tier_b = fs::canonicalize(dest_b.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let record = catalog
        .all_locations()
        .unwrap()
        .into_iter()
        .find(|location| location.tier == tier_b && location.storage_key == "shows/ep1.mkv")
        .unwrap_or_else(|| panic!("the rebuilt copy must be recorded"));
    assert!(record.verified, "a rebuilt copy is recorded verified");
    assert_eq!(
        record.checksum.as_deref(),
        Some(record.object.as_str()),
        "the stored checksum is the object identity"
    );

    // And the floor really is met again: a fresh sync has no differences.
    let healed = catalog_sync(&watch, &[&dest_a, dest_b.path()], 2);
    assert_exit(&healed, 0);
    assert!(
        stdout(&healed).contains("no differences"),
        "reconcile must close the under-replication finding:\n{}",
        stdout(&healed)
    );

    // A second reconcile has nothing left to do.
    let again = reconcile(&watch.join(CATALOG_NAME), &[]);
    assert_exit(&again, 0);
}

/// The verify-before-publish contract, proven at the unit boundary: a private copy that
/// does not hash to the recorded checksum is never renamed into place, so the destination
/// stays absent and no partial is left behind.
#[test]
fn a_copy_that_fails_its_read_back_is_never_published() {
    let tmp = tempfile::tempdir().unwrap();
    let good = tmp.path().join("good.bin");
    let dest = tmp.path().join("dest.bin");
    let bytes = payload(4096);
    fs::write(&good, &bytes).unwrap();

    // A checksum that is not the source's: the read-back cannot match, whatever the copy
    // does. (A torn read cannot be produced deterministically; a wrong expectation reaches
    // the same guard.)
    let wrong = blake3::hash(b"not these bytes");
    let error = build_verified_copy(&good, &dest, &wrong).expect_err("must refuse to publish");
    assert!(
        matches!(error, just_cache::RestoreError::VerifyMismatch { .. }),
        "expected a verification refusal, got {error:?}"
    );
    assert!(
        !dest.exists(),
        "no unverified bytes may be published at the destination"
    );
    assert!(
        support::partial_files(tmp.path()).is_empty(),
        "the private partial must be cleaned up"
    );

    // The same call with the real checksum publishes and preserves the bytes.
    let expected = blake3::hash(&bytes);
    assert_eq!(build_verified_copy(&good, &dest, &expected).unwrap(), 4096);
    assert_eq!(fs::read(&dest).unwrap(), bytes);
}

/// A same-size sibling is not a source: the digests differ, so the rebuild refuses, the
/// stranger is left exactly as it was, and the object is reported rather than faked.
#[test]
fn a_rebuild_with_only_a_same_size_sibling_refuses() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let catalog = watch.join(CATALOG_NAME);
    let bytes = payload(8192);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();

    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);
    assert_exit(&replicated_sweep(&watch, &[&dest_a, &dest_b]), 0);
    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);

    // The only surviving sibling becomes a same-size stranger: same length, different
    // bytes — the exact case a size-only check gets wrong.
    let sibling = dest_a.join("clip.bin");
    let flipped: Vec<u8> = bytes.iter().map(|byte| byte ^ 0xff).collect();
    fs::write(&sibling, &flipped).unwrap();
    assert_eq!(fs::metadata(&sibling).unwrap().len(), bytes.len() as u64);

    // And the copy that would have been rebuilt is gone anyway, so the object is below
    // its floor with only the stranger to rebuild from.
    let missing = dest_b.join("clip.bin");
    fs::remove_file(&missing).unwrap();

    let output = reconcile(&catalog, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("no verified sibling") || text.contains("no sibling matches"),
        "the refusal must be reported:\n{text}"
    );
    assert!(
        !missing.exists(),
        "nothing may be rebuilt from bytes that do not verify"
    );
    assert_eq!(
        fs::read(&sibling).unwrap(),
        flipped,
        "the same-size stranger must be left exactly as it was"
    );
}

/// Only a corrupt sibling exists: the rebuild marks it damaged in the catalog and copies
/// nothing — the same hand the scrubber shows, never a deletion.
#[test]
fn a_rebuild_with_only_a_corrupt_sibling_marks_rather_than_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let catalog_path = watch.join(CATALOG_NAME);
    let bytes = payload(8192);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();

    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);
    assert_exit(&replicated_sweep(&watch, &[&dest_a, &dest_b]), 0);
    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);

    // The sibling is corrupt in place (same size, so only a digest sees it)...
    let sibling = dest_a.join("clip.bin");
    let mut rotten = bytes.clone();
    let mid = rotten.len() / 2;
    rotten[mid] ^= 0xff;
    fs::write(&sibling, &rotten).unwrap();
    // ...and the other copy is gone, leaving the corrupt copy as the only sibling.
    let missing = dest_b.join("clip.bin");
    fs::remove_file(&missing).unwrap();

    let output = reconcile(&catalog_path, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("no sibling matches") || text.contains("no verified sibling"),
        "the corrupt sibling must not be used:\n{text}"
    );
    assert!(
        !missing.exists(),
        "a corrupt sibling is not a source, so nothing is rebuilt"
    );
    assert_eq!(
        fs::read(&sibling).unwrap(),
        rotten,
        "the corrupt bytes are the only record left and must not be touched"
    );

    // The corrupt sibling is now marked damaged, so the next scrub/audit sees the rot
    // without re-discovering it.
    let catalog = Catalog::open(&catalog_path).unwrap();
    assert_eq!(
        catalog.scrub_summary().unwrap().damaged,
        1,
        "the corrupt sibling must be marked damaged"
    );
}

/// A disk that is still out is reported, never turned into a directory (invariant 1): the
/// missing root is not created, and the object is left under-replicated.
#[test]
fn a_destination_root_that_is_still_out_is_reported_not_created() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let catalog = watch.join(CATALOG_NAME);
    let bytes = payload(4096);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();

    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);
    assert_exit(&replicated_sweep(&watch, &[&dest_a, &dest_b]), 0);
    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);

    // Both the copy and the root go away — an unmounted disk, as far as the path is
    // concerned.
    fs::remove_file(dest_b.join("clip.bin")).unwrap();
    fs::remove_dir(&dest_b).unwrap();

    let output = reconcile(&catalog, &[]);
    assert_exit(&output, 1);
    assert!(
        stdout(&output).contains("tier not mounted"),
        "the gone tier must be named:\n{}",
        stdout(&output)
    );
    assert!(
        !dest_b.exists(),
        "a destination root must never be created (invariant 1)"
    );
    assert_eq!(fs::read(dest_a.join("clip.bin")).unwrap(), bytes);
}

/// The issue's own scenario: disk-b is out when the offload runs, so it never receives a
/// copy and the catalog has no row for it. When the disk comes back, `reconcile` fills it
/// from the surviving sibling at the mirrored key — and a fresh sync agrees the floor is
/// met. This crosses a mount point on purpose, so the rebuild takes the cross-device path.
#[test]
fn a_disk_that_was_out_during_the_sweep_gets_its_copy_rebuilt() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    // The disk that is out lives on a genuinely different filesystem.
    let dest_b = support::work_dir(&second, "reconcile-readded");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    let catalog = watch.join(CATALOG_NAME);
    let bytes = payload(128 * 1024);
    fs::write(watch.join("shows/ep1.mkv"), &bytes).unwrap();

    // Record the floor for both tiers, then offload. `--copies 1` keeps the sweep honest
    // while disk-b is out (a sweep with --copies 2 would refuse, rather than place fewer
    // copies than the floor); the floor stays recorded at 2 for the *tiers*.
    assert_exit(&catalog_sync(&watch, &[&dest_a, dest_b.path()], 2), 0);
    let mut sweep = bin();
    sweep.arg("--watch").arg(&watch);
    sweep.arg("--dest").arg(&dest_a);
    sweep.args([
        "--min-idle-days",
        "0",
        "--min-observed-accesses",
        "0",
        "--min-free-gb",
        "0",
        "--once",
    ]);
    assert_exit(&sweep.output().unwrap(), 0);
    assert!(
        fs::symlink_metadata(watch.join("shows/ep1.mkv"))
            .unwrap()
            .is_symlink(),
        "the offload itself must have happened"
    );

    // The operator's next step: sync against the (still out) disk-b. This is what reports
    // `under-replicated` — and what leaves the catalog with no row for disk-b at all,
    // which is exactly the state reconcile has to know how to fix.
    let report = catalog_sync(&watch, &[&dest_a, dest_b.path()], 2);
    assert_exit(&report, 1);
    assert!(
        stdout(&report).contains("under-replicated"),
        "the missing disk must be reported:\n{}",
        stdout(&report)
    );

    // The disk is re-added (the directory exists again, empty). Nothing has reconciled
    // this yet, and the sweep that made the copy is long gone.
    let rebuilt = dest_b.path().join("shows/ep1.mkv");
    support::assert_cross_device(&dest_a, dest_b.path());

    let output = reconcile(&catalog, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("rebuilt") && text.contains(&rebuilt.display().to_string()),
        "the rebuild must be reported:\n{text}"
    );
    assert_eq!(
        fs::read(&rebuilt).unwrap(),
        bytes,
        "the disk that came back must hold the object again"
    );
    assert_eq!(
        fs::read(dest_a.join("shows/ep1.mkv")).unwrap(),
        bytes,
        "the surviving sibling is untouched"
    );
    assert!(
        support::partial_files(dest_b.path()).is_empty(),
        "a verified rebuild must leave no .just_cache-partial-* file"
    );

    // The floor is genuinely met: a fresh sync finds no differences.
    let healed = catalog_sync(&watch, &[&dest_a, dest_b.path()], 2);
    assert_exit(&healed, 0);
    assert!(
        stdout(&healed).contains("no differences"),
        "reconcile must close the under-replication finding:\n{}",
        stdout(&healed)
    );
}

/// A copy that was restored by hand onto the re-added disk is adopted, not re-copied: it
/// is hashed against the recorded checksum, recorded verified, and nothing is rewritten.
#[test]
fn a_hand_restored_copy_is_adopted_after_hashing() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let catalog_path = watch.join(CATALOG_NAME);
    let bytes = payload(8192);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();

    // Same setup as above: floor 2 recorded, the offload only reached disk-a, and the
    // sync against the still-absent disk-b reports the gap.
    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);
    let mut sweep = bin();
    sweep.arg("--watch").arg(&watch);
    sweep.arg("--dest").arg(&dest_a);
    sweep.args([
        "--min-idle-days",
        "0",
        "--min-observed-accesses",
        "0",
        "--min-free-gb",
        "0",
        "--once",
    ]);
    assert_exit(&sweep.output().unwrap(), 0);
    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 1);

    // The operator restores the copy by hand — byte-identical, but the catalog has no row
    // for it, so a rebuild would have been the tool's only other answer.
    let restored = dest_b.join("clip.bin");
    fs::write(&restored, &bytes).unwrap();

    let output = reconcile(&catalog_path, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("adopted") && text.contains(&restored.display().to_string()),
        "the adoption must be reported:\n{text}"
    );
    assert_eq!(fs::read(&restored).unwrap(), bytes, "nothing was rewritten");

    // And it is now recorded as a verified replica of the object.
    let catalog = Catalog::open(&catalog_path).unwrap();
    let tier_b = fs::canonicalize(&dest_b)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let record = catalog
        .all_locations()
        .unwrap()
        .into_iter()
        .find(|location| location.tier == tier_b && location.storage_key == "clip.bin")
        .unwrap_or_else(|| panic!("the adopted copy must be recorded"));
    assert!(record.verified, "an adopted copy is verified by its hash");
    assert_eq!(
        record.checksum.as_deref(),
        Some(record.object.as_str()),
        "the stored checksum is the object identity"
    );
}

/// `reconcile` without a catalog is a usage error: the recorded checksum a rebuild is
/// proved against has to come from somewhere, and a catalog is never created here.
#[test]
fn reconcile_without_a_catalog_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.sqlite");
    let output = reconcile(&missing, &[]);
    assert_exit(&output, 2);
    assert!(
        stderr(&output).contains("does not exist"),
        "the refusal must name the missing catalog:\n{}",
        stderr(&output)
    );
    assert!(!missing.exists(), "reconcile must not create a catalog");
}

/// `--dry-run` resolves and reports what it would rebuild, and changes nothing.
#[test]
fn a_dry_run_reports_the_rebuild_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let catalog = watch.join(CATALOG_NAME);
    let bytes = payload(4096);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();

    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);
    assert_exit(&replicated_sweep(&watch, &[&dest_a, &dest_b]), 0);
    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);

    let missing = dest_b.join("clip.bin");
    fs::remove_file(&missing).unwrap();
    // Snapshot the rows, so the dry run's "changes nothing" can be asserted exactly.
    let before = Catalog::open(&catalog).unwrap().all_locations().unwrap();

    let output = reconcile(&catalog, &["--dry-run"]);
    assert_exit(&output, 1);
    assert!(
        stdout(&output).contains("would rebuild"),
        "a dry run must report the plan:\n{}",
        stdout(&output)
    );
    assert!(!missing.exists(), "a dry run writes no copy");

    // And no row was recorded for it: the catalog is exactly as it was before the run.
    let catalog = Catalog::open(&catalog).unwrap();
    let after = catalog.all_locations().unwrap();
    assert_eq!(after, before, "a dry run must not change any location row");
}

/// The no-copy-left case: the object is reported and the bytes that remain are untouched.
#[test]
fn an_object_with_no_surviving_copy_is_reported_and_nothing_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest_a = tmp.path().join("cold-a");
    let dest_b = tmp.path().join("cold-b");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let catalog = watch.join(CATALOG_NAME);
    let bytes = payload(4096);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();

    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);
    assert_exit(&replicated_sweep(&watch, &[&dest_a, &dest_b]), 0);
    assert_exit(&catalog_sync(&watch, &[&dest_a, &dest_b], 2), 0);

    // Every copy is gone. Nothing can be rebuilt and nothing may be deleted to hide it.
    fs::remove_file(dest_a.join("clip.bin")).unwrap();
    fs::remove_file(dest_b.join("clip.bin")).unwrap();

    let output = reconcile(&catalog, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("no surviving copy"),
        "the dead end must be reported:\n{text}"
    );
    // Both destinations are exactly as they were: empty.
    assert!(!dest_a.join("clip.bin").exists());
    assert!(!dest_b.join("clip.bin").exists());
    // The hot path is still the symlink the mover left; reconcile touches nothing there.
    assert!(fs::symlink_metadata(watch.join("clip.bin"))
        .unwrap()
        .is_symlink());
}

/// A small guard for the helper shape above: `PathBuf` is used so `decode_hex` stays
/// reachable even if a future edit narrows the imports.
#[allow(dead_code)]
fn _type_anchor(_: &PathBuf) {}
