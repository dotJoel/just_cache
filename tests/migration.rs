//! End-to-end behaviour of the move layer against a real temporary tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use just_cache::disk_management::{self, DiskError, FileEntry, MoveOutcome};
use just_cache::file_movement::{self, FileOutcome, Policy, UsageTracker};
use just_cache::opened::{FileId, Guards, OpenFiles};
use just_cache::scope::Scope;

fn scan(root: &Path) -> Vec<FileEntry> {
    disk_management::list_files_recursive(root).expect("walk should succeed")
}

fn find<'a>(entries: &'a [FileEntry], relative: &str) -> &'a FileEntry {
    entries
        .iter()
        .find(|entry| entry.relative == Path::new(relative))
        .unwrap_or_else(|| panic!("{relative} should have been found"))
}

#[test]
fn a_cold_file_is_moved_and_left_behind_as_a_relative_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows/season1")).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("shows/season1/ep1.mkv"), b"episode one").unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "shows/season1/ep1.mkv");
    let outcome =
        disk_management::move_file_with_symlink(&cold, entry).expect("move should succeed");

    assert_eq!(outcome, MoveOutcome::Moved);
    let moved = cold.join("shows/season1/ep1.mkv");
    assert_eq!(fs::read(&moved).unwrap(), b"episode one");

    let link = watch.join("shows/season1/ep1.mkv");
    let link_metadata = fs::symlink_metadata(&link).unwrap();
    assert!(
        link_metadata.is_symlink(),
        "the original path must be a symlink"
    );
    assert_eq!(
        fs::read_to_string(&link).unwrap(),
        "episode one",
        "reads through the symlink must still work"
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        PathBuf::from("../../../cold/shows/season1/ep1.mkv"),
        "links are stored relative so the pair survives being moved or remounted"
    );
}

#[test]
fn a_second_run_is_a_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("clip.mov"), b"clip").unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "clip.mov");
    assert_eq!(
        disk_management::move_file_with_symlink(&cold, entry).unwrap(),
        MoveOutcome::Moved
    );

    // Re-scanning finds the symlink, and re-running against it changes nothing.
    let rescan = scan(&watch);
    let linked = find(&rescan, "clip.mov");
    assert!(linked.is_symlink);
    assert_eq!(
        disk_management::move_file_with_symlink(&cold, linked).unwrap(),
        MoveOutcome::AlreadyLinked
    );

    // And running the whole sweep again moves nothing.
    let mut tracker = UsageTracker::new();
    for entry in rescan.iter().filter(|entry| !entry.is_symlink) {
        tracker.observe(entry);
    }
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };
    let report = file_movement::migrate_least_used(
        &rescan,
        &tracker,
        &policy,
        &Scope::everything(),
        &Guards::permissive(),
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );

    assert_eq!(
        report.records.len(),
        0,
        "an already-migrated tree has no work"
    );
    assert_eq!(fs::read(cold.join("clip.mov")).unwrap(), b"clip");
}

#[test]
fn an_identical_copy_already_on_the_cold_disk_is_reused_not_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("same.bin"), b"identical").unwrap();
    fs::write(cold.join("same.bin"), b"identical").unwrap();

    let entries = scan(&watch);
    let outcome = disk_management::move_file_with_symlink(&cold, find(&entries, "same.bin"))
        .expect("a same-size copy is the resumed state of an interrupted move");

    assert_eq!(outcome, MoveOutcome::LinkedExisting);
    assert_eq!(fs::read(cold.join("same.bin")).unwrap(), b"identical");
    assert!(fs::symlink_metadata(watch.join("same.bin"))
        .unwrap()
        .is_symlink());
}

#[test]
fn a_destination_of_a_different_size_is_refused_rather_than_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("clash.bin"), b"the source copy").unwrap();
    fs::write(cold.join("clash.bin"), b"something else entirely").unwrap();

    let entries = scan(&watch);
    let err = disk_management::move_file_with_symlink(&cold, find(&entries, "clash.bin"))
        .expect_err("mismatched destination must not be clobbered");

    assert!(matches!(err, DiskError::DestinationConflict { .. }));
    assert_eq!(
        fs::read(cold.join("clash.bin")).unwrap(),
        b"something else entirely"
    );
    assert!(!fs::symlink_metadata(watch.join("clash.bin"))
        .unwrap()
        .is_symlink());
}

#[test]
fn a_symlink_loop_does_not_hang_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    fs::create_dir_all(watch.join("a")).unwrap();
    fs::write(watch.join("a/real.txt"), b"real").unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&watch, watch.join("a/loop")).unwrap();
    #[cfg(not(unix))]
    return;

    let entries = scan(&watch);
    let names: Vec<String> = entries
        .iter()
        .map(|entry| entry.relative.to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"a/real.txt".to_string()));
    assert!(
        names.contains(&"a/loop".to_string()),
        "the link itself is listed"
    );
    assert!(!names.iter().any(|name| name.starts_with("a/loop/")));
}

#[test]
fn interleaved_moves_of_two_files_keep_their_own_contents() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("x")).unwrap();
    fs::create_dir_all(watch.join("y")).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("x/same-name.bin"), b"from x").unwrap();
    fs::write(watch.join("y/same-name.bin"), b"from y").unwrap();

    for entry in scan(&watch) {
        let outcome = disk_management::move_file_with_symlink(&cold, &entry)
            .unwrap_or_else(|err| panic!("{}: {err}", entry.path.display()));
        assert_eq!(outcome, MoveOutcome::Moved);
    }

    assert_eq!(fs::read(cold.join("x/same-name.bin")).unwrap(), b"from x");
    assert_eq!(fs::read(cold.join("y/same-name.bin")).unwrap(), b"from y");
    assert_eq!(
        fs::read_to_string(watch.join("x/same-name.bin")).unwrap(),
        "from x"
    );
    assert_eq!(
        fs::read_to_string(watch.join("y/same-name.bin")).unwrap(),
        "from y"
    );
}

#[test]
fn a_sweep_skips_everything_outside_the_include_set() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("media")).unwrap();
    fs::create_dir_all(watch.join("scratch")).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("media/cold.bin"), b"media payload").unwrap();
    fs::write(watch.join("scratch/cold.bin"), b"scratch payload").unwrap();

    let entries = scan(&watch);
    let scope = Scope::build(&["media/**".to_string()], &[], 0, None).unwrap();
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &scope,
        &Guards::permissive(),
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );

    assert_eq!(report.moved(), 1);
    assert_eq!(report.excluded(), 1);
    assert!(cold.join("media/cold.bin").is_file());
    assert_eq!(
        fs::read(watch.join("scratch/cold.bin")).unwrap(),
        b"scratch payload",
        "a file outside the include set must not be touched"
    );
    assert!(!cold.join("scratch/cold.bin").exists());
}

#[test]
fn an_excluded_directory_survives_a_sweep_even_when_it_is_the_coldest_thing_there() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("app/node_modules/react")).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("app/node_modules/react/index.js"), b"{}").unwrap();
    fs::write(watch.join("app/index.js"), b"console.log(1)").unwrap();

    let entries = scan(&watch);
    let scope = Scope::build(&[], &["node_modules".to_string()], 0, None).unwrap();
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &scope,
        &Guards::permissive(),
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );

    assert_eq!(report.moved(), 1);
    assert!(fs::read(watch.join("app/node_modules/react/index.js")).is_ok());
    assert_eq!(
        fs::read(watch.join("app/index.js")).unwrap(),
        b"console.log(1)",
        "the unexcluded file was moved out and replaced by a symlink"
    );
    assert!(!cold.join("app/node_modules").exists());
}

#[test]
fn the_size_window_skips_tiny_and_enormous_files() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("tiny.txt"), b"hi").unwrap();
    fs::write(watch.join("just-right.bin"), vec![0u8; 4096]).unwrap();
    fs::write(watch.join("huge.img"), vec![0u8; 64 * 1024]).unwrap();

    let entries = scan(&watch);
    let scope = Scope::build(&[], &[], 1024, Some(8192)).unwrap();
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &scope,
        &Guards::permissive(),
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );

    assert_eq!(report.moved(), 1);
    assert_eq!(report.excluded(), 2);
    assert!(cold.join("just-right.bin").is_file());
    assert!(watch.join("tiny.txt").is_file());
    assert!(watch.join("huge.img").is_file());
}

/// A file that changes size between being scanned and being copied is a file something is
/// using. Copying it anyway would put a torn view on the cold tier and then delete the
/// only complete copy, so the discovery is exactly what the move must refuse.
#[test]
fn a_source_that_changed_since_the_scan_is_not_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let src = watch.join("shrinking.bin");
    fs::write(&src, vec![b'a'; 4096]).unwrap();

    let entries = scan(&watch);
    let entry = find(&entries, "shrinking.bin");

    // Shrink behind the scan's back, the way a truncating writer would.
    fs::write(&src, vec![b'b'; 512]).unwrap();

    let err = disk_management::move_file_with_symlink(&cold, entry)
        .expect_err("a source that changed must not be moved");
    match err {
        DiskError::SourceChanged {
            expected, actual, ..
        } => {
            assert_eq!(expected, 4096, "expected size comes from the scan");
            assert_eq!(actual, 512, "actual size is what the source has now");
        }
        other => panic!("expected SourceChanged, got {other:?}"),
    }

    assert!(
        fs::read(&src).is_ok(),
        "the source must be left where it is"
    );
    assert_eq!(fs::read(&src).unwrap().len(), 512);
    let leftovers: Vec<_> = fs::read_dir(&cold)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftovers.is_empty(),
        "no partial file may be left behind: {leftovers:?}"
    );
}

#[test]
fn a_file_something_else_has_open_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let held = watch.join("held.bin");
    fs::write(&held, b"being read right now").unwrap();
    fs::write(watch.join("idle.bin"), b"nobody wants this").unwrap();

    // A real second process holding a real descriptor, so the /proc scan is exercised
    // rather than mocked.
    let mut holder = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exec 3< '{}'; sleep 20", held.display()))
        .spawn()
        .expect("spawn a holder process");

    let target = FileId::of(&held).unwrap();
    let mut open = OpenFiles::snapshot();
    for _ in 0..50 {
        if open.contains(target) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        open = OpenFiles::snapshot();
    }
    let holder_seen = open.contains(target);

    let entries = scan(&watch);
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };
    let guards = Guards::new(open, true);
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &Scope::everything(),
        &guards,
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );

    let _ = holder.kill();
    let _ = holder.wait();

    assert!(holder_seen, "the holder process should have been observed");
    assert_eq!(report.in_use(), 1);
    assert_eq!(report.moved(), 1);
    assert!(
        !fs::symlink_metadata(&held).unwrap().is_symlink(),
        "a file held open must not be moved"
    );
    assert!(fs::read(&held).is_ok());
    assert!(!cold.join("held.bin").exists());
    assert!(cold.join("idle.bin").is_file());
}

#[test]
fn a_hardlinked_file_is_skipped_unless_the_guard_is_relaxed() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let original = watch.join("original.bin");
    fs::write(&original, b"shared bytes").unwrap();
    fs::hard_link(&original, watch.join("second-name.bin")).unwrap();

    let entries = scan(&watch);
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };

    let guarded = Guards::new(OpenFiles::default(), true);
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &Scope::everything(),
        &guarded,
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );
    assert_eq!(report.moved(), 0);
    assert_eq!(report.in_use(), 2, "both names of the pair are protected");

    let relaxed = Guards::new(OpenFiles::default(), false);
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &Scope::everything(),
        &relaxed,
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );
    assert_eq!(report.moved(), 2);
}

#[test]
fn a_full_tier_leaves_files_waiting_instead_of_failing_them() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("cold.bin"), b"needs a home").unwrap();

    let entries = scan(&watch);
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &Scope::everything(),
        &Guards::permissive(),
        SystemTime::now(),
        |_| Ok(None), // the tier says it is out of space
    );

    assert_eq!(report.waiting_for_room(), 1);
    assert_eq!(report.failed(), 0, "a full cold disk is not an error");
    assert_eq!(fs::read(watch.join("cold.bin")).unwrap(), b"needs a home");
}

#[test]
fn a_sweep_reports_failures_without_stopping_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("good.bin"), b"good").unwrap();
    fs::write(watch.join("blocked.bin"), b"blocked copy").unwrap();
    fs::write(cold.join("blocked.bin"), b"a different length entirely").unwrap();

    let entries = scan(&watch);
    let policy = Policy {
        min_idle: std::time::Duration::ZERO,
        observed_access_pin: 0,
        limit: 10,
        dry_run: false,
    };
    let report = file_movement::migrate_least_used(
        &entries,
        &UsageTracker::new(),
        &policy,
        &Scope::everything(),
        &Guards::permissive(),
        SystemTime::now(),
        |_| Ok(Some(cold.clone())),
    );

    assert_eq!(report.failed(), 1);
    assert_eq!(report.moved(), 1);
    assert!(report
        .records
        .iter()
        .any(|record| record.path.ends_with("good.bin") && record.outcome == FileOutcome::Moved));
    assert!(fs::read_to_string(watch.join("good.bin")).unwrap() == "good");
}
