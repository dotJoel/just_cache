//! End-to-end behaviour of `just_cache scrub` against real temporary trees, driven
//! through the binary for the user-visible contract (output and exit codes).
//!
//! The cases are the ones the issue calls out, plus the two that are easy to fake and
//! therefore worth proving for real:
//!
//! * a copy corrupted by hand after sync (same size, so only a checksum can see it) is
//!   caught and repaired from its sibling;
//! * the last surviving copy being corrupt is *marked damaged and reported*, never
//!   deleted;
//! * `--dry-run` reports the repair and changes nothing;
//! * a second scrub skips locations an earlier run already verified (the resume story,
//!   since killing a run mid-way is not deterministic);
//! * hashing a sparse file does not materialize its holes (the §9 concern, measured);
//! * `--rate` actually paces the read.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use just_cache::catalog::{Catalog, CATALOG_NAME};

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

/// Flip bytes in place without changing the length, so only a checksum can detect it.
fn corrupt_in_place(path: &Path) {
    let mut bytes =
        fs::read(path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    assert!(
        bytes.len() >= 4,
        "payload too small to corrupt meaningfully"
    );
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    bytes[mid + 1] ^= 0x0f;
    fs::write(path, &bytes).unwrap_or_else(|err| panic!("cannot write {}: {err}", path.display()));
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

fn scrub(catalog: &Path, extra: &[&str]) -> Output {
    let mut command = bin();
    command.arg("scrub").arg("--catalog").arg(catalog);
    command.args(extra);
    command.output().expect("just_cache runs")
}

/// Like [`sync`], but with more than one `--dest` root (a replicated tier).
fn sync_multi(watch: &Path, dests: &[&Path], catalog: &Path) -> Output {
    let mut command = bin();
    command.args(["catalog", "sync", "--watch"]).arg(watch);
    for dest in dests {
        command.arg("--dest").arg(dest);
    }
    command.arg("--catalog").arg(catalog);
    command.output().expect("just_cache runs")
}

fn summary(catalog: &Path) -> just_cache::catalog::ScrubSummary {
    Catalog::open(catalog)
        .expect("catalog opens")
        .scrub_summary()
        .unwrap()
}

/// A copy corrupted by hand after sync, same size, is caught and repaired from its sibling.
#[test]
fn a_copy_corrupted_by_hand_is_repaired_from_its_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(64 * 1024);
    fs::write(hot.join("a.bin"), &bytes).unwrap();
    fs::write(cold.join("a.bin"), &bytes).unwrap();

    // Two locations of one object: the hot file and its cold mirror.
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    assert_eq!(summary(&catalog).locations, 2);

    let cold_copy = cold.join("a.bin");
    corrupt_in_place(&cold_copy);
    assert_ne!(
        fs::read(&cold_copy).unwrap(),
        bytes,
        "the copy must be corrupt"
    );

    let output = scrub(&catalog, &[]);
    // A repair is still a finding: bitrot happened, and cron has to see it once.
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("repaired"),
        "the repair must be reported:\n{text}"
    );
    let expected_source = fs::canonicalize(&hot).unwrap().join("a.bin");
    assert!(
        text.contains(&expected_source.display().to_string()),
        "the repair source is the verified sibling:\n{text}"
    );
    assert_eq!(
        fs::read(&cold_copy).unwrap(),
        bytes,
        "the corrupt copy now holds the verified bytes"
    );
    assert_eq!(
        fs::read(hot.join("a.bin")).unwrap(),
        bytes,
        "the good sibling is untouched"
    );
    let after = summary(&catalog);
    assert_eq!(after.verified, 2, "both locations are verified now");
    assert_eq!(after.never_scrubbed, 0);
    assert_eq!(after.damaged, 0);
}

/// The last surviving copy being corrupt is marked, reported, and never deleted.
#[test]
fn the_last_surviving_copy_corrupt_is_marked_not_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(4096);
    fs::write(hot.join("only.bin"), &bytes).unwrap();

    // One location only: nothing to repair from.
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    assert_eq!(summary(&catalog).locations, 1);

    let only = hot.join("only.bin");
    corrupt_in_place(&only);
    let corrupt_bytes = fs::read(&only).unwrap();

    let output = scrub(&catalog, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(text.contains("DAMAGED"), "damage must be named:\n{text}");
    assert!(
        text.contains("nothing deleted"),
        "the report must say nothing was removed:\n{text}"
    );
    assert_eq!(
        fs::read(&only).unwrap(),
        corrupt_bytes,
        "the corrupt bytes are kept, not deleted"
    );
    let after = summary(&catalog);
    assert_eq!(after.damaged, 1, "the object is marked damaged");
    assert_eq!(after.verified, 0, "a damaged location is not 'verified'");
}

/// `--dry-run` reports what it would repair and changes neither bytes nor the catalog.
#[test]
fn dry_run_reports_the_repair_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(32 * 1024);
    fs::write(hot.join("a.bin"), &bytes).unwrap();
    fs::write(cold.join("a.bin"), &bytes).unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);

    let cold_copy = cold.join("a.bin");
    corrupt_in_place(&cold_copy);
    let corrupt_bytes = fs::read(&cold_copy).unwrap();

    let dry = scrub(&catalog, &["--dry-run"]);
    assert_exit(&dry, 1);
    let text = stdout(&dry);
    assert!(
        text.contains("would repair"),
        "dry run must say what it would do:\n{text}"
    );
    assert!(
        text.contains("dry run"),
        "and say it changed nothing:\n{text}"
    );
    assert_eq!(
        fs::read(&cold_copy).unwrap(),
        corrupt_bytes,
        "dry run must not write the repair"
    );
    // No last-verified state was recorded either: the dry run is invisible to the catalog.
    let after_dry = summary(&catalog);
    assert_eq!(
        after_dry.never_scrubbed, 2,
        "dry run must not persist scrub state"
    );
    assert_eq!(after_dry.verified, 0);

    // The real run does the repair.
    let real = scrub(&catalog, &[]);
    assert_exit(&real, 1);
    assert!(stdout(&real).contains("repaired"), "{}", stdout(&real));
    assert_eq!(fs::read(&cold_copy).unwrap(), bytes);

    // And a second run finds everything already verified and clean.
    let second = scrub(&catalog, &[]);
    assert_exit(&second, 0);
    let text = stdout(&second);
    assert!(
        text.contains("already verified"),
        "resume must be visible:\n{text}"
    );
    assert!(text.contains("no corruption found"), "{text}");
}

/// A second scrub skips locations an earlier run verified (the resume contract).
#[test]
fn a_second_scrub_skips_already_verified_locations() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    for name in ["one.bin", "two.bin"] {
        let bytes = payload(8192);
        fs::write(hot.join(name), &bytes).unwrap();
        fs::write(cold.join(name), &bytes).unwrap();
    }
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    assert_eq!(summary(&catalog).locations, 4);

    let first = scrub(&catalog, &[]);
    assert_exit(&first, 0);
    assert!(
        stdout(&first).contains("locations: 4 (4 verified"),
        "the first run verifies everything:\n{}",
        stdout(&first)
    );

    let second = scrub(&catalog, &[]);
    assert_exit(&second, 0);
    let text = stdout(&second);
    assert!(
        text.contains("locations: 4 (0 verified, 4 already verified (skipped)"),
        "the second run re-reads nothing:\n{text}"
    );
    assert_eq!(summary(&catalog).verified, 4);
}

/// Hashing a sparse file reads through its holes without materializing them (§9).
///
/// This is the honest measurement the design asked for: `st_blocks` is compared before and
/// after a scrub. The test skips loudly on a filesystem that does not represent holes
/// sparsely, because there it cannot prove anything.
#[test]
fn scrubbing_a_sparse_file_does_not_materialize_its_holes() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();

    let apparent: u64 = 64 * 1024 * 1024;
    let sparse = hot.join("disk.img");
    {
        let mut file = fs::File::create(&sparse).unwrap();
        file.write_all(&[0x5au8; 4096]).unwrap();
        file.seek(SeekFrom::Start(apparent / 2)).unwrap();
        file.write_all(&[0xa5u8; 4096]).unwrap();
        file.set_len(apparent).unwrap();
    }

    let allocated = |path: &Path| fs::metadata(path).unwrap().blocks() * 512;
    let before_sync = allocated(&sparse);
    if before_sync >= apparent / 2 {
        eprintln!("skipping: filesystem does not represent holes sparsely");
        return;
    }

    assert_exit(&sync(&hot, &cold, &catalog), 0);
    let before_scrub = allocated(&sparse);

    let output = scrub(&catalog, &[]);
    assert_exit(&output, 0);
    let after_scrub = allocated(&sparse);

    assert!(
        after_scrub < apparent / 2,
        "the file must still be sparse after a scrub: {after_scrub} allocated of {apparent}"
    );
    assert!(
        after_scrub <= before_scrub + 1024 * 1024,
        "scrubbing materialized blocks: {before_scrub} -> {after_scrub} allocated bytes"
    );
}

/// `--rate` paces the read; without it there is no throttle.
#[test]
fn the_rate_flag_paces_the_read() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(256 * 1024);
    fs::write(hot.join("a.bin"), &bytes).unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);

    // 256 KiB at 512 KiB/s is about half a second of budget.
    let start = Instant::now();
    let limited = scrub(&catalog, &["--rate", "512"]);
    let limited_elapsed = start.elapsed();
    assert_exit(&limited, 0);
    assert!(
        limited_elapsed >= Duration::from_millis(350),
        "a 512 KiB/s budget must pace ~0.5s for 256 KiB, took {limited_elapsed:?}"
    );

    // Reset the verification so the whole point is read again, this time unlimited.
    fs::remove_file(&catalog).unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    let start = Instant::now();
    let fast = scrub(&catalog, &[]);
    let fast_elapsed = start.elapsed();
    assert_exit(&fast, 0);
    assert!(
        fast_elapsed < Duration::from_millis(350),
        "an unlimited scrub must not wait on the limiter, took {fast_elapsed:?}"
    );
}

/// A missing catalog, and a zero rate, are usage errors (exit 2).
#[test]
fn a_bad_invocation_is_exit_two() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.sqlite");
    let output = scrub(&missing, &[]);
    assert_exit(&output, 2);
    assert!(
        stderr(&output).contains("not an existing file"),
        "{}",
        stderr(&output)
    );

    let catalog = tmp.path().join("catalog.sqlite");
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), b"payload").unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    let zero = scrub(&catalog, &["--rate", "0"]);
    assert_exit(&zero, 2);
    assert!(
        stderr(&zero).contains("at least 1 KiB/s"),
        "{}",
        stderr(&zero)
    );
}

/// `audit --catalog` reports "never scrubbed" rather than implying the catalog vouches
/// for bytes nobody has read back.
#[test]
fn audit_can_report_never_scrubbed_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), b"payload").unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);

    let output = bin()
        .arg("audit")
        .arg("--watch")
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .unwrap();
    let text = stdout(&output);
    assert!(
        text.contains("1 never scrubbed"),
        "audit must name the unverified copy:\n{text}"
    );

    // After a scrub, the same audit says it is verified.
    assert_exit(&scrub(&catalog, &[]), 0);
    let output = bin()
        .arg("audit")
        .arg("--watch")
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .unwrap();
    let text = stdout(&output);
    assert!(text.contains("1 verified, 0 never scrubbed"), "{text}");
}

/// A corrupt copy on a genuinely different filesystem is repaired across the mount point,
/// exercising `copy_contents`' extents path rather than an in-filesystem clone.
#[test]
fn a_copy_on_a_second_filesystem_is_repaired_across_the_mount() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = support::work_dir(&second, "scrub-xdev");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    let bytes = payload(128 * 1024);
    fs::write(hot.join("a.bin"), &bytes).unwrap();
    fs::write(cold.path().join("a.bin"), &bytes).unwrap();

    support::assert_cross_device(&hot, cold.path());
    assert_exit(&sync(&hot, cold.path(), &catalog), 0);

    let cold_copy: PathBuf = cold.path().join("a.bin");
    corrupt_in_place(&cold_copy);
    let output = scrub(&catalog, &[]);
    assert_exit(&output, 1);
    assert!(stdout(&output).contains("repaired"), "{}", stdout(&output));
    assert_eq!(
        fs::read(&cold_copy).unwrap(),
        bytes,
        "the copy is repaired byte-for-byte across the mount point"
    );
    assert!(
        support::partial_files(cold.path()).is_empty(),
        "no .just_cache-partial-* left on the tier: {:?}",
        support::partial_files(cold.path())
    );
}

/// A symlink at a recorded location is *not* a copy, even when it resolves to
/// byte-identical bytes: scrub reports it unreadable and does not record it verified.
///
/// The link's target hashes exactly to the object, so a stat that follows the final
/// component (`fs::metadata`) would call this location clean and write a `scrub_state`
/// row vouching for a name whose target can be removed without the catalog noticing.
#[test]
fn a_symlink_at_a_recorded_location_is_unreadable_not_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(16 * 1024);
    fs::write(hot.join("a.bin"), &bytes).unwrap();
    fs::write(cold.join("a.bin"), &bytes).unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    assert_eq!(summary(&catalog).locations, 2);

    // Replace the cold copy with a relative symlink to the byte-identical hot file.
    let cold_copy = cold.join("a.bin");
    fs::remove_file(&cold_copy).unwrap();
    std::os::unix::fs::symlink(Path::new("../hot/a.bin"), &cold_copy).unwrap();
    assert_eq!(
        fs::metadata(&cold_copy).unwrap().len(),
        bytes.len() as u64,
        "the link must resolve to byte-identical content for this test to mean anything"
    );

    let output = scrub(&catalog, &[]);
    // A location that is not a copy is a finding, not a pass.
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("unreadable"),
        "the symlink must be reported unreadable:\n{text}"
    );
    assert!(
        text.contains(&cold_copy.display().to_string()),
        "the report must name the symlink location:\n{text}"
    );

    let after = summary(&catalog);
    assert_eq!(after.verified, 1, "only the real hot copy is verified");
    assert_eq!(
        after.never_scrubbed, 1,
        "the symlink location must not be recorded as verified"
    );
}

/// Sanity: the default catalog path really is beside the watch root, so a `--catalog`-less
/// scrub of the default file works. (The command requires `--catalog` today; this pins the
/// name the default path uses.)
#[test]
fn the_default_catalog_name_is_beside_the_watch_root() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    assert_eq!(
        just_cache::Catalog::default_path(&watch),
        watch.join(CATALOG_NAME)
    );
}

/// Two locations of one object, both rotted in place, are both marked damaged when the
/// only repair candidate is itself an already-verified copy that has rotted since (#75).
///
/// The setup is the mixed state the bug needs: `hot` was verified by an earlier run and
/// `cold` was recorded afterwards, so only `cold` is read in the first pass. `cold` comes
/// back corrupt, which selects `hot` as the fallback repair source — and `hot`, re-read,
/// is corrupt too. Before the fix that second verdict was discarded: `hot` stayed
/// `AlreadyVerified`, was never marked damaged, and every later scrub would skip it
/// without reading it.
#[test]
fn a_rotted_already_verified_sibling_is_marked_damaged() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(16 * 1024);
    fs::write(hot.join("a.bin"), &bytes).unwrap();

    // The cold root exists but holds nothing yet, so the first sync records one location.
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    assert_eq!(summary(&catalog).locations, 1);
    assert_exit(&scrub(&catalog, &[]), 0);
    assert_eq!(summary(&catalog).verified, 1, "hot is verified by this run");

    // The cold copy appears now and is ingested without touching hot's scrub_state, so
    // hot is already-verified while cold has never been read.
    fs::write(cold.join("a.bin"), &bytes).unwrap();
    assert_exit(&sync(&hot, &cold, &catalog), 0);
    let before = summary(&catalog);
    assert_eq!(before.locations, 2);
    assert_eq!(before.verified, 1);
    assert_eq!(before.never_scrubbed, 1);

    // Rot both copies in place, same size: only a checksum can see it.
    corrupt_in_place(&hot.join("a.bin"));
    corrupt_in_place(&cold.join("a.bin"));

    let output = scrub(&catalog, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(text.contains("DAMAGED"), "damage must be named:\n{text}");
    assert!(
        text.contains("nothing deleted"),
        "the report must say nothing was removed:\n{text}"
    );

    // Both locations are marked and neither is counted as verified any more. This is the
    // crux: the already-verified sibling must not keep its scrub_state row.
    let after = summary(&catalog);
    assert_eq!(after.damaged, 2, "both locations are marked damaged");
    assert_eq!(after.verified, 0, "a damaged location is not 'verified'");
    assert_eq!(
        after.never_scrubbed, 2,
        "the dropped verdict must force a re-read next time"
    );
    assert!(
        text.contains("already verified (skipped)"),
        "the rotted fallback must not still be reported as a skip:\n{text}"
    );
    assert!(
        !text.contains("1 already verified"),
        "the failed fallback must not be counted as already verified:\n{text}"
    );
}

/// A clean already-verified sibling *after* a rotted one in the group is found and used as
/// the repair source; the rotted candidate is not mistaken for a source (#75).
#[test]
fn a_clean_sibling_later_in_the_group_is_the_repair_source() {
    let tmp = tempfile::tempdir().unwrap();
    // Names are prefixed so the canonical tier strings sort in a known order: the rotted
    // already-verified copy (`a`) comes before the clean one (`m`) in the group.
    let a_tier = tmp.path().join("01-a");
    let watch = tmp.path().join("02-m");
    let z_tier = tmp.path().join("03-z");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&a_tier).unwrap();
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&z_tier).unwrap();
    let bytes = payload(24 * 1024);
    fs::write(a_tier.join("a.bin"), &bytes).unwrap();
    fs::write(watch.join("a.bin"), &bytes).unwrap();

    // Two locations are ingested and verified; the third tier is empty for now.
    assert_exit(&sync_multi(&watch, &[&a_tier, &z_tier], &catalog), 0);
    assert_eq!(summary(&catalog).locations, 2);
    assert_exit(&scrub(&catalog, &[]), 0);
    assert_eq!(summary(&catalog).verified, 2);

    // A copy appears on the third tier and is ingested unverified.
    fs::write(z_tier.join("a.bin"), &bytes).unwrap();
    assert_exit(&sync_multi(&watch, &[&a_tier, &z_tier], &catalog), 0);
    assert_eq!(summary(&catalog).locations, 3);
    assert_eq!(summary(&catalog).never_scrubbed, 1);

    // Rot the first (already-verified) and third (unread) copies; the middle stays clean.
    corrupt_in_place(&a_tier.join("a.bin"));
    corrupt_in_place(&z_tier.join("a.bin"));

    let output = scrub(&catalog, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    let clean = fs::canonicalize(&watch).unwrap().join("a.bin");
    assert!(
        text.contains(&clean.display().to_string()),
        "the clean sibling later in the group must be the repair source:\n{text}"
    );
    assert_eq!(
        fs::read(a_tier.join("a.bin")).unwrap(),
        bytes,
        "the rotted already-verified copy is repaired from the later clean sibling"
    );
    assert_eq!(
        fs::read(z_tier.join("a.bin")).unwrap(),
        bytes,
        "the corrupt unread copy is repaired too"
    );
    assert_eq!(
        fs::read(watch.join("a.bin")).unwrap(),
        bytes,
        "the clean source is untouched"
    );
    let after = summary(&catalog);
    assert_eq!(after.damaged, 0, "a repair is not damage");
    assert_eq!(after.verified, 3, "every copy now verifies");
}

/// `sync` with more than one `--dest` root records every copy in one object group.
#[test]
fn sync_accepts_multiple_destinations() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let one = tmp.path().join("one");
    let two = tmp.path().join("two");
    let catalog = tmp.path().join("catalog.sqlite");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&one).unwrap();
    fs::create_dir_all(&two).unwrap();
    let bytes = payload(1024);
    for dir in [&watch, &one, &two] {
        fs::write(dir.join("a.bin"), &bytes).unwrap();
    }
    assert_exit(&sync_multi(&watch, &[&one, &two], &catalog), 0);
    assert_eq!(summary(&catalog).locations, 3);
}

// ---------------------------------------------------------------------------
// Offline-volume copies (#181)
//
// A copy on an `offline` tier lives on a disk a person inserts, sealed in the envelope, and
// the object id is the checksum of its *plaintext*. The scrub has to read it back and hash
// it when the volume is in the drive, report it by its volume identity when it is not, and
// repair it from a verified sibling like any other copy — never calling an unplugged disk
// corrupt, and never calling it healthy.
// ---------------------------------------------------------------------------

const OFFLINE_KEY_ENV: &str = "JC_SCRUB_OFFLINE_KEY";
const OFFLINE_KEY_HEX: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0\
                               f0e1d2c3b4a5968778695a4b3c2d1e0f";

/// The commands that load `tiers.toml` need the envelope key the config names, from the
/// environment (never argv), exactly as the mover does.
fn offline_bin() -> Command {
    let mut command = bin();
    command.env(OFFLINE_KEY_ENV, OFFLINE_KEY_HEX);
    command
}

struct Offline {
    dir: tempfile::TempDir,
    hot: PathBuf,
    cold: PathBuf,
    vol: PathBuf,
    catalog: PathBuf,
}

/// A hot tree and a local cold mirror of one file, an empty volume mount, and `tiers.toml`
/// beside the watch root (the placement the catalog shares) naming the mount as an `offline`
/// tier. The `tiers.toml` is itself a synced file, so it is one more location the scrub
/// verifies — the counts below include it.
fn offline_fixture(payload: &[u8]) -> Offline {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot");
    let cold = dir.path().join("cold");
    let vol = dir.path().join("vol");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::create_dir_all(&vol).unwrap();
    fs::write(hot.join("shows/movie.mkv"), payload).unwrap();
    fs::write(cold.join("shows/movie.mkv"), payload).unwrap();
    fs::write(
        hot.join("tiers.toml"),
        format!(
            "[tiers.drawer]\n\
             kind = \"offline\"\n\
             path = \"{}\"\n\
             volatility = \"persistent\"\n\
             recall = \"hours\"\n\
             copies = 1\n\
             vaults = [\"shelf-a\"]\n\
             encryption_key = \"${OFFLINE_KEY_ENV}\"\n",
            vol.display()
        ),
    )
    .unwrap();
    Offline {
        catalog: hot.join(CATALOG_NAME),
        hot,
        cold,
        vol,
        dir,
    }
}

/// Record an `offline` location for `payload`'s object and seal `sealed` (the plaintext to
/// write) onto the volume there — exactly the catalog row and sealed bytes a sweep onto the
/// volume leaves, set up directly so the test controls what the volume holds.
fn add_offline_copy(f: &Offline, payload: &[u8], sealed: &[u8]) {
    let digest = just_cache::digest::bytes_digest(payload);
    Catalog::open(&f.catalog)
        .unwrap()
        .record_replica(
            digest.as_bytes(),
            "drawer",
            "drawer-01/shows/movie.mkv",
            true,
            Some(digest.as_bytes()),
        )
        .unwrap();
    let source = f.dir.path().join("seal-source.bin");
    fs::write(&source, sealed).unwrap();
    let key = just_cache::envelope::Key::from_hex(OFFLINE_KEY_HEX).unwrap();
    just_cache::offline::export(&key, &source, &f.vol.join("shows/movie.mkv")).unwrap();
}

fn set_volume(f: &Offline, id: &str, state: &str) {
    let output = offline_bin()
        .args(["volume", "set", id, state])
        .args(["--tier", "drawer", "--catalog"])
        .arg(&f.catalog)
        .output()
        .expect("just_cache runs");
    assert_exit(&output, 0);
}

fn offline_scrub(f: &Offline, extra: &[&str]) -> Output {
    let mut command = offline_bin();
    command
        .arg("scrub")
        .arg("--catalog")
        .arg(&f.catalog)
        .args(extra);
    command.output().expect("just_cache runs")
}

/// A copy on an `offline` tier is read back, decrypted, and hashed when its volume is in the
/// drive — the same verdict a local copy gets, and a failure to verify is what breaks it.
#[test]
fn a_present_volume_copy_is_verified() {
    let bytes = payload(32 * 1024);
    let f = offline_fixture(&bytes);
    assert_exit(&sync(&f.hot, &f.cold, &f.catalog), 0);
    set_volume(&f, "drawer-01", "mounted");
    add_offline_copy(&f, &bytes, &bytes);

    let output = offline_scrub(&f, &[]);
    assert_exit(&output, 0);
    let text = stdout(&output);
    assert!(
        !text.contains("unavailable: tier"),
        "the volume is present: {text}"
    );
    assert!(!text.contains("DAMAGED"), "{text}");
    let after = summary(&f.catalog);
    assert_eq!(
        after.verified, after.locations,
        "every location, the offline copy included, must be verified: {text}"
    );
    assert_eq!(after.damaged, 0);
}

/// A copy on an offline tier whose volume is *not* in the drive is reported unavailable by
/// its volume identity: never corrupt (nothing was read), never healthy (nothing was
/// proved), and not a missing file (the row names a disk, not a path that has gone).
#[test]
fn an_absent_volume_is_reported_unavailable_by_volume_identity() {
    let bytes = payload(8 * 1024);
    let f = offline_fixture(&bytes);
    assert_exit(&sync(&f.hot, &f.cold, &f.catalog), 0);
    set_volume(&f, "drawer-01", "in_vault");
    add_offline_copy(&f, &bytes, &bytes);

    let output = offline_scrub(&f, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("unavailable: tier `drawer` volume `drawer-01`"),
        "the volume must be named by its identity: {text}"
    );
    assert!(
        text.contains("drawer-01/shows/movie.mkv"),
        "the storage key must be named: {text}"
    );
    assert!(
        !text.contains("missing:"),
        "an absent volume is not a missing file: {text}"
    );
    assert!(
        !text.contains("DAMAGED"),
        "an absent volume is not corrupt: {text}"
    );
    // Not healthy either: the copy was not verified this run, and the local copies still
    // were — the report has to keep the two apart.
    let after = summary(&f.catalog);
    assert_eq!(after.damaged, 0);
    assert_eq!(
        after.never_scrubbed, 1,
        "the offline copy is unverified, not verified: {text}"
    );
    assert!(
        text.contains("verified"),
        "the local copies still verify: {text}"
    );
}

/// A sealed copy that decrypts but does not match the recorded digest is rot, and with no
/// verified sibling to repair from it is marked damaged — and never deleted.
#[test]
fn a_sealed_copy_that_does_not_match_is_marked_damaged_but_kept() {
    let bytes = payload(16 * 1024);
    let f = offline_fixture(&bytes);
    assert_exit(&sync(&f.hot, &f.cold, &f.catalog), 0);
    set_volume(&f, "drawer-01", "mounted");
    // The envelope is intact, so it decrypts; the plaintext inside is simply not the object.
    add_offline_copy(&f, &bytes, b"a different plaintext entirely");
    // No verified sibling: the local copies go, leaving the offline copy as the only one.
    fs::remove_file(f.hot.join("shows/movie.mkv")).unwrap();
    fs::remove_file(f.cold.join("shows/movie.mkv")).unwrap();
    let stored = f.vol.join("shows/movie.mkv");
    let before = fs::read(&stored).unwrap();

    let output = offline_scrub(&f, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(text.contains("DAMAGED"), "damage must be named: {text}");
    assert_eq!(summary(&f.catalog).damaged, 1);
    assert_eq!(
        fs::read(&stored).unwrap(),
        before,
        "the damaged copy is reported, never deleted"
    );
}

/// A corrupt copy on an offline tier is repaired from a sibling that verifies — re-exported
/// as a fresh envelope, read back, and only then recorded verified.
#[test]
fn a_corrupt_offline_copy_is_repaired_from_a_verified_sibling() {
    let bytes = payload(32 * 1024);
    let f = offline_fixture(&bytes);
    assert_exit(&sync(&f.hot, &f.cold, &f.catalog), 0);
    set_volume(&f, "drawer-01", "mounted");
    add_offline_copy(&f, &bytes, b"a different plaintext entirely");
    let stored = f.vol.join("shows/movie.mkv");

    let output = offline_scrub(&f, &[]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("repaired ") && text.contains("from verified copy"),
        "the repair must be named, and its verified source: {text}"
    );
    assert!(!text.contains("DAMAGED"), "{text}");

    // The sealed copy now decrypts *and* hashes to the recorded digest: a real repair, not a
    // rewrite of the bytes that were already there.
    let key = just_cache::envelope::Key::from_hex(OFFLINE_KEY_HEX).unwrap();
    let digest = just_cache::digest::bytes_digest(&bytes);
    let plain = f.dir.path().join("readback.bin");
    just_cache::offline::read_verify_decrypt(&key, &stored, Some(&digest), &plain)
        .expect("the repaired copy must verify against the record");
    assert_eq!(fs::read(&plain).unwrap(), bytes);
    assert_eq!(summary(&f.catalog).damaged, 0, "a repair is not damage");
}

/// `--dry-run` reports the repair of an offline copy and changes neither the sealed bytes nor
/// the recorded state.
#[test]
fn dry_run_reports_the_offline_repair_and_changes_nothing() {
    let bytes = payload(16 * 1024);
    let f = offline_fixture(&bytes);
    assert_exit(&sync(&f.hot, &f.cold, &f.catalog), 0);
    set_volume(&f, "drawer-01", "mounted");
    add_offline_copy(&f, &bytes, b"a different plaintext entirely");
    let stored = f.vol.join("shows/movie.mkv");
    let before = fs::read(&stored).unwrap();

    let output = offline_scrub(&f, &["--dry-run"]);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("would repair"),
        "the dry run must name the repair it would make: {text}"
    );
    assert_eq!(
        fs::read(&stored).unwrap(),
        before,
        "a dry run writes no repair"
    );
    assert_eq!(
        summary(&f.catalog).damaged,
        0,
        "a dry run records no damage either"
    );
}
