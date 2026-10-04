//! Garbage collection for tier bytes no catalog row references (issue #144), end to end
//! through the real binary.
//!
//! The claims under test: a report-only pass names the bytes and removes nothing; a copy a
//! row claims is kept whether or not it was ever verified; a row the pass cannot read as
//! pointing inside a configured root is a finding, never a licence; an `--apply` pass is
//! journalled so a crash after an unlink leaves a recoverable record, not an adopted row;
//! and the exit codes and `--json` shape hold.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::{Catalog, CATALOG_NAME};
use just_cache::digest::bytes_digest;
use just_cache::gc::GC_JOURNAL_NAME;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the process should exit")
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// A watched hot tree and one cold tier, both empty, with the catalog already synced (so
/// the roots are recorded) and no rows. Each test then puts down the bytes it means to
/// judge, *after* the sync, so they are unreferenced by construction.
struct Fixture {
    _dir: tempfile::TempDir,
    hot: PathBuf,
    cold: PathBuf,
    catalog: PathBuf,
}

impl Fixture {
    fn build() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let hot = dir.path().join("hot");
        let cold = dir.path().join("cold");
        fs::create_dir_all(&hot).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let fixture = Fixture {
            catalog: hot.join(CATALOG_NAME),
            hot,
            cold,
            _dir: dir,
        };
        let sync = fixture.sync();
        assert_eq!(code(&sync), 0, "sync failed: {}", stderr(&sync));
        fixture
    }

    fn sync(&self) -> Output {
        bin()
            .args(["catalog", "sync", "--watch"])
            .arg(&self.hot)
            .arg("--dest")
            .arg(&self.cold)
            .output()
            .unwrap()
    }

    fn gc(&self, extra: &[&str]) -> Output {
        let mut command = bin();
        command.args(["gc", "--catalog"]).arg(&self.catalog);
        command.args(extra);
        command.output().unwrap()
    }

    fn open(&self) -> Catalog {
        Catalog::open(&self.catalog).unwrap()
    }
}

#[test]
fn a_report_only_pass_names_the_bytes_and_removes_nothing() {
    let fixture = Fixture::build();
    fs::write(fixture.cold.join("orphan.bin"), b"nobody references this").unwrap();
    let mounted = fixture.gc(&[]);
    assert_eq!(
        code(&mounted),
        1,
        "a finding exits 1:\n{}",
        stderr(&mounted)
    );
    assert!(
        stdout(&mounted).contains("orphan.bin"),
        "the garbage is named: {}",
        stdout(&mounted)
    );
    assert!(
        exists(&fixture.cold.join("orphan.bin")),
        "a report-only pass must remove nothing"
    );
    assert_eq!(fixture.open().location_count().unwrap(), 0);
    assert_eq!(fixture.open().object_count().unwrap(), 0);

    // `--json` carries the same answer.
    let json = fixture.gc(&["--json"]);
    assert_eq!(code(&json), 1);
    let body = stdout(&json);
    assert!(body.contains("\"applied\":false"), "{body}");
    assert!(body.contains("\"bytes\":"), "{body}");
    assert!(body.contains("orphan.bin"), "{body}");
}

/// A copy a row still claims is kept colour-blind to a scrub verdict: an unverified row is
/// still a claim. Both files here have a location row and no name.
#[test]
fn a_claimed_copy_is_kept_even_when_unverified() {
    let fixture = Fixture::build();
    fs::write(fixture.cold.join("claimed.bin"), b"claimed bytes").unwrap();
    fs::write(fixture.cold.join("unverified.bin"), b"unknown bytes").unwrap();
    let cold_tier = fixture
        .cold
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    {
        let catalog = fixture.open();
        let claimed = bytes_digest(b"claimed bytes");
        let unverified = bytes_digest(b"unknown bytes");
        let now = 0;
        // A claimed-but-unverified row: identity only, no scrub has confirmed it.
        catalog
            .ensure_object(claimed.as_bytes(), 13, claimed.as_bytes(), "offloaded", now)
            .unwrap();
        catalog
            .record_replica(claimed.as_bytes(), &cold_tier, "claimed.bin", false, None)
            .unwrap();
        catalog
            .ensure_object(
                unverified.as_bytes(),
                13,
                unverified.as_bytes(),
                "offloaded",
                now,
            )
            .unwrap();
        catalog
            .record_replica(
                unverified.as_bytes(),
                &cold_tier,
                "unverified.bin",
                false,
                None,
            )
            .unwrap();
    }

    let output = fixture.gc(&[]);
    assert_eq!(
        code(&output),
        0,
        "no garbage: every byte is claimed\n{}",
        stderr(&output)
    );
    assert!(exists(&fixture.cold.join("claimed.bin")));
    assert!(exists(&fixture.cold.join("unverified.bin")));
}

/// A row whose tier is not a recorded root and names no volume is reported by name and
/// never acted on — the walk deletes nothing on the strength of a row it could not read.
#[test]
fn a_row_outside_every_root_is_a_finding_not_a_licence() {
    let fixture = Fixture::build();
    fs::write(fixture.cold.join("orphan.bin"), b"real garbage").unwrap();
    {
        let catalog = fixture.open();
        let id = bytes_digest(b"somewhere else");
        catalog
            .ensure_object(id.as_bytes(), 13, id.as_bytes(), "offloaded", 0)
            .unwrap();
        catalog
            .record_replica(id.as_bytes(), "/nowhere/else", "x.bin", false, None)
            .unwrap();
    }

    let output = fixture.gc(&[]);
    assert_eq!(code(&output), 1);
    let printed = stdout(&output);
    assert!(
        printed.contains("unresolved row /nowhere/else/x.bin"),
        "the unreadable row is named: {printed}"
    );
    // The licensed-looking path is not touched, and the real garbage is still only reported.
    assert!(exists(&fixture.cold.join("orphan.bin")));
}

/// The crash window the GC journal exists for: the first unlink landed, the process died
/// before the pass finished. The next pass drops the stale record, adopts nothing, and
/// removes nothing that remains.
#[test]
fn an_interrupted_pass_adopts_nothing_and_recovers() {
    let fixture = Fixture::build();
    fs::write(fixture.cold.join("only.bin"), b"the only garbage").unwrap();

    let crashed = bin()
        .args(["gc", "--catalog"])
        .arg(&fixture.catalog)
        .arg("--apply")
        .env("JUST_CACHE_FAULT", "gc-after-delete=1")
        .output()
        .unwrap();
    assert_ne!(
        crashed.status.code(),
        Some(0),
        "the fault did not fire: {}",
        stderr(&crashed)
    );
    assert!(
        !exists(&fixture.cold.join("only.bin")),
        "the first unlink landed"
    );
    assert!(
        exists(&fixture.hot.join(GC_JOURNAL_NAME)),
        "the intent was journalled before the unlink"
    );
    // No adopted row: GC never writes the catalog.
    assert_eq!(fixture.open().location_count().unwrap(), 0);
    assert_eq!(fixture.open().object_count().unwrap(), 0);

    let recovered = fixture.gc(&["--apply"]);
    assert_eq!(code(&recovered), 0, "{}", stderr(&recovered));
    assert!(
        stdout(&recovered).contains("dropped a stale journal record"),
        "the stale record is named: {}",
        stdout(&recovered)
    );
    assert!(
        stdout(&recovered).contains("no garbage"),
        "nothing remains to remove: {}",
        stdout(&recovered)
    );
}

/// `--apply` removes exactly what the report named, and a second pass finds nothing.
#[test]
fn apply_removes_the_bytes_and_then_there_is_nothing_left() {
    let fixture = Fixture::build();
    fs::write(fixture.cold.join("gone.bin"), b"junk bytes").unwrap();
    fs::create_dir_all(fixture.cold.join("emptied/leaf")).unwrap();

    let applied = fixture.gc(&["--apply"]);
    assert_eq!(code(&applied), 0, "{}", stderr(&applied));
    assert!(!exists(&fixture.cold.join("gone.bin")));
    assert!(
        !exists(&fixture.cold.join("emptied")),
        "emptied dirs go too"
    );
    let json = fixture.gc(&["--json"]);
    assert_eq!(code(&json), 0, "{}", stderr(&json));
    assert!(stdout(&json).contains("\"bytes\":0"), "{}", stdout(&json));
}

#[test]
fn a_bad_invocation_is_a_usage_error() {
    let fixture = Fixture::build();

    let absent = bin()
        .args(["gc", "--catalog"])
        .arg(fixture.hot.join("no-such.sqlite"))
        .output()
        .unwrap();
    assert_eq!(code(&absent), 2, "{}", stderr(&absent));

    let unknown = fixture.gc(&["--tier", "not-a-tier"]);
    assert_eq!(code(&unknown), 2, "{}", stderr(&unknown));
    assert!(stderr(&unknown).contains("not-a-tier"));
}
