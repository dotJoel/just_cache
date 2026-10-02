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
