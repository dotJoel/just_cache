//! End-to-end behaviour of `policy.toml` through the real binary (issue #41).
//!
//! The design (§5) makes two promises about rules, and both are promises a user sees
//! rather than things the code can assert about itself: a rule decides *which* tier a file
//! belongs on, and every transition records which rule fired so `explain` can name it.
//! A rule that silently does nothing looks exactly like a tree with nothing to move, so
//! these tests drive the binary and read what a shell sees — the move, the symlink, the
//! recorded rule in `explain`, and the refusals.
//!
//! Moves here stay on one filesystem (the `rename` path). The cross-device copy fallback is
//! covered by its own tests; what is under test here is *which candidates a rule selects
//! and where it sends them*, which the rename path exercises exactly.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

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

fn unique_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "just_cache_policy_{tag}_{}_{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

/// A watch tree on the `ssd` tier, a cold root on `hdd_parked`, and configs written where
/// each test wants them. Everything lives under one temporary root so cleanup is one tree.
struct Fixture {
    root: PathBuf,
    watch: PathBuf,
    cold: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = unique_root(tag);
        let watch = root.join("hot");
        let cold = root.join("cold");
        fs::create_dir_all(watch.join("shows")).unwrap();
        fs::create_dir_all(&cold).unwrap();
        Self { root, watch, cold }
    }

    /// Write a file under the watch root and stamp it `days` idle.
    fn place(&self, relative: &str, bytes: &[u8], days: u64) -> PathBuf {
        let path = self.watch.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        set_times(&path, days_ago(days));
        path
    }

    /// `ssd` is the watch root at ms recall, `hdd_parked` the cold root at s recall — the
    /// pair the design's §5 chain starts from.
    fn tiers(&self) -> String {
        format!(
            "[tiers.ssd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n\n\
             [tiers.hdd_parked]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"s\"\ncopies = 1\n",
            self.watch.display(),
            self.cold.display()
        )
    }

    /// `tiers.toml` beside the watch root, where the default lookup expects it.
    fn write_default_tiers(&self) -> PathBuf {
        let path = self.watch.join("tiers.toml");
        fs::write(&path, self.tiers()).unwrap();
        path
    }

    fn write_default_policy(&self, text: &str) -> PathBuf {
        let path = self.watch.join("policy.toml");
        fs::write(&path, text).unwrap();
        path
    }

    /// One sweep against this tree, with the default config lookup. `--min-free-gb 0` keeps
    /// the room check out of the way of a tiny temp filesystem.
    fn sweep(&self, extra: &[&str]) -> Output {
        let mut command = bin();
        command
            .arg("--watch")
            .arg(&self.watch)
            .arg("--dest")
            .arg(&self.cold)
            .arg("--min-free-gb")
            .arg("0")
            .arg("--once");
        for arg in extra {
            command.arg(arg);
        }
        command.output().expect("the binary should run")
    }

    fn explain(&self, path: &str, extra: &[&str]) -> Output {
        let mut command = bin();
        command
            .arg("explain")
            .arg(path)
            .arg("--watch")
            .arg(&self.watch)
            .arg("--dest")
            .arg(&self.cold)
            .arg("--min-free-gb")
            .arg("0");
        for arg in extra {
            command.arg(arg);
        }
        command.output().expect("the binary should run")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

const CATCH_ALL: &str = "[[rule]]\nname = \"tiering\"\nmatch = \"**\"\ndown = { after_idle = \"30d\", from = \"ssd\", to = \"hdd_parked\" }\nup = { on_access = true }\n";

#[test]
fn a_rule_moves_on_its_idle_gate_and_explain_records_which_rule_fired() {
    let fx = Fixture::new("moves");
    fx.write_default_tiers();
    fx.write_default_policy(CATCH_ALL);
    let old = fx.place("shows/old.mkv", b"movie bytes", 90);
    fx.place("shows/warm.mkv", b"still hot", 5);

    // A catalog first, so the sweep has a name to attach the rule to and `explain` can read
    // it back. `catalog sync` is also how the catalog is created (a sweep never creates one).
    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&fx.watch)
        .arg("--dest")
        .arg(&fx.cold)
        .output()
        .expect("catalog sync runs");
    assert_eq!(sync.status.code(), Some(0), "sync: {}", stderr(&sync));
    // Sync hashes file contents and can update atime on relatime mounts; restore the
    // test stamp afterwards so the rule's 30-day gate is the variable under test.
    set_times(&old, days_ago(90));

    let output = fx.sweep(&[]);
    assert_eq!(output.status.code(), Some(0), "sweep: {}", stderr(&output));

    // The rule fired for the 90-day file: bytes moved, a symlink left behind…
    assert!(
        fx.watch.join("shows/old.mkv").is_symlink(),
        "the idle file should be a symlink now"
    );
    assert!(
        fx.cold.join("shows/old.mkv").exists(),
        "the bytes should be on the cold tier"
    );
    // …and only for it: the 5-day file is warm under the same 30d rule.
    assert!(
        !fx.watch.join("shows/warm.mkv").is_symlink(),
        "a 5-day idle file must not pass a 30d rule"
    );
    assert!(!fx.cold.join("shows/warm.mkv").exists());

    // The transition recorded the rule, and `explain` names it.
    let explained = fx.explain("shows/old.mkv", &[]);
    assert_eq!(explained.status.code(), Some(0), "{}", stderr(&explained));
    let text = stdout(&explained);
    assert!(
        text.contains("last rule") && text.contains("tiering"),
        "explain should name the recorded rule:\n{text}"
    );
    assert!(
        text.contains("promotes it back") && text.contains("on `hdd_parked`"),
        "explain should evaluate the rule's access-up decision:\n{text}"
    );
}

#[test]
fn dry_run_evaluates_the_same_rule_without_moving_anything() {
    let fx = Fixture::new("dry_run");
    fx.write_default_tiers();
    fx.write_default_policy(CATCH_ALL);
    fx.place("shows/old.mkv", b"movie bytes", 90);

    let output = fx.sweep(&["--dry-run", "-v"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("would move") && text.contains("tiering"),
        "dry-run should report the same rule decision:\n{text}"
    );
    assert!(
        fx.watch.join("shows/old.mkv").is_file() && !fx.watch.join("shows/old.mkv").is_symlink(),
        "dry-run must leave the source untouched"
    );
    assert!(!fx.cold.join("shows/old.mkv").exists());
}

#[test]
fn explain_names_the_rule_for_a_file_that_is_not_idle_enough_yet() {
    let fx = Fixture::new("too_warm");
    fx.write_default_tiers();
    fx.write_default_policy(CATCH_ALL);
    fx.place("shows/warm.mkv", b"still hot", 5);

    let explained = fx.explain("shows/warm.mkv", &[]);
    // `explain` exits nonzero when the answer is "not this sweep" — what matters here is
    // that it is a real answer, not a crash, and that it names the rule.
    assert_ne!(explained.status.code(), Some(101), "{}", stderr(&explained));
    let text = stdout(&explained);
    assert!(
        text.contains("tiering") && text.contains("after_idle"),
        "explain should name the rule and why it has not fired:\n{text}"
    );
}

#[test]
fn a_pin_protects_a_file_from_the_rule_that_matches_it() {
    let fx = Fixture::new("pin");
    fx.write_default_tiers();
    fx.write_default_policy(
        "[[rule]]\nname = \"tiering\"\nmatch = \"**\"\n\
         down = { after_idle = \"30d\", from = \"ssd\", to = \"hdd_parked\" }\n\
         pins = [\"*.drp\"]\n",
    );
    fx.place("projects/active.drp", b"project file", 90);

    let explained = fx.explain("projects/active.drp", &[]);
    assert_ne!(explained.status.code(), Some(101), "{}", stderr(&explained));
    let text = stdout(&explained);
    assert!(
        text.contains("pinned") && text.contains("tiering") && text.contains("*.drp"),
        "explain should name the pin, not the transition:\n{text}"
    );

    // And the mover agrees: a pinned file is not a candidate however idle it is.
    let output = fx.sweep(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        !fx.watch.join("projects/active.drp").is_symlink(),
        "a pinned file must not be moved"
    );
    assert!(!fx.cold.join("projects/active.drp").exists());
}

#[test]
fn a_per_path_rule_overrides_a_catch_all_for_its_subtree() {
    let fx = Fixture::new("override");
    fx.write_default_tiers();
    fx.write_default_policy(&format!(
        "{CATCH_ALL}\n[[rule]]\nname = \"shows-are-hotter\"\nmatch = \"shows/**\"\n\
         down = {{ after_idle = \"3d\", from = \"ssd\", to = \"hdd_parked\" }}\n"
    ));
    // Both are 5 days idle: past the override's 3d, short of the catch-all's 30d.
    fx.place("shows/old.mkv", b"movie bytes", 5);
    fx.place("readme.md", b"still hot", 5);

    let output = fx.sweep(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        fx.watch.join("shows/old.mkv").is_symlink(),
        "the override should move a 5-day file"
    );
    assert!(
        !fx.watch.join("readme.md").is_symlink(),
        "the catch-all's 30d gate must still hold outside shows/"
    );
}

#[test]
fn a_rule_naming_an_unconfigured_tier_is_refused_by_rule_name() {
    let fx = Fixture::new("unconfigured");
    fx.write_default_tiers();
    fx.write_default_policy(
        "[[rule]]\nname = \"bogus\"\nmatch = \"**\"\n\
         down = { after_idle = \"30d\", from = \"ssd\", to = \"nosuch\" }\n",
    );
    fx.place("shows/old.mkv", b"movie bytes", 90);

    let output = fx.sweep(&[]);
    assert_eq!(output.status.code(), Some(2), "should be a usage error");
    let err = stderr(&output);
    assert!(
        err.contains("bogus") && err.contains("nosuch"),
        "the refusal should name the rule and the tier:\n{err}"
    );
    assert!(
        fx.watch.join("shows/old.mkv").is_file(),
        "a refused policy must not have moved anything"
    );
}

#[test]
fn an_unparseable_duration_is_refused_naming_the_rule() {
    let fx = Fixture::new("bad_duration");
    fx.write_default_tiers();
    fx.write_default_policy(
        "[[rule]]\nname = \"typo\"\nmatch = \"**\"\n\
         down = { after_idle = \"soon\", from = \"ssd\", to = \"hdd_parked\" }\n",
    );
    let output = fx.sweep(&[]);
    assert_eq!(output.status.code(), Some(2), "should be a usage error");
    let err = stderr(&output);
    assert!(
        err.contains("typo") && err.contains("after_idle"),
        "the refusal should name the rule and the field:\n{err}"
    );
}

#[test]
fn a_volatile_tier_is_refused_as_a_rule_target() {
    let fx = Fixture::new("volatile");
    fs::write(
        fx.watch.join("tiers.toml"),
        format!(
            "{}[tiers.ram]\nkind = \"ram\"\npath = \"{}\"\nvolatility = \"volatile\"\nrecall = \"ms\"\ncopies = 1\n",
            fx.tiers(),
            fx.cold.display()
        ),
    )
    .unwrap();
    fx.write_default_policy(
        "[[rule]]\nname = \"cache-please\"\nmatch = \"**\"\n\
         down = { after_idle = \"30d\", from = \"ssd\", to = \"ram\" }\n",
    );
    let output = fx.sweep(&[]);
    assert_eq!(output.status.code(), Some(2), "should be a usage error");
    let err = stderr(&output);
    assert!(
        err.contains("cache-please") && err.contains("ram") && err.contains("volatile"),
        "the refusal should name the rule and the volatile tier:\n{err}"
    );
}

#[test]
fn without_a_policy_toml_the_flag_driven_decision_stands() {
    let fx = Fixture::new("no_policy");
    fx.write_default_tiers();
    fx.place("shows/old.mkv", b"movie bytes", 90);
    fx.place("shows/warm.mkv", b"still hot", 5);

    // No policy: the flags are the whole decision, exactly as before rules existed.
    let output = fx.sweep(&["--min-idle-days", "30"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(fx.watch.join("shows/old.mkv").is_symlink());
    assert!(!fx.watch.join("shows/warm.mkv").is_symlink());

    let text = stdout(&fx.explain("shows/old.mkv", &["--min-idle-days", "30"]));
    assert!(
        text.contains("no policy.toml"),
        "explain should say there is no policy rather than inventing one:\n{text}"
    );
}

#[test]
fn a_config_file_in_the_watched_tree_is_never_moved() {
    let fx = Fixture::new("config_reserved");
    let policy = fx.write_default_policy(CATCH_ALL);
    let tiers = fx.write_default_tiers();
    // Age the configs far past every gate: the sweep reads them, so it must still not move
    // them — moving `policy.toml` would change the rules governing the running sweep.
    set_times(&policy, days_ago(400));
    set_times(&tiers, days_ago(400));
    fx.place("shows/old.mkv", b"movie bytes", 90);

    let output = fx.sweep(&[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        fx.watch.join("policy.toml").is_file() && !fx.watch.join("policy.toml").is_symlink(),
        "policy.toml must stay where it is"
    );
    assert!(
        fx.watch.join("tiers.toml").is_file() && !fx.watch.join("tiers.toml").is_symlink(),
        "tiers.toml must stay where it is"
    );
    assert!(!fx.cold.join("policy.toml").exists());
    assert!(!fx.cold.join("tiers.toml").exists());
    assert!(
        fx.watch.join("shows/old.mkv").is_symlink(),
        "the ordinary file should still move"
    );
}

#[test]
fn an_explicit_policy_flag_names_a_file_outside_the_watch_tree() {
    let fx = Fixture::new("explicit");
    let tiers = fx.root.join("tiers.toml");
    fs::write(&tiers, fx.tiers()).unwrap();
    let policy = fx.root.join("policy.toml");
    fs::write(&policy, CATCH_ALL).unwrap();
    fx.place("shows/old.mkv", b"movie bytes", 90);

    let output = fx.sweep(&[
        "--tiers",
        tiers.to_str().unwrap(),
        "--policy",
        policy.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        fx.watch.join("shows/old.mkv").is_symlink(),
        "the explicitly named policy should have governed the sweep"
    );
    assert!(fx.cold.join("shows/old.mkv").exists());
}
