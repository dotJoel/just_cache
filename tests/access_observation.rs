//! Access observation by the namespace provider (issue #43, design §4) against a real
//! catalog, with no mount.
//!
//! The FUSE handlers are thin: `open` calls [`AccessLog::opened`], `read`/`release` call
//! [`AccessLog::touched`], and `release`/`destroy`/the interval flush call
//! [`AccessLog::flush`]. CI has no usable `/dev/fuse`, so these tests drive that same log
//! directly and then read what the catalog, a sweep and `explain` make of it. That the
//! handlers really call the log is covered only by the `JUST_CACHE_TEST_FUSE=1` mount test,
//! which CI does not run.
//!
//! "atime is not read" is proved by contradiction: the object's atime is stamped 400 days
//! old — a rule would move it — yet a provider-observed access keeps it put, and `explain`
//! names `provider` as the source. A control file with the same old atime and no observed
//! access does move, so the test fails if the sweep goes back to judging by atime.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use just_cache::catalog::Catalog;
use just_cache::AccessLog;

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

    fn open(&self) -> Catalog {
        Catalog::open(self.catalog_path()).unwrap()
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

    /// Re-establish a placed file's idle stamp.
    ///
    /// The tests stamp a file "idle past the threshold" with `set_times`, but the mover's
    /// idle signal *is* atime, and any read of the file refreshes atime on a mount that
    /// maintains it under the `relatime` rule — which is every normal Linux mount, and is
    /// why `catalog sync` (which hashes the file) undoes the stamp, while a `noatime` mount
    /// (the box these tests were written on) hides it. A test that asserts "this moved
    /// because it was idle" therefore has to re-stamp after the last read of the file and
    /// before the sweep whose decision it asserts. This is that call, named for the trap
    /// rather than repeated inline at every site.
    fn restamp(&self, path: &Path, days: u64) {
        set_times(path, days_ago(days));
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

/// Observe `opens` opens of `path` (each followed by a read and a close, as a consumer
/// would) and flush, exactly as the mount's handlers do.
fn observe(catalog: &mut Catalog, path: &str, opens: usize) {
    let mut log = AccessLog::new();
    for _ in 0..opens {
        log.opened(path, SystemTime::now());
        log.touched(path, SystemTime::now());
        log.touched(path, SystemTime::now());
    }
    log.flush(catalog).expect("flush writes the batch");
    assert_eq!(
        log.pending(),
        0,
        "a successful flush leaves nothing pending"
    );
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn an_access_through_the_provider_updates_the_row_and_a_second_increments_it() {
    let fixture = Fixture::new();
    fixture.place("shows/ep1.mkv", 400);
    fixture.sync();

    let mut catalog = fixture.open();
    let before = catalog.record_for_path("shows/ep1.mkv").unwrap().unwrap();
    assert_eq!(before.accesses, Some(0));
    assert!(
        !before.access_observed,
        "sync ingests a timestamp, not an observation"
    );

    observe(&mut catalog, "shows/ep1.mkv", 1);
    let once = catalog.record_for_path("shows/ep1.mkv").unwrap().unwrap();
    assert_eq!(
        once.accesses,
        Some(1),
        "one open is one access, whatever it read"
    );
    assert!(once.access_observed);
    let stamp = once.last_access.unwrap();
    assert!(
        (now_seconds() - stamp).abs() < 60,
        "last_access is the moment of access, not the 400-day-old atime: {stamp}"
    );
    // The access is an observation, never a transition: no rule is recorded.
    assert_eq!(once.rule, None);

    observe(&mut catalog, "shows/ep1.mkv", 1);
    let twice = catalog.record_for_path("shows/ep1.mkv").unwrap().unwrap();
    assert_eq!(
        twice.accesses,
        Some(2),
        "a second access increments the counter"
    );
    assert!(
        twice.last_access.unwrap() >= stamp,
        "last_access never moves backwards"
    );
}

#[test]
fn a_late_flush_never_makes_an_object_look_colder() {
    let fixture = Fixture::new();
    fixture.place("a.bin", 1);
    fixture.sync();
    let mut catalog = fixture.open();
    observe(&mut catalog, "a.bin", 1);
    let recent = catalog
        .record_for_path("a.bin")
        .unwrap()
        .unwrap()
        .last_access;

    let mut log = AccessLog::new();
    log.opened("a.bin", days_ago(10));
    log.flush(&mut catalog).unwrap();
    let after = catalog.record_for_path("a.bin").unwrap().unwrap();
    assert_eq!(after.last_access, recent);
    assert_eq!(after.accesses, Some(2));
}

#[test]
fn an_unbuffered_observation_is_not_in_the_catalog_until_flushed() {
    // The named loss bound: what has not been flushed is exactly what a SIGKILL loses.
    let fixture = Fixture::new();
    fixture.place("a.bin", 1);
    fixture.sync();
    let mut catalog = fixture.open();
    let mut log = AccessLog::new();
    log.opened("a.bin", SystemTime::now());
    assert!(
        !log.due(std::time::Instant::now()),
        "inside the flush interval"
    );
    assert_eq!(log.pending(), 1);
    let row = catalog.record_for_path("a.bin").unwrap().unwrap();
    assert_eq!(row.accesses, Some(0), "buffered, not yet written");
    assert!(log.due(std::time::Instant::now() + just_cache::FLUSH_INTERVAL));
    log.flush(&mut catalog).unwrap();
    assert_eq!(
        catalog.record_for_path("a.bin").unwrap().unwrap().accesses,
        Some(1)
    );
}

#[test]
fn a_name_the_catalog_does_not_know_writes_no_row() {
    let fixture = Fixture::new();
    fixture.place("a.bin", 1);
    fixture.sync();
    let mut catalog = fixture.open();
    let mut log = AccessLog::new();
    log.opened("created-through-the-mount.txt", SystemTime::now());
    assert_eq!(log.flush(&mut catalog).unwrap(), 0);
}

#[test]
fn atime_is_not_read_for_an_object_the_provider_observed() {
    let fixture = Fixture::new();
    let watched = fixture.place("shows/watched.mkv", 400);
    let control = fixture.place("shows/control.mkv", 400);
    fixture.sync();
    observe(&mut fixture.open(), "shows/watched.mkv", 1);
    // Both files now carry a 400-day-old atime: by timestamp both are past the 30-day rule.
    fixture.restamp(&watched, 400);
    fixture.restamp(&control, 400);

    let explained = fixture.explain(watched.to_str().unwrap());
    let text = stdout(&explained);
    assert_ne!(explained.status.code(), Some(101), "{text}");
    assert!(
        text.contains("via provider"),
        "explain names the provider as the source:\n{text}"
    );
    assert!(
        text.contains("observed by the mount provider"),
        "the catalog note says the count came from the provider:\n{text}"
    );
    assert!(
        !text.contains("atime equals mtime"),
        "no atime caveat for an observed stamp:\n{text}"
    );

    let control_text = stdout(&fixture.explain(control.to_str().unwrap()));
    assert!(
        control_text.contains("via atime"),
        "control keeps the symlink-provider stamp:\n{control_text}"
    );

    let swept = fixture.sweep();
    assert_eq!(swept.status.code(), Some(0), "{}", stderr(&swept));
    assert!(
        !fs::symlink_metadata(&watched).unwrap().is_symlink(),
        "the observed access keeps it hot even though its atime is 400 days old:\n{}",
        stdout(&swept)
    );
    assert!(
        fs::symlink_metadata(&control).unwrap().is_symlink(),
        "the control (same atime, no observation) moves, so the sweep did run the rule:\n{}",
        stdout(&swept)
    );
}
