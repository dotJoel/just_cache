//! The append-only events file (#169), end to end through the real binary.
//!
//! The claims under test: a scripted maintenance pass (sweep, scrub, gc — plus the sync
//! that set the tree up) appends one parseable JSON line per report; the walk and `audit`
//! never report the events file as a finding and never move it; a truncated final line is
//! skipped by a reader rather than called corrupt; and `-v`/`-q`/`--json` land the same
//! record, because the event is built from the report and not from stdout.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use just_cache::catalog::CATALOG_NAME;
use just_cache::events::{self, EVENTS_NAME, EVENTS_SEGMENT_NAME};

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

/// The report document inside an event line: everything after `"report":`, without the
/// envelope's closing brace. Comparing these across runs is how the verbosity-invariance
/// claim is checked.
fn report_of(line: &str) -> &str {
    let key = "\"report\":";
    let start = line.find(key).expect("an event carries a report") + key.len();
    &line[start..line.len() - 1]
}

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

    fn events_path(&self) -> PathBuf {
        self.hot.join(EVENTS_NAME)
    }

    fn events(&self) -> Vec<String> {
        events::read_events(&self.events_path()).unwrap()
    }

    fn sweep(&self, extra: &[&str]) -> Output {
        let mut command = bin();
        command
            .args(["sweep", "--watch"])
            .arg(&self.hot)
            .arg("--dest")
            .arg(&self.cold)
            .args(["--once", "--min-idle-days", "999"]);
        command.args(extra);
        command.output().unwrap()
    }
}

#[test]
fn a_scripted_pass_appends_one_parseable_line_per_report() {
    let fixture = Fixture::build();
    // A file that is not old enough stays put, so the sweep has a report to record either
    // way, and `scrub` and `gc` have an evaluated tree to report on.
    fs::write(fixture.hot.join("fresh.bin"), b"recently touched").unwrap();
    // Ingest the new file so `gc` does not see it as unreferenced garbage and exit 1; a
    // sync is itself a maintenance pass, so it records its own line.
    let resync = fixture.sync();
    assert_eq!(code(&resync), 0, "sync failed: {}", stderr(&resync));

    let sweep = fixture.sweep(&[]);
    assert_eq!(code(&sweep), 0, "sweep failed: {}", stderr(&sweep));

    let scrub = bin()
        .args(["scrub", "--catalog"])
        .arg(&fixture.catalog)
        .output()
        .unwrap();
    assert_eq!(code(&scrub), 0, "scrub failed: {}", stderr(&scrub));

    let gc = bin()
        .args(["gc", "--catalog"])
        .arg(&fixture.catalog)
        .output()
        .unwrap();
    assert_eq!(code(&gc), 0, "gc failed: {}", stderr(&gc));

    let events = fixture.events();
    // Two syncs (one in `build`, one above) plus the three passes: five reports, and
    // `read_events` keeps only complete objects — so a count of five means all five
    // parsed.
    let passes: Vec<&str> = events
        .iter()
        .filter_map(|line| {
            let key = "\"pass\":\"";
            let start = line.find(key)? + key.len();
            let end = line[start..].find('"')? + start;
            Some(&line[start..end])
        })
        .collect();
    assert!(
        passes.contains(&"catalog-sync")
            && passes.contains(&"sweep")
            && passes.contains(&"scrub")
            && passes.contains(&"gc"),
        "every maintenance pass records a line, got {passes:?}"
    );
    assert_eq!(
        events.len(),
        5,
        "exactly one line per report, got {}: {events:?}",
        events.len()
    );
    // The event is the report, not empty scaffolding: the gc report carries its counts.
    let gc_line = events
        .iter()
        .find(|line| line.contains("\"pass\":\"gc\""))
        .unwrap();
    assert!(report_of(gc_line).contains("\"files\":"), "{gc_line}");
}

#[test]
fn verbosity_changes_stdout_but_not_the_record() {
    let fixture = Fixture::build();
    fs::write(fixture.hot.join("fresh.bin"), b"recently touched").unwrap();

    let quiet = fixture.sweep(&["-q"]);
    assert_eq!(code(&quiet), 0, "sweep -q failed: {}", stderr(&quiet));
    let verbose = fixture.sweep(&["-v"]);
    assert_eq!(code(&verbose), 0, "sweep -v failed: {}", stderr(&verbose));

    let events = fixture.events();
    let sweeps: Vec<&str> = events
        .iter()
        .filter(|line| line.contains("\"pass\":\"sweep\""))
        .map(String::as_str)
        .collect();
    assert_eq!(sweeps.len(), 2, "one event per sweep: {events:?}");
    assert_eq!(
        report_of(sweeps[0]),
        report_of(sweeps[1]),
        "`-q` and `-v` must land the same report; verbosity is stdout presentation only"
    );

    // `--json` is a third presentation of the same report for `gc`.
    let gc_json = bin()
        .args(["gc", "--catalog"])
        .arg(&fixture.catalog)
        .arg("--json")
        .output()
        .unwrap();
    let gc_quiet = bin()
        .args(["gc", "--catalog"])
        .arg(&fixture.catalog)
        .arg("-q")
        .output()
        .unwrap();
    assert_eq!(code(&gc_json), code(&gc_quiet), "same findings, same exit");
    let events = fixture.events();
    let gcs: Vec<&str> = events
        .iter()
        .filter(|line| line.contains("\"pass\":\"gc\""))
        .map(String::as_str)
        .collect();
    assert_eq!(
        report_of(gcs[0]),
        report_of(gcs[1]),
        "`--json` and `-q` must land the same gc record"
    );
}

#[test]
fn the_walk_and_audit_never_see_the_events_file() {
    let fixture = Fixture::build();
    fs::write(fixture.hot.join("fresh.bin"), b"recently touched").unwrap();
    fixture.sweep(&[]);

    let events_path = fixture.events_path();
    assert!(events_path.is_file(), "the pass wrote an events file");

    // The walk that decides what to move must not offer the events file as a candidate.
    let walk = just_cache::disk_management::list_files_recursive(&fixture.hot).unwrap();
    assert!(
        walk.iter().all(|entry| !entry.path.ends_with(EVENTS_NAME)
            && !entry.path.ends_with(EVENTS_SEGMENT_NAME)),
        "the walk must skip the events file"
    );

    // And `audit` must not report it as a finding, nor move it.
    let audit = bin()
        .args(["audit", "--watch"])
        .arg(&fixture.hot)
        .arg("--dest")
        .arg(&fixture.cold)
        .arg("--catalog")
        .arg(&fixture.catalog)
        .output()
        .unwrap();
    let text = format!("{}{}", stdout(&audit), stderr(&audit));
    assert!(
        !text.contains(EVENTS_NAME),
        "audit named the events file: {text}"
    );
    let metadata = fs::symlink_metadata(&events_path).unwrap();
    assert!(
        metadata.is_file(),
        "the events file must never be moved or replaced by a symlink"
    );
}

#[test]
fn a_truncated_final_line_is_skipped_by_a_reader() {
    let fixture = Fixture::build();
    fs::write(fixture.hot.join("fresh.bin"), b"recently touched").unwrap();
    fixture.sweep(&[]);

    let before = fixture.events().len();
    assert!(before >= 2, "sync plus sweep recorded at least two lines");

    // A crash mid-write: cut the last line short, mid-object, with no closing brace and no
    // trailing newline.
    let text = fs::read_to_string(fixture.events_path()).unwrap();
    let cut = text.len() - 12;
    fs::write(fixture.events_path(), &text[..cut]).unwrap();

    let after = fixture.events();
    assert_eq!(
        after.len(),
        before - 1,
        "the torn tail is skipped, not reported as corrupt"
    );
}

#[test]
fn a_pass_with_a_catalog_outside_the_tree_keeps_the_events_file_beside_it() {
    // `--catalog` names where the record goes: a catalog outside the watched tree must not
    // scatter an events file inside it (or, here, create one at all under `hot`).
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot");
    let cold = dir.path().join("cold");
    let elsewhere = dir.path().join("meta");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let catalog = elsewhere.join(CATALOG_NAME);

    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .args(["--catalog"])
        .arg(&catalog)
        .output()
        .unwrap();
    assert_eq!(code(&sync), 0, "sync failed: {}", stderr(&sync));

    assert!(
        elsewhere.join(EVENTS_NAME).is_file(),
        "the events file sits beside the catalog"
    );
    assert!(
        !hot.join(EVENTS_NAME).exists(),
        "a catalog outside the tree must not scatter an events file inside it"
    );
}
