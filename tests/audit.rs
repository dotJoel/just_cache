//! End-to-end behaviour of `just_cache audit` against real temporary trees, plus CLI
//! checks that pin down the exit-code contract cron depends on.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use just_cache::audit::{self, RepairAction, VerdictKind};
use just_cache::disk_management;

fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

/// Build the four-way split the classifier cares about in one tree and assert on it.
#[test]
fn audit_separates_healthy_from_every_problem_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(watch.join("gone")).unwrap();
    fs::create_dir_all(&cold).unwrap();

    // healthy: a plain file nobody has moved
    fs::write(watch.join("live.bin"), b"still hot").unwrap();
    // healthy: a migrated file, symlink and cold copy present
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"migrated").unwrap();
    link(
        Path::new("../../cold/shows/moved.mkv"),
        &watch.join("shows/moved.mkv"),
    );

    // duplicate: the source removal never happened
    fs::write(watch.join("dup.bin"), b"copied twice").unwrap();
    fs::write(cold.join("dup.bin"), b"copied twice").unwrap();

    // orphaned-copy: cold bytes with no name at the source
    fs::create_dir_all(cold.join("gone")).unwrap();
    fs::write(cold.join("gone/orphan.bin"), b"only cold").unwrap();

    // dangling-symlink: the target does not exist
    link(
        Path::new("../../cold/vanished/target.bin"),
        &watch.join("gone/vanished.bin"),
    );

    // unexpected-target: resolves, but outside every cold tier
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("real.bin"), b"not ours").unwrap();
    link(
        &elsewhere.join("real.bin"),
        &watch.join("gone/elsewhere.bin"),
    );

    let report = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();

    assert_eq!(report.count(VerdictKind::Duplicate), 1);
    assert_eq!(report.count(VerdictKind::OrphanedCopy), 1);
    assert_eq!(report.count(VerdictKind::DanglingSymlink), 1);
    assert_eq!(report.count(VerdictKind::UnexpectedTarget), 1);
    assert_eq!(
        report.healthy, 2,
        "live.bin and shows/moved.mkv are healthy"
    );
    assert_eq!(report.scanned, 6);
    assert!(report.has_findings());

    let duplicate = report
        .findings
        .iter()
        .find(|finding| finding.verdict.kind() == VerdictKind::Duplicate)
        .unwrap();
    assert_eq!(duplicate.path, watch.join("dup.bin"));
    assert_eq!(
        duplicate.cold_copy.as_deref(),
        Some(cold.join("dup.bin").as_path())
    );
}

#[test]
fn a_tree_the_mover_just_left_is_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("a/b")).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("a/b/clip.mov"), b"a real payload").unwrap();

    let entries = disk_management::list_files_recursive(&watch).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.relative == Path::new("a/b/clip.mov"))
        .unwrap();
    disk_management::move_file_with_symlink(&cold, entry).unwrap();

    let report = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    assert!(!report.has_findings(), "{:?}", report.findings);
    assert_eq!(report.healthy, 1);
}

#[test]
fn several_cold_tiers_are_all_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let tier_a = tmp.path().join("cold-a");
    let tier_b = tmp.path().join("cold-b");
    for dir in [&watch, &tier_a, &tier_b] {
        fs::create_dir_all(dir).unwrap();
    }
    // The copy is on the *second* tier, which the symlink points into: healthy.
    fs::write(tier_b.join("file.bin"), b"payload").unwrap();
    link(Path::new("../cold-b/file.bin"), &watch.join("file.bin"));

    let report = audit::audit(&watch, &[tier_a.clone(), tier_b.clone()]).unwrap();
    assert!(
        !report.has_findings(),
        "a symlink into any configured tier is expected: {:?}",
        report.findings
    );
    assert_eq!(report.dests.len(), 2);
}

#[test]
fn repair_completes_an_interrupted_move_and_audit_then_comes_back_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("dup.bin"), b"identical on both sides").unwrap();
    fs::write(cold.join("dup.bin"), b"identical on both sides").unwrap();

    let report = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    assert_eq!(report.count(VerdictKind::Duplicate), 1);

    let repairs = audit::repair(&report).unwrap();
    assert_eq!(repairs.len(), 1);
    assert!(repairs[0].repaired());
    assert!(matches!(
        repairs[0].action,
        RepairAction::RemovedDuplicate { .. }
    ));

    let metadata = fs::symlink_metadata(watch.join("dup.bin")).unwrap();
    assert!(metadata.is_symlink(), "the source must become a symlink");
    assert_eq!(
        fs::read_to_string(watch.join("dup.bin")).unwrap(),
        "identical on both sides"
    );
    assert_eq!(
        fs::read(cold.join("dup.bin")).unwrap(),
        b"identical on both sides"
    );

    let after = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    assert!(
        !after.has_findings(),
        "repair must converge: {:?}",
        after.findings
    );
}

#[test]
fn repair_refuses_to_delete_when_the_copies_differ() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("clash.bin"), b"source bytes").unwrap();
    fs::write(cold.join("clash.bin"), b"totally different bytes").unwrap();

    let report = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    let repairs = audit::repair(&report).unwrap();

    assert_eq!(repairs.len(), 1);
    assert!(!repairs[0].repaired());
    assert!(matches!(repairs[0].action, RepairAction::Refused { .. }));
    assert!(
        !fs::symlink_metadata(watch.join("clash.bin"))
            .unwrap()
            .is_symlink(),
        "a size-only match is not enough; without a checksum match the source stays a file"
    );
    assert_eq!(fs::read(watch.join("clash.bin")).unwrap(), b"source bytes");
    assert_eq!(
        fs::read(cold.join("clash.bin")).unwrap(),
        b"totally different bytes"
    );
}

#[test]
fn repair_repoints_a_dangling_symlink_at_the_cold_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(cold.join("target.bin"), b"still here").unwrap();
    // The link text does not match where the cold copy actually is.
    link(Path::new("wrong-place.bin"), &watch.join("target.bin"));

    let report = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    assert_eq!(report.count(VerdictKind::DanglingSymlink), 1);

    let repairs = audit::repair(&report).unwrap();
    assert!(repairs[0].repaired(), "{:?}", repairs[0].action);
    assert_eq!(
        fs::read_to_string(watch.join("target.bin")).unwrap(),
        "still here"
    );

    let after = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    assert!(!after.has_findings(), "{:?}", after.findings);
}

#[test]
fn repair_does_not_touch_orphaned_copies_or_unexpected_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let elsewhere = tmp.path().join("elsewhere");
    for dir in [&watch, &cold, &elsewhere] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(cold.join("orphan.bin"), b"only cold").unwrap();
    fs::write(elsewhere.join("real.bin"), b"not ours").unwrap();
    link(&elsewhere.join("real.bin"), &watch.join("real.bin"));

    let report = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    assert_eq!(report.count(VerdictKind::OrphanedCopy), 1);
    assert_eq!(report.count(VerdictKind::UnexpectedTarget), 1);

    let repairs = audit::repair(&report).unwrap();
    assert!(repairs.iter().all(|repair| !repair.repaired()));
    assert!(
        fs::read(cold.join("orphan.bin")).is_ok(),
        "cold bytes must survive"
    );
    assert_eq!(
        fs::read_link(watch.join("real.bin")).unwrap(),
        elsewhere.join("real.bin")
    );
}

#[test]
fn the_cli_exits_nonzero_on_findings_and_zero_when_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();

    // clean tree
    fs::write(watch.join("live.bin"), b"hot").unwrap();
    let status = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .status()
        .unwrap();
    assert!(status.success(), "a clean tree must exit zero");

    // introduce a duplicate and a dangling link: non-zero, no parsing needed
    fs::write(watch.join("dup.bin"), b"same").unwrap();
    fs::write(cold.join("dup.bin"), b"same").unwrap();
    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("duplicate: 1"), "{text}");
    assert!(text.contains("dup.bin"), "{text}");

    // --json is parseable-ish and carries the same facts
    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let json = String::from_utf8_lossy(&output.stdout);
    assert!(json.contains("\"kind\":\"duplicate\""), "{json}");
    assert!(json.contains("\"duplicate\":1"), "{json}");
    assert!(json.contains("\"healthy\":1"), "{json}");

    // --repair resolves it, and a repaired tree exits zero
    let status = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--repair")
        .status()
        .unwrap();
    assert!(status.success(), "a fully repaired run must exit zero");
    assert!(fs::symlink_metadata(watch.join("dup.bin"))
        .unwrap()
        .is_symlink());
}

#[test]
fn repair_still_alerts_when_it_cannot_fix_a_finding() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(cold.join("orphan.bin"), b"only cold").unwrap();

    let status = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--repair")
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(1),
        "an orphaned copy is reported, never silently deleted, and keeps the alert up"
    );
    assert!(fs::read(cold.join("orphan.bin")).is_ok());
}

#[test]
fn the_original_flat_command_line_still_runs_a_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("cold.bin"), b"payload").unwrap();

    // No subcommand: exactly the v0.3.0 invocation.
    let legacy = bin()
        .arg("--watch")
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .args(["--once", "--dry-run", "--quiet"])
        .status()
        .unwrap();
    assert!(legacy.success(), "legacy flat flags must keep working");

    // And the new, explicit form does the same thing.
    let explicit = bin()
        .args(["sweep", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .args(["--once", "--dry-run", "--quiet"])
        .status()
        .unwrap();
    assert!(explicit.success());
}

#[test]
fn audit_rejects_a_destination_that_does_not_exist() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    fs::create_dir_all(&watch).unwrap();

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(tmp.path().join("not-mounted"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not an existing directory"));
}

#[test]
fn a_walked_tree_that_is_entirely_absent_on_the_cold_side_is_healthy() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold: PathBuf = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("a.txt"), b"a").unwrap();
    fs::write(watch.join("b.txt"), b"b").unwrap();

    let report = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();
    assert!(!report.has_findings());
    assert_eq!(report.healthy, 2);
    assert_eq!(report.scanned, 2);
}
