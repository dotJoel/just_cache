//! Deterministic fault injection for the replication window: a destination that
//! disappears *between* two copies of one sweep, or while a freshly written copy is
//! being read back.
//!
//! The existing library tests (`src/replication.rs`, `tests/replication.rs`) construct a
//! destination failure by arranging the world **before** calling `replicate` — removing
//! the root up front. That arrangement cannot happen in production: the guard exists for
//! the moment a mount goes away *while the sweep is inside the loop*, and no test that
//! only drives the filesystem can enter that moment. The `JUST_CACHE_FAULT` hook in
//! `src/replication.rs` is the deterministic seam for it, and these tests drive the real
//! binary through it.
//!
//! Every test here asserts the four things the durability rule promises when the fault
//! lands:
//!
//! * the source is **kept** (never retired below the floor);
//! * the failure is **reported**, naming the disk that went away;
//! * the catalog is **not told** a copy is good when it is not;
//! * no `.just_cache-partial-*` file is left on either destination;
//! * and a later sweep **heals** what a healed sweep can heal.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn text_of(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
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

/// A sweep that replicates with the fault hook set for this one process only: the env
/// var is inherited by the child, so the injection cannot leak into the recovery run.
fn fault_sweep(watch: &Path, dests: &[&Path], fault: &str) -> Output {
    let mut command = bin();
    command.env("JUST_CACHE_FAULT", fault);
    run_sweep(command, watch, dests)
}

/// The same sweep with the hook unset — the recovery run, and the control that shows the
/// hook is inert when the variable is not set.
fn clean_sweep(watch: &Path, dests: &[&Path]) -> Output {
    run_sweep(bin(), watch, dests)
}

fn run_sweep(mut command: Command, watch: &Path, dests: &[&Path]) -> Output {
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

fn locate(catalog: &Path, query: &str) -> Output {
    bin()
        .args(["locate", query, "--catalog"])
        .arg(catalog)
        .output()
        .expect("just_cache runs")
}

/// A payload large enough that a half-written copy is a real state, not an artifact of
/// one buffer.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251 + 1) as u8).collect()
}

fn setup(watch_label: &str) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, Vec<u8>) {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join(watch_label);
    let dest_a = tmp.path().join(format!("{watch_label}-cold-a"));
    let dest_b = tmp.path().join(format!("{watch_label}-cold-b"));
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest_a).unwrap();
    fs::create_dir_all(&dest_b).unwrap();
    let bytes = payload(64 * 1024);
    fs::write(watch.join("clip.bin"), &bytes).unwrap();
    (tmp, watch, dest_a, dest_b, bytes)
}

/// The disk goes away after the first copy verifies and before the second is placed:
/// the source is kept, the report names the vanished disk, nothing half-written is left
/// behind — and a later sweep, on the disk back, finishes the job.
#[test]
fn a_destination_vanishing_between_copies_keeps_the_source_and_later_sweeps_heal() {
    let (_tmp, watch, dest_a, dest_b, bytes) = setup("fi-between");

    let output = fault_sweep(&watch, &[&dest_a, &dest_b], "unavailable-after=1");
    // The floor was not met, so the sweep reports a finding and exits non-zero.
    assert_exit(&output, 1);
    let text = text_of(&output);
    assert!(
        text.contains("under-replicated"),
        "the unmet floor must be reported:\n{text}"
    );
    assert!(
        text.contains("destination is gone") && text.contains(&dest_b.display().to_string()),
        "the report must name the disk that went away:\n{text}"
    );

    // The source is the last copy and must still be a real file with its bytes.
    let source_meta = fs::symlink_metadata(watch.join("clip.bin")).unwrap();
    assert!(
        !source_meta.is_symlink(),
        "a below-floor object must keep its source as a real file"
    );
    assert_eq!(
        fs::read(watch.join("clip.bin")).unwrap(),
        bytes,
        "the source bytes must be untouched"
    );

    // The copy that did verify is on the first disk; the second was never touched.
    assert_eq!(fs::read(dest_a.join("clip.bin")).unwrap(), bytes);
    assert!(
        !dest_b.join("clip.bin").exists(),
        "the vanished destination must not have been created (invariant 1)"
    );

    // No half-written copy anywhere: the fault happened *between* copies, but assert it
    // anyway so a future change of the injection point cannot quietly start leaving one.
    assert!(
        support::partial_files(&dest_a).is_empty() && support::partial_files(&dest_b).is_empty(),
        "no .just_cache-partial-* file may be left on either destination"
    );

    // Recovery: the disk is back (the directory exists again), the hook is unset, and the
    // same invocation finishes the offload it could not finish before.
    let healed = clean_sweep(&watch, &[&dest_a, &dest_b]);
    assert_exit(&healed, 0);
    assert!(
        fs::symlink_metadata(watch.join("clip.bin"))
            .unwrap()
            .is_symlink(),
        "the source is retired once the floor is met"
    );
    assert_eq!(fs::read(dest_b.join("clip.bin")).unwrap(), bytes);
    assert!(
        support::partial_files(&dest_a).is_empty() && support::partial_files(&dest_b).is_empty()
    );
}

/// The copy vanishes during its own read-back: the bytes were written and renamed into
/// place, then gone before they could be hashed. The mover must treat that as **unknown,
/// never good** — it does not count toward the floor, and the catalog is never told the
/// copy exists.
#[test]
fn a_copy_vanishing_during_read_back_is_unknown_and_never_recorded_good() {
    let (_tmp, watch, dest_a, dest_b, bytes) = setup("fi-readback");

    // A baseline sync first, so the catalog exists and the sweep's record_replicas has
    // somewhere to (not) write the unverifiable copy.
    let baseline = catalog_sync(&watch, &[&dest_a, &dest_b], 2);
    assert_exit(&baseline, 0);
    let catalog = watch.join(".just_cache-catalog.sqlite");

    let output = fault_sweep(&watch, &[&dest_a, &dest_b], "vanish-readback=1");
    assert_exit(&output, 1);
    let text = text_of(&output);
    assert!(
        text.contains("under-replicated"),
        "a copy that could not be verified must not satisfy the floor:\n{text}"
    );
    assert!(
        text.contains(&dest_a.display().to_string()),
        "the failed verification must be reported against its destination:\n{text}"
    );

    // The source is kept: one verified copy of two is below the floor.
    let source_meta = fs::symlink_metadata(watch.join("clip.bin")).unwrap();
    assert!(
        !source_meta.is_symlink(),
        "the source must not be retired below the floor"
    );
    assert_eq!(fs::read(watch.join("clip.bin")).unwrap(), bytes);

    // The vanished copy is gone — the mover neither vouched for it nor lied about it.
    assert!(!dest_a.join("clip.bin").exists());
    // The second copy verified normally and is on disk.
    assert_eq!(fs::read(dest_b.join("clip.bin")).unwrap(), bytes);
    assert!(
        support::partial_files(&dest_a).is_empty() && support::partial_files(&dest_b).is_empty(),
        "the vanished copy must not leave a .just_cache-partial-* file behind"
    );

    // The catalog-side proof that the unverified copy was never recorded as good: locate
    // answers from the catalog alone, and must list the copy that verified but not the
    // one that vanished. (The object still has a hot copy, so `catalog sync` skips the
    // floor check for it by design — the sweep report above is the under-replication
    // finding; this is the "no false record" half of the contract.)
    let found = locate(&catalog, "clip.bin");
    assert_exit(&found, 0);
    let locate_text = text_of(&found);
    assert!(
        locate_text.contains(&dest_b.canonicalize().unwrap().display().to_string()),
        "the verified copy must be in the catalog:\n{locate_text}"
    );
    assert!(
        !locate_text.contains(&dest_a.canonicalize().unwrap().display().to_string()),
        "a copy the mover could not verify must not be recorded as good:\n{locate_text}"
    );
}

/// Verification fails on a copy the sweep itself just made — one byte flipped between
/// the write and the read-back, with more copies still to come. The bad copy does not
/// count, is dropped rather than left to masquerade as data of record, the remaining
/// destinations are still tried, and the source is kept.
#[test]
fn a_checksum_mismatch_on_a_fresh_copy_mid_placement_keeps_going_and_keeps_the_source() {
    let (_tmp, watch, dest_a, dest_b, bytes) = setup("fi-mismatch");

    let output = fault_sweep(&watch, &[&dest_a, &dest_b], "corrupt-readback=1");
    assert_exit(&output, 1);
    let text = text_of(&output);
    assert!(
        text.contains("under-replicated") && text.contains(&dest_a.display().to_string()),
        "the mismatch must be reported against its destination:\n{text}"
    );

    // The corrupt copy of our own making was dropped: the source is still present, so
    // leaving it would let a later adopt mistake it for data of record.
    assert!(!dest_a.join("clip.bin").exists());
    // The sweep kept going: the second destination still received and verified a copy.
    assert_eq!(fs::read(dest_b.join("clip.bin")).unwrap(), bytes);
    // One verified of two is below the floor, so the source stays.
    assert_eq!(fs::read(watch.join("clip.bin")).unwrap(), bytes);
    assert!(
        support::partial_files(&dest_a).is_empty() && support::partial_files(&dest_b).is_empty()
    );

    // Recovery: with the fault gone, the sweep places the missing copy and retires.
    let healed = clean_sweep(&watch, &[&dest_a, &dest_b]);
    assert_exit(&healed, 0);
    assert!(fs::symlink_metadata(watch.join("clip.bin"))
        .unwrap()
        .is_symlink());
    assert_eq!(fs::read(dest_a.join("clip.bin")).unwrap(), bytes);
    assert_eq!(fs::read(dest_b.join("clip.bin")).unwrap(), bytes);
}

/// The hook is inert when unset: an identical sweep with no `JUST_CACHE_FAULT` behaves
/// exactly as before the hook existed — two verified copies, source retired, exit 0.
/// This is the "no behaviour change when the hook is unset" half of the acceptance
/// criteria, asserted against the fault-injection build itself.
#[test]
fn the_hook_is_inert_when_unset() {
    let (_tmp, watch, dest_a, dest_b, bytes) = setup("fi-inert");

    let output = clean_sweep(&watch, &[&dest_a, &dest_b]);
    assert_exit(&output, 0);
    assert!(fs::symlink_metadata(watch.join("clip.bin"))
        .unwrap()
        .is_symlink());
    assert_eq!(fs::read(dest_a.join("clip.bin")).unwrap(), bytes);
    assert_eq!(fs::read(dest_b.join("clip.bin")).unwrap(), bytes);
}

/// The copy is recorded only *after* the source is retired, and it is gone before that
/// ever happens: a healthy sweep retires the source, then the copy on the second disk
/// disappears, with no catalog in existence. Both reporters must name the disk the copy
/// was on — `replica-lost` from the walk-based audit (which is what runs with no
/// catalog), `under-replicated` from the first sync, which builds one and still says the
/// truth. Neither may invent a record for the disk that no longer holds the object.
///
/// Removing the *copy* rather than the root is deliberate: an unmounted root is a
/// different, equally-tested failure (invariant 1 — no command creates a destination
/// root), and the tail of this test asserts exactly that refusal rather than pretending
/// a missing root is a reportable replica loss.
#[test]
fn a_copy_vanishing_after_retirement_is_reported_by_audit_and_sync() {
    let (_tmp, watch, dest_a, dest_b, bytes) = setup("fi-after-retire");

    // No catalog exists for this watch yet, so the sweep retires the source on a healthy
    // replication and the loss lands strictly between retirement and any catalog record.
    let output = clean_sweep(&watch, &[&dest_a, &dest_b]);
    assert_exit(&output, 0);
    assert!(fs::symlink_metadata(watch.join("clip.bin"))
        .unwrap()
        .is_symlink());
    assert_eq!(fs::read(dest_a.join("clip.bin")).unwrap(), bytes);
    assert_eq!(fs::read(dest_b.join("clip.bin")).unwrap(), bytes);

    // The disk is still there; the object's copy on it is not.
    fs::remove_file(dest_b.join("clip.bin")).unwrap();

    // The walk-based audit (no catalog to answer from) reports the lost replica and
    // names the disk it was on.
    let audit = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&dest_a)
        .arg("--dest")
        .arg(&dest_b)
        .args(["--copies", "2"])
        .output()
        .unwrap();
    assert_exit(&audit, 1);
    let audit_text = stdout(&audit);
    assert!(
        audit_text.contains("replica-lost") && audit_text.contains("fi-after-retire-cold-b"),
        "the audit must name the disk the copy was on:\n{audit_text}"
    );

    // The first catalog sync to see the world records the truth: one tier holds the
    // object, the other does not, and the floor of 2 is unmet — the vanished copy is not
    // recorded as a good location anywhere.
    let sync = catalog_sync(&watch, &[&dest_a, &dest_b], 2);
    assert_exit(&sync, 1);
    let sync_text = stdout(&sync);
    assert!(
        sync_text.contains("under-replicated") || sync_text.contains("location-missing"),
        "the sync must report the copy that is gone:\n{sync_text}"
    );
    assert!(
        sync_text.contains("fi-after-retire-cold-b"),
        "the report must name the disk that lost the copy:\n{sync_text}"
    );

    // A later sweep cannot heal this one: the hot path is a symlink, so there is no
    // source left to re-replicate from, and the tool does not recreate copies from a
    // sibling (that is repair's job, still future work). The state is stable and
    // reported, not silently "fixed" — and the finding repeats on every sync.
    let later = clean_sweep(&watch, &[&dest_a, &dest_b]);
    let later_text = text_of(&later);
    assert!(
        fs::symlink_metadata(watch.join("clip.bin"))
            .unwrap()
            .is_symlink(),
        "the surviving copy must not be touched by a later sweep:\n{later_text}"
    );
    let resync = catalog_sync(&watch, &[&dest_a, &dest_b], 2);
    assert_exit(&resync, 1);
    assert!(
        text_of(&resync).contains("fi-after-retire-cold-b"),
        "the finding repeats on every sync until a human acts"
    );

    // The stronger form of "the destination vanishes": the root itself is gone. Every
    // command then refuses before touching anything — a destination root is never
    // recreated (invariant 1) — which is why a genuinely unmounted tier is handled by
    // restoring the mount, not by `audit` inventing a directory on the wrong filesystem.
    fs::remove_dir_all(&dest_b).unwrap();
    let unmounted = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&dest_a)
        .arg("--dest")
        .arg(&dest_b)
        .args(["--copies", "2"])
        .output()
        .unwrap();
    assert_exit(&unmounted, 2);
    assert!(
        stderr(&unmounted).contains("will not create destination roots"),
        "a missing root must be a refusal, never a silent directory:\n{}",
        stderr(&unmounted)
    );
    assert!(
        !dest_b.exists(),
        "the refused invocation must not have created the destination root"
    );
    assert_eq!(
        fs::read(dest_a.join("clip.bin")).unwrap(),
        bytes,
        "and it must not have touched the surviving copy"
    );
}
