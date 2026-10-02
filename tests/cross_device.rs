//! The move fallback that only a real second filesystem can reach.
//!
//! `fs::rename` cannot cross a mount point, so every production move onto a cold tier
//! runs `copy_then_remove` instead — the one path that can leave a partial file behind.
//! The rest of the suite keeps both sides on a single filesystem and never executes it.
//! These tests put the destination on a genuinely different filesystem, assert that the
//! crossing is real *before* trusting the result, and then check the bytes, the symlink
//! and the absence of leftovers.
//!
//! They skip when no second filesystem is available (see `support`); CI sets
//! `JUST_CACHE_REQUIRE_SECOND_FS=1` so the same skip becomes a failure there.

mod support;

use std::fs;
use std::path::Path;

use just_cache::disk_management::{self, DiskError, FileEntry, MoveOutcome};

fn scan(root: &Path) -> Vec<FileEntry> {
    disk_management::list_files_recursive(root).expect("walk should succeed")
}

fn find<'a>(entries: &'a [FileEntry], relative: &str) -> &'a FileEntry {
    entries
        .iter()
        .find(|entry| entry.relative == Path::new(relative))
        .unwrap_or_else(|| panic!("{relative} should have been found"))
}

/// A payload with no runs long enough to survive a lazy copy, and non-zero bytes so a
/// zero-filled destination is obvious.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251 + 1) as u8).collect()
}

/// The reason these tests exist: the destination must be on a different filesystem, and
/// that has to be proven on the exact directories the move uses rather than assumed.
/// `assert_cross_device` panics if both sides share a device, so a future refactor
/// cannot silently reduce this file to a same-filesystem no-op.
#[test]
fn a_cold_file_crosses_a_mount_point_and_is_copied_byte_for_byte() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = support::work_dir(&second, "cross-device-move");
    fs::create_dir_all(watch.join("shows/season1")).unwrap();
    let bytes = payload(128 * 1024);
    fs::write(watch.join("shows/season1/ep1.mkv"), &bytes).unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "shows/season1/ep1.mkv");

    // Guard first, move second: everything below is meaningless if this fires.
    support::assert_cross_device(&watch, cold.path());
    support::assert_rename_is_cross_device(&watch, cold.path());

    let outcome = disk_management::move_file_with_symlink(cold.path(), entry)
        .expect("a cross-device move must fall back to the copy path");
    assert_eq!(outcome, MoveOutcome::Moved);

    let moved = cold.path().join("shows/season1/ep1.mkv");
    assert_eq!(
        fs::read(&moved).unwrap(),
        bytes,
        "the copy must be byte-for-byte identical to the source"
    );

    let link = watch.join("shows/season1/ep1.mkv");
    assert!(
        fs::symlink_metadata(&link).unwrap().is_symlink(),
        "the original path must be left as a symlink"
    );
    assert_eq!(
        fs::read(&link).unwrap(),
        bytes,
        "reading through the symlink must return the moved bytes"
    );
    assert_eq!(
        fs::canonicalize(&link).unwrap(),
        fs::canonicalize(&moved).unwrap(),
        "the link must resolve to the file on the cold filesystem"
    );

    assert!(
        support::partial_files(cold.path()).is_empty(),
        "a successful cross-device copy must leave no .just_cache-partial-* file: {:?}",
        support::partial_files(cold.path())
    );
}

/// A cross-device move of a nested tree into a pre-existing directory layout: the copy
/// has to create the missing parents on the cold side itself.
#[test]
fn the_cold_side_gains_the_missing_directories() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = support::work_dir(&second, "cross-device-nested");
    fs::create_dir_all(watch.join("a/b/c")).unwrap();
    fs::write(watch.join("a/b/c/deep.bin"), b"deep").unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "a/b/c/deep.bin");
    support::assert_cross_device(&watch, cold.path());
    support::assert_rename_is_cross_device(&watch, cold.path());

    assert_eq!(
        disk_management::move_file_with_symlink(cold.path(), entry).unwrap(),
        MoveOutcome::Moved
    );
    assert_eq!(
        fs::read(cold.path().join("a/b/c/deep.bin")).unwrap(),
        b"deep"
    );
    assert_eq!(
        fs::read_to_string(watch.join("a/b/c/deep.bin")).unwrap(),
        "deep"
    );
    assert!(support::partial_files(cold.path()).is_empty());
}

/// An existing identical copy on the cold filesystem is the resumed state of an
/// interrupted move, and must be adopted rather than transferred again — on the
/// cross-device path too.
#[test]
fn an_identical_copy_across_the_mount_point_is_adopted_not_refused() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = support::work_dir(&second, "cross-device-adopt");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.path().join("shows")).unwrap();
    fs::write(watch.join("shows/same.bin"), b"identical").unwrap();
    fs::write(cold.path().join("shows/same.bin"), b"identical").unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "shows/same.bin");
    support::assert_cross_device(&watch, cold.path());
    support::assert_rename_is_cross_device(&watch, cold.path());

    assert_eq!(
        disk_management::move_file_with_symlink(cold.path(), entry).unwrap(),
        MoveOutcome::LinkedExisting
    );
    assert_eq!(
        fs::read(cold.path().join("shows/same.bin")).unwrap(),
        b"identical"
    );
    assert!(fs::symlink_metadata(watch.join("shows/same.bin"))
        .unwrap()
        .is_symlink());
    assert!(support::partial_files(cold.path()).is_empty());
}

/// A source that shrank between the scan and the move must not cross the boundary. The
/// mover now notices before it copies anything (`SourceChanged`, checked against the size
/// the scan promised), so the destination gets neither the bytes nor a stray partial.
///
/// The separate `ShortCopy` path — a source that changes *during* a long copy, where a
/// partial has already been written and must be cleaned up — has **no automated test**:
/// arranging a mid-copy change means racing a thread against the copy, and a test that
/// passes only when the race is lost is worse than an admitted gap. The pre-check makes
/// that path rare by design (it only triggers for a file changing under a copy already
/// running), and its cleanup is a single `remove_file` beside the copy loop.
#[test]
fn a_failed_cross_device_copy_leaves_no_partial_file_behind() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = support::work_dir(&second, "cross-device-failure");
    fs::create_dir_all(&watch).unwrap();
    let src = watch.join("shrinking.bin");
    fs::write(&src, payload(4096)).unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "shrinking.bin");
    support::assert_cross_device(&watch, cold.path());
    support::assert_rename_is_cross_device(&watch, cold.path());

    // Shrink behind the scan's back: the file is no longer the one the policy chose.
    fs::write(&src, payload(512)).unwrap();

    let err = disk_management::move_file_with_symlink(cold.path(), entry)
        .expect_err("a changed source must not be moved");
    match err {
        DiskError::SourceChanged {
            actual, expected, ..
        } => {
            assert_eq!(actual, 512, "actual size is what the source has now");
            assert_eq!(expected, 4096, "expected size came from the scan");
        }
        other => panic!("expected SourceChanged, got {other:?}"),
    }

    assert!(
        !cold.path().join("shrinking.bin").exists(),
        "a failed copy must not leave the destination path behind"
    );
    assert!(
        support::partial_files(cold.path()).is_empty(),
        "a refused move must leave no .just_cache-partial-* file: {:?}",
        support::partial_files(cold.path())
    );
    assert!(
        !fs::symlink_metadata(&src).unwrap().is_symlink(),
        "the source stays a real file when the move failed"
    );
    assert_eq!(
        fs::read(&src).unwrap(),
        payload(512),
        "the source bytes are untouched by a failed cross-device move"
    );
}
