//! Recovery from an interrupted move, and the CLI's behaviour around a damaged journal.
//!
//! The states here are the ones a crash actually leaves behind, built by hand rather than
//! by killing a process mid-sweep: a test that races a real crash passes only when the
//! timing lands the right way, and a crash test that usually passes is worse than none.
//! What matters is that recovery reads these states correctly, and every state below is
//! one the mover can genuinely produce.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use just_cache::journal::{self, Journal, JOURNAL_NAME};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

/// A tree with one file, a cold tier, and a journal — the layout a sweep would have had.
fn scenario() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    (tmp, watch, cold)
}

fn journal_of(watch: &Path) -> Journal {
    Journal::in_tree(watch).expect("journal opens")
}

/// The crash window this module exists for: the bytes are on the cold tier and complete,
/// the source is gone, and no symlink was ever created. The file is unreachable at its own
/// path — until the journal says what was happening.
#[test]
fn a_name_lost_between_the_move_and_the_link_is_restored() {
    let (tmp, watch, cold) = scenario();
    let hot = watch.join("shows/episode.mkv");
    let cold_copy = cold.join("shows/episode.mkv");
    let payload = b"the only copy of this, right now".to_vec();
    fs::write(&cold_copy, &payload).unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(
            Path::new("shows/episode.mkv"),
            &cold_copy,
            payload.len() as u64,
        )
        .unwrap();
    journal.compact().unwrap();
    assert!(!hot.exists(), "the crash window means the name is gone");

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert_eq!(report.restored(), 1);
    assert_eq!(report.trouble(), 0);

    let link = fs::symlink_metadata(&hot).unwrap();
    assert!(link.is_symlink(), "the name must come back as a symlink");
    assert_eq!(
        fs::read(&hot).unwrap(),
        payload,
        "and it must read the cold bytes"
    );

    // The record has served its purpose and must not survive to be replayed again.
    let reopened = journal_of(&watch);
    assert!(reopened.is_empty(), "a repaired record is forgotten");
    let _ = tmp;
}

/// The cold copy at the final name must match what the move promised. Otherwise the
/// "restore the name" path would point a name at bytes nobody promised — a file that
/// merely shares the path of an interrupted copy.
#[test]
fn a_copy_of_the_wrong_size_is_never_linked_to() {
    let (tmp, watch, cold) = scenario();
    let cold_copy = cold.join("shows/episode.mkv");
    fs::write(&cold_copy, b"short").unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(Path::new("shows/episode.mkv"), &cold_copy, 4096)
        .unwrap();

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert_eq!(report.restored(), 0);
    assert_eq!(report.trouble(), 1, "a refusal is something to look at");
    assert!(
        !watch.join("shows/episode.mkv").exists(),
        "no name may be created pointing at a copy we cannot vouch for"
    );
    assert!(fs::read(&cold_copy).is_ok(), "and nothing may be deleted");

    // Kept, so the next run still reports it rather than quietly forgetting.
    assert_eq!(journal_of(&watch).unfinished_count(), 1);
    let _ = tmp;
}

/// Nothing moved: the source is intact and no copy exists. Nothing to do, and nothing to
/// keep either.
#[test]
fn a_move_that_never_started_is_forgotten() {
    let (tmp, watch, cold) = scenario();
    let hot = watch.join("shows/episode.mkv");
    fs::write(&hot, b"still here").unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(
            Path::new("shows/episode.mkv"),
            &cold.join("shows/episode.mkv"),
            10,
        )
        .unwrap();

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert!(matches!(
        report.outcomes[0].1,
        journal::Recovery::NeverStarted
    ));
    assert!(fs::read(&hot).is_ok());
    assert!(journal_of(&watch).is_empty());
    let _ = tmp;
}

/// The move finished and the record simply never got updated.
#[test]
fn a_move_that_actually_finished_is_not_repaired_twice() {
    let (tmp, watch, cold) = scenario();
    let hot = watch.join("shows/episode.mkv");
    let cold_copy = cold.join("shows/episode.mkv");
    fs::write(&cold_copy, b"moved already").unwrap();
    std::os::unix::fs::symlink(std::path::Path::new("../cold/shows/episode.mkv"), &hot).unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(Path::new("shows/episode.mkv"), &cold_copy, 13)
        .unwrap();

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert_eq!(report.outcomes[0].1, journal::Recovery::AlreadyLinked);
    assert!(fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert!(journal_of(&watch).is_empty());
    let _ = tmp;
}

/// Both copies are present: the source was never removed. Recovery does not guess — the
/// next sweep's adoption path hashes a same-size destination and decides. Proven here by
/// letting a real sweep do exactly that.
#[test]
fn both_copies_present_is_left_to_the_sweep_which_adopts_it() {
    let (tmp, watch, cold) = scenario();
    let payload = b"identical on both sides";
    let hot = watch.join("shows/episode.mkv");
    let cold_copy = cold.join("shows/episode.mkv");
    fs::write(&hot, payload).unwrap();
    fs::write(&cold_copy, payload).unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(
            Path::new("shows/episode.mkv"),
            &cold_copy,
            payload.len() as u64,
        )
        .unwrap();

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert!(matches!(
        report.outcomes[0].1,
        journal::Recovery::LeftForTheSweep { .. }
    ));
    assert!(journal_of(&watch).is_empty());

    // The sweep then does what it always does with a byte-identical destination.
    let status = bin()
        .args([
            "--watch",
            watch.to_str().unwrap(),
            "--dest",
            cold.to_str().unwrap(),
            "--min-idle-days",
            "0",
            "--min-free-gb",
            "0",
            "--once",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        fs::symlink_metadata(&hot).unwrap().is_symlink(),
        "the sweep adopts the identical cold copy and leaves a link"
    );
    assert_eq!(fs::read(&hot).unwrap(), payload);
    let _ = tmp;
}

/// An interrupted copy is ours to delete when the source is still whole…
#[test]
fn an_interrupted_copy_is_removed_when_the_source_is_intact() {
    let (tmp, watch, cold) = scenario();
    let hot = watch.join("shows/episode.mkv");
    fs::write(&hot, b"the source is fine").unwrap();
    let partial = cold.join("shows").join(format!(
        "{}999-1.tmp",
        just_cache::disk_management::PARTIAL_PREFIX
    ));
    fs::write(&partial, b"half written").unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(
            Path::new("shows/episode.mkv"),
            &cold.join("shows/episode.mkv"),
            17,
        )
        .unwrap();

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert!(matches!(
        report.outcomes[0].1,
        journal::Recovery::PartialRemoved { .. }
    ));
    assert!(!partial.exists(), "the partial is ours and worthless");
    assert!(fs::read(&hot).is_ok(), "the source is untouched");
    assert!(journal_of(&watch).is_empty());
    let _ = tmp;
}

/// …and must NOT be deleted when it may be the only surviving bytes.
#[test]
fn an_interrupted_copy_is_kept_when_nothing_else_survived() {
    let (tmp, watch, cold) = scenario();
    let partial = cold.join("shows").join(format!(
        "{}999-2.tmp",
        just_cache::disk_management::PARTIAL_PREFIX
    ));
    fs::write(&partial, b"possibly the last bytes of this file").unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(
            Path::new("shows/episode.mkv"),
            &cold.join("shows/episode.mkv"),
            4096,
        )
        .unwrap();

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert!(matches!(
        report.outcomes[0].1,
        journal::Recovery::PartialKept { .. }
    ));
    assert!(partial.exists(), "deleting it could be destroying the file");
    assert!(
        !watch.join("shows/episode.mkv").exists(),
        "and no name may be invented for an incomplete copy"
    );
    assert_eq!(report.trouble(), 1);
    let _ = tmp;
}

/// The one state that means real loss: no source, no copy, nothing to recover from. It is
/// kept in the journal, because nothing on disk remains for `audit` to find later.
#[test]
fn data_loss_is_reported_and_remembered() {
    let (tmp, watch, cold) = scenario();
    let mut journal = journal_of(&watch);
    journal
        .intent(
            Path::new("shows/episode.mkv"),
            &cold.join("shows/episode.mkv"),
            4096,
        )
        .unwrap();

    let report = journal::repair(&mut journal, &watch).unwrap();
    assert!(matches!(
        report.outcomes[0].1,
        journal::Recovery::DataLost { .. }
    ));
    let line = report.outcomes[0].1.describe(&report.outcomes[0].0);
    assert!(
        line.contains("DATA LOST"),
        "the wording must not soften it: {line}"
    );
    assert_eq!(
        journal_of(&watch).unfinished_count(),
        1,
        "kept for the next run"
    );
    let _ = tmp;
}

/// A sweep leaves no unfinished records behind, so a busy tree does not accumulate journal
/// noise that would make a real record hard to spot.
#[test]
fn a_completed_sweep_leaves_nothing_unfinished() {
    let (tmp, watch, cold) = scenario();
    let hot = watch.join("shows/episode.mkv");
    fs::write(&hot, b"move me").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let status = bin()
        .args([
            "--watch",
            watch.to_str().unwrap(),
            "--dest",
            cold.to_str().unwrap(),
            "--min-idle-days",
            "0",
            "--min-free-gb",
            "0",
            "--once",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(fs::symlink_metadata(&hot).unwrap().is_symlink());
    assert_eq!(journal_of(&watch).unfinished_count(), 0);
    let _ = tmp;
}

/// A damaged journal stops the sweep rather than being ignored: it may describe a move
/// whose bytes are already on a cold tier, and carrying on would both lose that knowledge
/// and run new moves through an unguarded window. The message names the line and the fix.
#[test]
fn a_damaged_journal_refuses_to_sweep_and_says_which_line() {
    let (tmp, watch, cold) = scenario();
    fs::write(watch.join("shows/episode.mkv"), b"payload").unwrap();
    fs::write(
        watch.join(JOURNAL_NAME),
        "{\"stage\":\"intent\",\"rel\":\"shows/episode.mkv\",\"dest\":\"x\",\"size\":1}\n{{{ broken\n",
    )
    .unwrap();

    let output = bin()
        .args([
            "--watch",
            watch.to_str().unwrap(),
            "--dest",
            cold.to_str().unwrap(),
            "--min-idle-days",
            "0",
            "--once",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success(), "a damaged journal must not sweep");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("line 2"), "name the line: {stderr}");
    assert!(
        !fs::symlink_metadata(watch.join("shows/episode.mkv"))
            .unwrap()
            .is_symlink(),
        "and nothing may have moved"
    );
    let _ = tmp;
}

/// Recovery is what makes the window survivable, so it is worth proving end to end through
/// the CLI: the file is gone at its path, the CLI restores it, and the audit command then
/// finds a healthy tree rather than an orphaned copy.
#[test]
fn the_cli_restores_a_lost_name_on_its_next_run() {
    let (tmp, watch, cold) = scenario();
    let payload = b"recover me through the real command line";
    let cold_copy = cold.join("shows/episode.mkv");
    fs::write(&cold_copy, payload).unwrap();

    let mut journal = journal_of(&watch);
    journal
        .intent(
            Path::new("shows/episode.mkv"),
            &cold_copy,
            payload.len() as u64,
        )
        .unwrap();
    journal.compact().unwrap();

    let output = bin()
        .args([
            "--watch",
            watch.to_str().unwrap(),
            "--dest",
            cold.to_str().unwrap(),
            "--min-idle-days",
            "30",
            "--min-free-gb",
            "0",
            "--once",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("restored the name"),
        "recovery must be visible, not silent: {stdout}"
    );
    assert_eq!(fs::read(watch.join("shows/episode.mkv")).unwrap(), payload);

    // And the tree is consistent afterwards, not merely patched.
    let audit = bin()
        .args([
            "audit",
            "--watch",
            watch.to_str().unwrap(),
            "--dest",
            cold.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&audit.stdout);
    assert!(audit.status.success(), "audit should be clean: {report}");
    assert!(report.contains("healthy: 1"), "{report}");
    let _ = tmp;
}

/// A symlink planted at the journal name must be refused, not followed: following it would
/// append every intent record into — or, on the append open, redirect writes into — a file
/// the planter chose. The symlink target keeps its contents and the run says why.
#[cfg(unix)]
#[test]
fn a_symlink_at_the_journal_name_is_refused_and_the_target_survives() {
    let (tmp, watch, cold) = scenario();
    let victim = tmp.path().join("victim.txt");
    fs::write(&victim, b"SECRET-DATA-THAT-MUST-SURVIVE\n").unwrap();
    fs::write(watch.join("shows/episode.mkv"), b"payload").unwrap();
    std::os::unix::fs::symlink(&victim, watch.join(JOURNAL_NAME)).unwrap();

    let output = bin()
        .args([
            "--watch",
            watch.to_str().unwrap(),
            "--dest",
            cold.to_str().unwrap(),
            "--min-idle-days",
            "30",
            "--min-free-gb",
            "0",
            "--once",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a symlinked journal must stop the run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("symlink"),
        "the refusal must say what it refused: {stderr}"
    );
    assert_eq!(
        fs::read(&victim).unwrap(),
        b"SECRET-DATA-THAT-MUST-SURVIVE\n",
        "the link target must be untouched"
    );
    assert!(
        fs::symlink_metadata(watch.join(JOURNAL_NAME))
            .unwrap()
            .is_symlink(),
        "the link itself must be left as it was"
    );
    assert!(
        !fs::symlink_metadata(watch.join("shows/episode.mkv"))
            .unwrap()
            .is_symlink(),
        "and nothing may have moved"
    );
    let _ = tmp;
}

/// The compaction temp is the other fixed name. `create_new` refuses the planted link rather
/// than truncating through it, so the file it points at keeps its bytes, and the end-of-sweep
/// compaction that hits it is reported rather than swallowed.
#[cfg(unix)]
#[test]
fn a_symlink_at_the_compaction_temp_is_refused_and_the_target_survives() {
    let (tmp, watch, cold) = scenario();
    let victim = tmp.path().join("victim.txt");
    fs::write(&victim, b"SECRET-DATA-THAT-MUST-SURVIVE\n").unwrap();
    // A warm file so the sweep itself has nothing to do; the only write is the compaction.
    fs::write(watch.join("shows/episode.mkv"), b"payload").unwrap();
    let temporary = watch.join(JOURNAL_NAME).with_extension("compacting");
    std::os::unix::fs::symlink(&victim, &temporary).unwrap();

    let output = bin()
        .args([
            "--watch",
            watch.to_str().unwrap(),
            "--dest",
            cold.to_str().unwrap(),
            "--min-idle-days",
            "30",
            "--min-free-gb",
            "0",
            "--once",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "the refused compaction must fail the run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("symlink"),
        "the refusal must say what it refused: {stderr}"
    );
    assert_eq!(
        fs::read(&victim).unwrap(),
        b"SECRET-DATA-THAT-MUST-SURVIVE\n",
        "the link target must never be truncated into"
    );
    assert!(
        fs::symlink_metadata(&temporary).unwrap().is_symlink(),
        "the planted link is left for a human, not deleted"
    );
    let _ = tmp;
}

/// The journal records every in-flight move, destination included, so it must not be
/// world-readable. Created under `umask 0` — where an un-hinted create would be 0666 — it
/// still comes out 0600.
#[cfg(unix)]
#[test]
fn a_newly_created_journal_is_mode_0600_even_under_a_permissive_umask() {
    use std::os::unix::fs::PermissionsExt;

    let (tmp, watch, cold) = scenario();
    fs::write(watch.join("shows/episode.mkv"), b"still warm").unwrap();

    // The umask is per-process, so it is set in a child: `sh -c 'umask 0; exec …'`.
    let script = format!(
        "umask 0; exec '{}' --watch '{}' --dest '{}' --min-idle-days 30 --min-free-gb 0 --once",
        env!("CARGO_BIN_EXE_just_cache"),
        watch.display(),
        cold.display()
    );
    let status = Command::new("sh").args(["-c", &script]).status().unwrap();
    assert!(status.success());

    let mode = fs::metadata(watch.join(JOURNAL_NAME))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the journal must be created with an explicit mode, not the umask"
    );
    let _ = tmp;
}
