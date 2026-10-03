//! End-to-end behaviour of `just_cache restore` against real temporary trees, driven
//! through the binary for the user-visible contract (output and exit codes), plus one
//! genuinely cross-device restore through `/dev/shm`.
//!
//! The cases that matter are the ones where getting it wrong loses data: a same-size hot
//! file with different bytes must be refused rather than clobbered, a broken symlink must
//! be repaired from the mirrored copy, restoring twice must be a no-op, and `--remove-copy`
//! must not drop the cold bytes until the fresh copy has verified.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use just_cache::catalog::{Catalog, CATALOG_NAME};
use just_cache::disk_management;
use just_cache::restore::{self, RestoreOutcome, RestoreRequest};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

/// Run `catalog sync` for the tree, creating the default catalog beside the watch root.
/// Asserts it succeeded: the restore tests below are only meaningful against a real
/// catalog, so a silent failure here would let them pass without one.
fn sync_catalog(watch: &Path, cold: &Path) {
    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(watch)
        .arg("--dest")
        .arg(cold)
        .output()
        .expect("catalog sync runs");
    assert_eq!(
        output.status.code(),
        Some(0),
        "catalog sync must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

/// A payload with no runs long enough to survive a lazy copy, non-zero so a zero-filled
/// destination is obvious.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251 + 1) as u8).collect()
}

/// Migrate `relative` out of `watch` onto `cold` with the real mover, leaving the symlink
/// a sweep would have left. Returns the hot path.
fn migrate(watch: &Path, cold: &Path, relative: &str) -> PathBuf {
    let entries = disk_management::list_files_recursive(watch).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.relative == Path::new(relative))
        .unwrap_or_else(|| panic!("{relative} should have been found"));
    disk_management::move_file_with_symlink(cold, entry).unwrap();
    watch.join(relative)
}

/// Run the restore subcommand and return (exit code, stdout, stderr).
fn restore_via_binary(
    hot: &Path,
    watch: &Path,
    cold: &Path,
    extra: &[&str],
) -> (i32, String, String) {
    let mut command = bin();
    command
        .arg("restore")
        .arg(hot)
        .arg("--watch")
        .arg(watch)
        .arg("--dest")
        .arg(cold);
    command.args(extra);
    let output = command.output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The library round trip: a real move, then a restore, leaving the cold copy in place.
#[test]
fn restore_materializes_a_migrated_file_and_keeps_the_cold_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows/season1")).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(64 * 1024);
    fs::write(watch.join("shows/season1/ep1.mkv"), &bytes).unwrap();

    let hot = migrate(&watch, &cold, "shows/season1/ep1.mkv");
    assert!(
        fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "the mover should have left a symlink"
    );

    let dests = vec![cold.clone()];
    let outcome = restore::restore(&RestoreRequest {
        path: &hot,
        watch: &watch,
        dests: &dests,
        remove_copy: false,
        catalog: None,
        object_tier_configs: &[],
        encryption_keys: &[],
    })
    .unwrap();

    match outcome {
        RestoreOutcome::Restored {
            cold_copy,
            bytes: n,
            removed_copy,
        } => {
            assert_eq!(cold_copy, cold.join("shows/season1/ep1.mkv"));
            assert_eq!(n as usize, bytes.len());
            assert!(!removed_copy, "the cold copy stays by default");
        }
        other => panic!("expected a restore, got {other:?}"),
    }

    assert!(
        !fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "restore must collapse the symlink into a real file"
    );
    assert_eq!(fs::read(&hot).unwrap(), bytes);
    assert!(
        cold.join("shows/season1/ep1.mkv").is_file(),
        "the cold copy must survive a default restore"
    );
}

/// The binary's user-visible contract for a plain restore.
#[test]
fn the_binary_restores_a_migrated_file_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(4096);
    fs::write(watch.join("clip.mov"), &bytes).unwrap();
    let hot = migrate(&watch, &cold, "clip.mov");

    let (code, stdout, stderr) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("restored"), "stdout: {stdout}");
    assert_eq!(fs::read(&hot).unwrap(), bytes, "the bytes read back");
    assert!(!fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert!(cold.join("clip.mov").is_file());
}

/// Restoring an already-present (identical) file is a no-op that exits 0.
#[test]
fn restoring_twice_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(2048);
    fs::write(watch.join("clip.mov"), &bytes).unwrap();
    let hot = migrate(&watch, &cold, "clip.mov");

    let (first_code, _, first_err) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(first_code, 0, "stderr: {first_err}");
    let after_first = fs::read(&hot).unwrap();

    // Second run: the path is now a real file whose content matches the cold copy.
    let (second_code, second_out, second_err) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(second_code, 0, "stderr: {second_err}");
    assert!(
        second_out.contains("already present"),
        "the no-op must say so: {second_out}"
    );
    assert_eq!(fs::read(&hot).unwrap(), after_first, "nothing changed");
    assert!(!fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert!(cold.join("clip.mov").is_file(), "cold copy still there");
}

/// A broken symlink with an intact mirrored cold copy is repaired.
#[test]
fn restore_repairs_a_broken_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(cold.join("shows/ep2.mkv"), b"episode two").unwrap();
    // The link points where the copy is not, so it does not resolve.
    let hot = watch.join("shows/ep2.mkv");
    link(Path::new("../../cold/vanished/ep2.mkv"), &hot);
    assert!(fs::metadata(&hot).is_err(), "the link must start broken");

    let (code, stdout, stderr) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("restored"), "stdout: {stdout}");
    assert_eq!(fs::read(&hot).unwrap(), b"episode two");
    assert!(!fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert!(cold.join("shows/ep2.mkv").is_file());
}

/// A same-size, different-content file at the hot path is refused and left untouched.
///
/// The size is identical on both sides, so a length-only comparison would call this a
/// match and clobber the hot file. That is exactly the failure this test exists to catch.
#[test]
fn a_same_size_different_content_hot_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("stale.bin"), b"AAAA").unwrap();
    fs::write(cold.join("stale.bin"), b"BBBB").unwrap();
    assert_eq!(
        fs::metadata(watch.join("stale.bin")).unwrap().len(),
        fs::metadata(cold.join("stale.bin")).unwrap().len(),
        "the two sides must be the same size for this test to mean anything"
    );

    let hot = watch.join("stale.bin");
    let (code, _, stderr) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(code, 1, "a mismatch is a hard error; stderr: {stderr}");
    assert!(
        stderr.contains("refusing") || stderr.contains("differs"),
        "the refusal must be explained: {stderr}"
    );
    assert_eq!(fs::read(&hot).unwrap(), b"AAAA", "hot bytes untouched");
    assert_eq!(
        fs::read(cold.join("stale.bin")).unwrap(),
        b"BBBB",
        "cold bytes untouched"
    );
}

/// `--remove-copy` drops the cold copy only after the restored file has verified.
#[test]
fn remove_copy_drops_the_cold_copy_after_a_verified_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(8192);
    fs::write(watch.join("clip.mov"), &bytes).unwrap();
    let hot = migrate(&watch, &cold, "clip.mov");

    let (code, stdout, stderr) = restore_via_binary(&hot, &watch, &cold, &["--remove-copy"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("cold copy removed"), "stdout: {stdout}");
    assert_eq!(fs::read(&hot).unwrap(), bytes);
    assert!(
        !cold.join("clip.mov").exists(),
        "the cold copy is dropped after the restored copy verified"
    );
    assert!(
        support::partial_files(&watch).is_empty(),
        "no .just_cache-partial-* leftovers: {:?}",
        support::partial_files(&watch)
    );
}

/// Several destinations: the copy lives on the second tier and is still found.
#[test]
fn a_copy_on_a_later_tier_is_found() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold_a = tmp.path().join("cold-a");
    let cold_b = tmp.path().join("cold-b");
    for dir in [&watch, &cold_a, &cold_b] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(cold_b.join("only-here.bin"), b"on the second tier").unwrap();
    let hot = watch.join("only-here.bin");
    link(Path::new("../cold-b/only-here.bin"), &hot);

    let mut command = bin();
    command
        .arg("restore")
        .arg(&hot)
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold_a)
        .arg("--dest")
        .arg(&cold_b);
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&hot).unwrap(), b"on the second tier");
}

/// Two tiers holding *different* bytes for one name is not guessed through.
#[test]
fn disagreeing_cold_copies_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold_a = tmp.path().join("cold-a");
    let cold_b = tmp.path().join("cold-b");
    for dir in [&watch, &cold_a, &cold_b] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(cold_a.join("clash.bin"), b"tier a bytes").unwrap();
    fs::write(cold_b.join("clash.bin"), b"tier b bytes").unwrap();

    let mut command = bin();
    command
        .arg("restore")
        .arg(watch.join("clash.bin"))
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold_a)
        .arg("--dest")
        .arg(&cold_b);
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("disagree"), "stderr: {stderr}");
    assert!(
        !watch.join("clash.bin").exists(),
        "nothing may be created when the copies disagree"
    );
}

/// A path with no cold copy to restore from fails loudly.
#[test]
fn a_path_with_no_cold_copy_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let hot = watch.join("never-moved.bin");

    let (code, _, stderr) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no cold copy"), "stderr: {stderr}");
    assert!(!hot.exists(), "and nothing is created at the path");
}

/// A path outside the watched tree is a usage error (exit 2), not a failed restore.
#[test]
fn a_path_outside_the_watched_tree_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let outside = tmp.path().join("elsewhere.bin");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(&outside, b"not ours").unwrap();

    let (code, _, stderr) = restore_via_binary(&outside, &watch, &cold, &[]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("not inside"), "stderr: {stderr}");
}

/// A real cross-device restore: the cold copy lives on `/dev/shm`, the hot tree does not.
#[test]
fn restore_brings_bytes_back_across_a_mount_point() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = support::work_dir(&second, "restore-xdev");
    fs::create_dir_all(watch.join("shows/season1")).unwrap();
    let bytes = payload(256 * 1024);
    fs::write(watch.join("shows/season1/ep1.mkv"), &bytes).unwrap();

    // The mover's cross-device path puts the copy on the other filesystem and leaves a
    // symlink; prove the crossing is real before trusting anything the restore does.
    support::assert_cross_device(&watch, cold.path());
    support::assert_rename_is_cross_device(&watch, cold.path());
    let hot = migrate(&watch, cold.path(), "shows/season1/ep1.mkv");
    assert!(fs::symlink_metadata(&hot).unwrap().is_symlink());
    let cold_copy = cold.path().join("shows/season1/ep1.mkv");
    assert!(cold_copy.is_file());

    let (code, stdout, stderr) = restore_via_binary(&hot, &watch, cold.path(), &[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("restored"), "stdout: {stdout}");
    assert_eq!(
        fs::read(&hot).unwrap(),
        bytes,
        "the bytes come back byte-for-byte across the mount point"
    );
    assert!(!fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert!(cold_copy.is_file(), "the cold copy stays by default");
    assert!(
        support::partial_files(&watch).is_empty(),
        "a restore must leave no .just_cache-partial-* file: {:?}",
        support::partial_files(&watch)
    );

    // And `--remove-copy` across the mount point drops the cold bytes after verifying.
    let bytes_again = fs::read(&hot).unwrap();
    let (code, _, stderr) =
        restore_via_binary(&hot, &watch, cold.path(), &["--remove-copy", "--quiet"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(fs::read(&hot).unwrap(), bytes_again, "still the same bytes");
    assert!(!cold_copy.exists(), "the cold copy is dropped");
    assert!(support::partial_files(&watch).is_empty());
}

/// With a catalog, the restored bytes are verified against the object's recorded checksum
/// and the restore succeeds when they agree. The catalog is named explicitly here to
/// exercise the `--catalog` form; the default-beside-the-watch case is covered below.
#[test]
fn restore_verifies_against_the_catalog_recorded_checksum() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    let bytes = payload(16 * 1024);
    fs::write(watch.join("shows/ep1.mkv"), &bytes).unwrap();
    let hot = migrate(&watch, &cold, "shows/ep1.mkv");
    sync_catalog(&watch, &cold);

    let catalog = Catalog::default_path(&watch);
    assert!(catalog.is_file(), "sync must have written the catalog");
    assert_eq!(
        catalog.file_name().unwrap().to_string_lossy(),
        CATALOG_NAME,
        "the default catalog lives beside the watch root"
    );

    let (code, stdout, stderr) = restore_via_binary(
        &hot,
        &watch,
        &cold,
        &["--catalog", catalog.to_str().unwrap()],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("restored"), "stdout: {stdout}");
    assert_eq!(
        fs::read(&hot).unwrap(),
        bytes,
        "the verified bytes read back"
    );
    assert!(!fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert!(cold.join("shows/ep1.mkv").is_file(), "the cold copy stays");
}

/// A cold copy that no longer matches the catalog's recorded checksum is refused, and
/// nothing is touched: the hot path keeps its symlink, the cold bytes are left alone, and
/// the private partial is cleaned up. This is the case a no-catalog restore cannot see —
/// it would copy the corrupt bytes back and only compare them against themselves.
///
/// The default catalog beside the watch root is consulted when it exists (no `--catalog`
/// flag is passed), so this also proves the open-if-present rule reaches restore.
#[test]
fn a_cold_copy_that_fails_the_catalog_checksum_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    let bytes = payload(4096);
    fs::write(watch.join("shows/ep1.mkv"), &bytes).unwrap();
    let hot = migrate(&watch, &cold, "shows/ep1.mkv");
    sync_catalog(&watch, &cold);
    assert!(
        Catalog::default_path(&watch).is_file(),
        "the default catalog must exist for this test to mean anything"
    );

    // Corrupt the cold copy after the catalog recorded the good checksum. The catalog's
    // digest is now the only thing that can tell these bytes are wrong.
    let cold_copy = cold.join("shows/ep1.mkv");
    fs::write(&cold_copy, b"bitrot, not the movie").unwrap();

    let (code, stdout, stderr) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(
        code, 1,
        "a checksum mismatch is a hard error; stderr: {stderr}"
    );
    assert!(
        stderr.contains("recorded checksum"),
        "the refusal must name the catalog checksum: {stderr}"
    );
    assert!(
        fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "the hot path must be left exactly as it was"
    );
    assert_eq!(
        fs::read(&cold_copy).unwrap(),
        b"bitrot, not the movie",
        "the cold bytes must be untouched"
    );
    assert!(
        support::partial_files(&watch).is_empty(),
        "no .just_cache-partial-* leftovers: {:?}",
        support::partial_files(&watch)
    );
    assert!(stdout.is_empty(), "nothing to report on stdout: {stdout}");
}

/// Without a catalog the behaviour is unchanged, and restore never brings one into being
/// (invariant 9): the bytes come back, and no catalog file appears beside the watch root.
#[test]
fn restore_without_a_catalog_is_unchanged_and_creates_none() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(1024);
    fs::write(watch.join("clip.mov"), &bytes).unwrap();
    let hot = migrate(&watch, &cold, "clip.mov");
    assert!(
        !Catalog::default_path(&watch).exists(),
        "no catalog exists before the restore"
    );

    let (code, stdout, stderr) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("restored"), "stdout: {stdout}");
    assert_eq!(fs::read(&hot).unwrap(), bytes);
    assert!(
        !Catalog::default_path(&watch).exists(),
        "restore must not create a catalog"
    );
}

/// An explicitly named catalog that is not there is a usage error (exit 2), not a quiet
/// fall back to filesystem-only verification.
#[test]
fn a_missing_named_catalog_is_a_usage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("clip.mov"), b"payload").unwrap();
    let hot = migrate(&watch, &cold, "clip.mov");

    let missing = tmp.path().join("nowhere.sqlite");
    let (code, _, stderr) = restore_via_binary(
        &hot,
        &watch,
        &cold,
        &["--catalog", missing.to_str().unwrap()],
    );
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("does not exist"),
        "the usage error must say so: {stderr}"
    );
    assert!(
        fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "nothing may be touched on a bad invocation"
    );
}

/// A symlink in the watched tree whose target resolves outside every `--dest` is a foreign
/// link, not a cold copy the mover left. A plain restore must not treat it as a copy, must
/// not create anything from it, and must leave the target alone. Anyone who can write the
/// tree can create such a link, so the target is an arbitrary file the operator can read.
#[test]
fn a_link_target_outside_every_dest_is_not_a_cold_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let outside = tmp.path().join("outside.bin");
    fs::write(&outside, b"outside-precious").unwrap();
    // The target is relative and leaves both the watch and every dest, exactly the shape
    // the mover's symlinks can be forged into.
    let hot = watch.join("movie.bin");
    link(Path::new("../outside.bin"), &hot);

    let (code, _, stderr) = restore_via_binary(&hot, &watch, &cold, &[]);
    assert_eq!(code, 1, "a foreign link is not a copy; stderr: {stderr}");
    assert!(stderr.contains("no cold copy"), "stderr: {stderr}");
    assert_eq!(
        fs::read(&outside).unwrap(),
        b"outside-precious",
        "the link target must never be read or copied"
    );
    assert!(
        fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "the link is left as it was"
    );
    assert!(
        !cold.join("movie.bin").exists(),
        "nothing is created under --dest"
    );
}

/// `--remove-copy` must never delete a foreign link's target: the copy it removes has to
/// be proved part of a `--dest` root first, or the command becomes deletion of an
/// arbitrary file the operator can read.
#[test]
fn remove_copy_refuses_a_link_target_outside_every_dest() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let outside = tmp.path().join("outside.bin");
    fs::write(&outside, b"outside-precious").unwrap();
    let hot = watch.join("movie.bin");
    link(Path::new("../outside.bin"), &hot);

    let (code, _, stderr) = restore_via_binary(&hot, &watch, &cold, &["--remove-copy"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("no cold copy"), "stderr: {stderr}");
    assert!(
        outside.exists(),
        "the foreign target must survive a --remove-copy run"
    );
    assert_eq!(fs::read(&outside).unwrap(), b"outside-precious");
    assert!(
        fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "the link is left as it was"
    );
    assert!(
        support::partial_files(&watch).is_empty(),
        "no .just_cache-partial-* leftovers: {:?}",
        support::partial_files(&watch)
    );
}

/// A path argument whose relative part contains a `..` component is refused before any
/// lookup, copy or delete. `absolute()` is lexical and does not resolve `..`, so
/// `T/../escaped.bin` strips to `../escaped.bin` and would otherwise name a file outside
/// the tree — with `--remove-copy` it would be deleted.
#[test]
fn a_path_with_a_parent_component_is_refused_before_any_work() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let outside = tmp.path().join("escaped.bin");
    fs::write(&outside, b"escapee").unwrap();

    // watch/../escaped.bin resolves to `outside`; the `..` is not resolved lexically.
    let hot = watch.join("../escaped.bin");
    let (code, _, stderr) = restore_via_binary(&hot, &watch, &cold, &["--remove-copy"]);
    assert_eq!(code, 2, "a bad path is a usage error; stderr: {stderr}");
    assert!(
        stderr.contains(".."),
        "the refusal must name the component: {stderr}"
    );
    assert_eq!(
        fs::read(&outside).unwrap(),
        b"escapee",
        "the file the `..` reaches must never be touched"
    );
    assert!(
        support::partial_files(&watch).is_empty(),
        "no .just_cache-partial-* leftovers on a refused path: {:?}",
        support::partial_files(&watch)
    );
}

/// `restore` reaches a copy on a tier this invocation was *not* given as a `--dest`, because
/// the catalog recorded it — the case the symlink provider's filesystem search cannot serve.
///
/// The copy is moved onto `cold-b` with the real mover (so the link names it), the catalog is
/// synced with `cold-b` as a root, and the restore is then run with only `cold-a` as `--dest`.
/// Without the catalog the link target is on `cold-b` (outside `cold-a`) and the mirrored path
/// under `cold-a` is absent, so nothing is found; naming the catalog resolves the recorded
/// location and the bytes come back through the same verified path.
#[test]
fn restore_reaches_a_copy_on_a_catalog_recorded_tier_the_dests_cannot_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold_a = tmp.path().join("cold-a");
    let cold_b = tmp.path().join("cold-b");
    for dir in [&watch, &cold_a, &cold_b] {
        fs::create_dir_all(dir).unwrap();
    }
    let bytes = payload(8192);
    fs::write(watch.join("clip.mov"), &bytes).unwrap();

    // Move onto cold-b: the hot path becomes a symlink to a copy on b.
    let hot = migrate(&watch, &cold_b, "clip.mov");
    assert!(fs::symlink_metadata(&hot).unwrap().is_symlink());
    let copy_b = cold_b.join("clip.mov");
    assert!(copy_b.is_file(), "the mover should have left the copy on b");

    // An explicit catalog *elsewhere*, so the restore below can be run with and without it:
    // the roots it records are the watch and cold-b.
    let catalog = tmp.path().join("elsewhere.sqlite");
    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold_b)
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "sync must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // With only --dest cold-a and no catalog, the filesystem search cannot reach the copy:
    // the link target is on cold-b, outside cold-a, and cold-a has no mirrored copy.
    let mut command = bin();
    command
        .arg("restore")
        .arg(&hot)
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold_a);
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "without the catalog this restore must fail; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no cold copy"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "nothing may be restored without the catalog"
    );

    // Naming the catalog, the recorded location on cold-b is reached and the bytes come back.
    let mut command = bin();
    command
        .arg("restore")
        .arg(&hot)
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold_a)
        .arg("--catalog")
        .arg(&catalog);
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "the catalog-recorded tier must serve the restore; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&hot).unwrap(), bytes, "the bytes come back");
    assert!(!fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert!(copy_b.is_file(), "the cold copy stays by default");
    assert!(support::partial_files(&watch).is_empty());

    // `--remove-copy` may drop a copy under a catalog-recorded tier root, after verification.
    let mut command = bin();
    command
        .arg("restore")
        .arg(&hot)
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold_a)
        .arg("--catalog")
        .arg(&catalog)
        .arg("--remove-copy");
    let output = command.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !copy_b.exists(),
        "the cold copy on the catalog-recorded tier is dropped after the restored file verifies"
    );
    assert_eq!(fs::read(&hot).unwrap(), bytes);
}
