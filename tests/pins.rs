//! End-to-end behaviour of `lifecycle.pinned_until` and the `pin`/`unpin` commands (issue
//! #45) through the real binary.
//!
//! §5 gives a pin the power to win over policy, and the design's rule is that an explanation
//! names what decided — a pin that silently stopped a move would be indistinguishable from a
//! sweep that found nothing to do. These tests therefore drive the binary and read what a
//! shell sees: a pinned file stays put while its rule would otherwise fire, the expiry is
//! *printed* rather than implied, `explain` names the pin, and `unpin` releases it.
//!
//! The pin blocks through the mover's policy, so the central test is written to fail if the
//! catalog-pin check in `file_movement` is removed: the sweep would move the file and there
//! would be no "pinned in the catalog" line. (Verified by deleting the check and watching the
//! test fail, not by inspection.)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

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

fn set_times(path: &Path, when: SystemTime) {
    let times = fs::FileTimes::new().set_accessed(when).set_modified(when);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(times)
        .unwrap();
}

fn days_ago(days: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(days * 86_400)
}

/// A watch tree on the `ssd` tier and a cold root on `hdd_parked`, with a `policy.toml`
/// that would move anything idle past 30 days — so the only thing that keeps a file put is
/// the pin under test.
struct Fixture {
    _tmp: tempfile::TempDir,
    watch: PathBuf,
    cold: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(watch.join("shows")).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(
            watch.join("tiers.toml"),
            format!(
                "[tiers.ssd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n\n\
                 [tiers.hdd_parked]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"s\"\ncopies = 1\n",
                watch.display(),
                cold.display()
            ),
        )
        .unwrap();
        fs::write(
            watch.join("policy.toml"),
            "[[rule]]\nname = \"intelligent-tiering\"\nmatch = \"**\"\n\
             down = { after_idle = \"30d\", from = \"ssd\", to = \"hdd_parked\" }\n",
        )
        .unwrap();
        Self {
            _tmp: tmp,
            watch,
            cold,
        }
    }

    fn catalog_path(&self) -> PathBuf {
        Catalog::default_path(&self.watch)
    }

    /// A file under the watch root, stamped `days` idle. The content is unique per path so
    /// two files are two objects: a pin is keyed on the object's identity, so identical
    /// files would share one.
    fn place(&self, relative: &str, days: u64) -> PathBuf {
        let path = self.watch.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("payload for {relative}")).unwrap();
        set_times(&path, days_ago(days));
        path
    }

    fn sync(&self) {
        let output = bin()
            .args(["catalog", "sync", "--watch"])
            .arg(&self.watch)
            .arg("--dest")
            .arg(&self.cold)
            .output()
            .expect("catalog sync runs");
        assert_eq!(
            output.status.code(),
            Some(0),
            "catalog sync must succeed; stderr: {}",
            stderr(&output)
        );
    }

    fn pin(&self, path: &str, until: &str) -> Output {
        bin()
            .arg("pin")
            .arg(path)
            .arg("--until")
            .arg(until)
            .arg("--catalog")
            .arg(self.catalog_path())
            .output()
            .expect("pin runs")
    }

    fn unpin(&self, path: &str) -> Output {
        bin()
            .arg("unpin")
            .arg(path)
            .arg("--catalog")
            .arg(self.catalog_path())
            .output()
            .expect("unpin runs")
    }

    fn list_pins(&self) -> Output {
        bin()
            .arg("pin")
            .arg("--list")
            .arg("--catalog")
            .arg(self.catalog_path())
            .output()
            .expect("pin --list runs")
    }

    /// One policy sweep with the default config lookup, verbose so every skip line is shown.
    fn sweep(&self) -> Output {
        bin()
            .arg("--watch")
            .arg(&self.watch)
            .arg("--dest")
            .arg(&self.cold)
            .arg("--min-free-gb")
            .arg("0")
            .arg("-v")
            .arg("--once")
            .output()
            .expect("the sweep runs")
    }

    fn explain(&self, path: &str) -> Output {
        bin()
            .arg("explain")
            .arg(path)
            .arg("--watch")
            .arg(&self.watch)
            .arg("--dest")
            .arg(&self.cold)
            .arg("--min-free-gb")
            .arg("0")
            .output()
            .expect("explain runs")
    }
}

/// The central promise: a live pin prevents a move its rule would otherwise make, and the
/// sweep says the pin is why. Losing the pin check makes the sweep move the file and drops
/// the "pinned in the catalog" line, so this test fails on a broken feature.
#[test]
fn a_live_pin_blocks_a_sweep_and_is_named_as_the_reason() {
    let fixture = Fixture::new();
    let hot = fixture.place("shows/season1/ep1.mkv", 90);
    fixture.sync();
    assert!(fixture.catalog_path().is_file(), "sync writes the catalog");

    let pin = fixture.pin("shows/season1/ep1.mkv", "30d");
    assert_eq!(pin.status.code(), Some(0), "stderr: {}", stderr(&pin));
    assert!(
        stdout(&pin).contains("pinned shows/season1/ep1.mkv"),
        "the pin must be confirmed: {}",
        stdout(&pin)
    );

    let sweep = fixture.sweep();
    let out = stdout(&sweep);
    assert!(
        out.contains("pinned in the catalog"),
        "the sweep must name the pin as the reason: {out}"
    );
    assert!(
        !fs::symlink_metadata(&hot).unwrap().file_type().is_symlink(),
        "a pinned file must not have been moved"
    );
    assert!(
        hot.is_file(),
        "the pinned file must still be a regular file at the hot path"
    );
    assert!(
        !fixture.cold.join("shows/season1/ep1.mkv").exists(),
        "no copy may have been written while the pin held"
    );
}

/// An expired pin blocks nothing: policy applies again, and both the sweep and `explain`
/// show the lapse rather than hiding it.
#[test]
fn an_expired_pin_blocks_nothing_and_its_expiry_is_visible() {
    let fixture = Fixture::new();
    let hot = fixture.place("shows/season1/ep2.mkv", 90);
    fixture.sync();

    // A Unix timestamp in the past: recorded, and already lapsed.
    let pin = fixture.pin("shows/season1/ep2.mkv", "1");
    assert_eq!(pin.status.code(), Some(0), "stderr: {}", stderr(&pin));
    assert!(
        stdout(&pin).contains("already expired"),
        "the pin command must say it is already expired: {}",
        stdout(&pin)
    );

    // `explain` shows the pin and its expiry, and does *not* claim the pin is the verdict.
    let explained = fixture.explain("shows/season1/ep2.mkv");
    let explained_out = stdout(&explained);
    assert!(
        explained_out.contains("pin expired"),
        "explain must show the expiry: {explained_out}"
    );
    assert!(
        !explained_out.contains("pinned in the catalog until"),
        "an expired pin must not be the verdict: {explained_out}"
    );

    // And the sweep is free to move it.
    let sweep = fixture.sweep();
    let out = stdout(&sweep);
    assert!(
        !out.contains("pinned in the catalog"),
        "an expired pin must not be reported as a skip: {out}"
    );
    assert!(
        fs::symlink_metadata(&hot).unwrap().file_type().is_symlink(),
        "the file should have been moved once the pin lapsed: {out}"
    );
    assert!(
        fixture.cold.join("shows/season1/ep2.mkv").is_file(),
        "the cold copy is where the move put it"
    );
}

/// `explain` names a live pin in the same way it names a rule — with the instant it holds
/// until — so a pinned file's answer is never a silent "would never move".
#[test]
fn explain_names_a_live_pin_and_its_expiry() {
    let fixture = Fixture::new();
    fixture.place("shows/season1/ep3.mkv", 90);
    fixture.sync();
    let pin = fixture.pin("shows/season1/ep3.mkv", "30d");
    assert_eq!(pin.status.code(), Some(0), "stderr: {}", stderr(&pin));

    let explained = fixture.explain("shows/season1/ep3.mkv");
    let out = stdout(&explained);
    assert!(
        out.contains("pinned in the catalog until"),
        "explain's verdict must name the pin: {out}"
    );
    assert!(
        out.contains("in force"),
        "the pin's expiry must read as still in force: {out}"
    );
    // A pinned file is not managed: it would not move now.
    assert_eq!(
        explained.status.code(),
        Some(1),
        "stderr: {}",
        stderr(&explained)
    );
}

/// The listing includes both states so an operator can see what is protecting what, and how
/// long for.
#[test]
fn pin_list_shows_live_and_expired_pins() {
    let fixture = Fixture::new();
    fixture.place("shows/live.bin", 90);
    fixture.place("shows/lapsed.bin", 90);
    fixture.sync();
    assert_eq!(fixture.pin("shows/live.bin", "30d").status.code(), Some(0));
    assert_eq!(fixture.pin("shows/lapsed.bin", "1").status.code(), Some(0));

    let listed = fixture.list_pins();
    assert_eq!(listed.status.code(), Some(0), "stderr: {}", stderr(&listed));
    let out = stdout(&listed);
    assert!(
        out.contains("pins: 2 recorded, 1 in force"),
        "the count must separate live from expired: {out}"
    );
    assert!(
        out.contains("shows/live.bin: pinned until") && out.contains("(in force)"),
        "{out}"
    );
    assert!(
        out.contains("shows/lapsed.bin: pin expired") && out.contains("(blocks nothing)"),
        "{out}"
    );
}

/// `unpin` is idempotent and releases the block.
#[test]
fn unpin_is_idempotent_and_releases_the_pin() {
    let fixture = Fixture::new();
    let hot = fixture.place("shows/release.bin", 90);
    fixture.sync();
    assert_eq!(
        fixture.pin("shows/release.bin", "30d").status.code(),
        Some(0)
    );

    let first = fixture.unpin("shows/release.bin");
    assert_eq!(first.status.code(), Some(0), "stderr: {}", stderr(&first));
    // A second unpin on a path with no pin is still a clean success — a cron job needs no
    // failure handling for the common "already clear" case.
    let second = fixture.unpin("shows/release.bin");
    assert_eq!(second.status.code(), Some(0), "stderr: {}", stderr(&second));

    let sweep = fixture.sweep();
    assert!(
        fs::symlink_metadata(&hot).unwrap().file_type().is_symlink(),
        "with the pin cleared the file must move: {}",
        stdout(&sweep)
    );
    assert!(
        !stdout(&sweep).contains("pinned in the catalog"),
        "nothing is pinned any more"
    );
}

/// A pin on a path the catalog does not know is a finding (exit 1), not a silent no-op: a
/// typo must not look like protection.
#[test]
fn pinning_an_unknown_path_is_a_finding() {
    let fixture = Fixture::new();
    fixture.place("shows/known.bin", 90);
    fixture.sync();

    let pin = fixture.pin("shows/typo.bin", "30d");
    assert_eq!(pin.status.code(), Some(1), "stderr: {}", stderr(&pin));
    assert!(
        stderr(&pin).contains("not named in the catalog"),
        "the finding must explain itself: {}",
        stderr(&pin)
    );
}

/// A missing catalog is a usage error (exit 2): a pin must attach to a known object, and
/// `pin` never creates a catalog (invariant 9).
#[test]
fn pinning_without_a_catalog_is_a_usage_error() {
    let fixture = Fixture::new();
    let missing = fixture.watch.join("nowhere.sqlite");
    let output = bin()
        .arg("pin")
        .arg("shows/known.bin")
        .arg("--until")
        .arg("30d")
        .arg("--catalog")
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(!missing.exists(), "pin must never create a catalog");
}

/// `--until` refuses something that is neither a duration nor a timestamp, rather than
/// silently pinning for zero seconds.
#[test]
fn an_unparseable_until_is_a_usage_error() {
    let fixture = Fixture::new();
    fixture.place("shows/known.bin", 90);
    fixture.sync();

    let pin = fixture.pin("shows/known.bin", "soon");
    assert_eq!(pin.status.code(), Some(2), "stderr: {}", stderr(&pin));
    assert!(
        stderr(&pin).contains("neither a Unix timestamp nor a duration"),
        "the refusal must say what it wanted: {}",
        stderr(&pin)
    );
    // And nothing was written: the listing still shows no pin.
    assert!(stdout(&fixture.list_pins()).contains("pins: 0 recorded"));
}

/// The default catalog path is named by `CATALOG_NAME`; a regression there would make every
/// `pin` above miss the catalog the sweep reads.
#[test]
fn the_default_catalog_lives_beside_the_watch_root() {
    let fixture = Fixture::new();
    fixture.sync();
    assert_eq!(
        fixture
            .catalog_path()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        CATALOG_NAME
    );
}
