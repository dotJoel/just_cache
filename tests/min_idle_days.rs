//! `--min-idle-days` is an `f64`, and `f64` parses values the idle gate cannot honour.
//!
//! `NaN` is the quiet one: `NaN < 0.0` is false and `NaN.max(0.0)` is `0.0`, so the old
//! validation let it through and the gate became `Duration::ZERO` — every file, including
//! one accessed a second ago, became a move candidate. `inf`, and a finite value whose
//! seconds overflow `Duration`, reached `from_secs_f64` and panicked. These tests drive the
//! binary, because the contract is what a shell sees: a refusal, a nonzero exit, and the
//! warm file still in place. A test that called a helper in-process would not catch a
//! regression that reintroduces the panic in the conversion.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn unique_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "just_cache_min_idle_{tag}_{}_{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    root
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

/// A watched tree holding one *recently* used file. A working idle gate leaves it alone;
/// a gate silently disabled to `Duration::ZERO` moves it, which is the regression under
/// test. Returns `(root, tree, dest)`.
fn warm_tree(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = unique_root(tag);
    let tree = root.join("tree");
    let dest = root.join("dest");
    fs::create_dir_all(&tree).unwrap();
    fs::create_dir_all(&dest).unwrap();
    let file = tree.join("recent.bin");
    fs::write(&file, b"payload\n").unwrap();
    set_times(&file, SystemTime::now() - Duration::from_secs(1));
    (root, tree, dest)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The three inputs the issue names, with the phrase each refusal must carry.
const BAD_VALUES: &[(&str, &str)] = &[("NaN", "finite"), ("inf", "finite"), ("1e300", "maximum")];

fn assert_refused(output: &Output, value: &str, expected: &str) {
    let err = stderr(output);
    assert!(
        !output.status.success(),
        "--min-idle-days {value} should be refused, but the run succeeded"
    );
    assert!(
        output.status.code() != Some(101),
        "--min-idle-days {value} panicked instead of being refused:\n{err}"
    );
    assert!(
        err.contains("--min-idle-days") && err.contains(expected),
        "--min-idle-days {value} produced an unclear refusal:\n{err}"
    );
}

#[test]
fn sweep_refuses_non_finite_and_overflowing_min_idle_days() {
    for &(value, expected) in BAD_VALUES {
        let (root, tree, dest) = warm_tree(&format!("sweep_{value}"));
        let output = bin()
            .arg("--watch")
            .arg(&tree)
            .arg("--dest")
            .arg(&dest)
            .arg("--min-idle-days")
            .arg(value)
            .arg("--min-free-gb")
            .arg("0")
            .arg("--once")
            .output()
            .expect("the binary should run");
        assert_refused(&output, value, expected);
        // The whole point: a file touched a second ago must not have moved. With the old
        // code it did (NaN) or the run panicked (inf/1e300) before this could be checked.
        assert!(
            tree.join("recent.bin").exists(),
            "--min-idle-days {value} moved a file that was in active use"
        );
        assert!(
            !dest.join("recent.bin").exists(),
            "--min-idle-days {value} placed a copy at the destination"
        );
        fs::remove_dir_all(&root).ok();
    }
}

#[test]
fn explain_refuses_non_finite_and_overflowing_min_idle_days() {
    for &(value, expected) in BAD_VALUES {
        let (root, tree, dest) = warm_tree(&format!("explain_{value}"));
        let output = bin()
            .arg("explain")
            .arg("recent.bin")
            .arg("--watch")
            .arg(&tree)
            .arg("--dest")
            .arg(&dest)
            .arg("--min-idle-days")
            .arg(value)
            .arg("--min-free-gb")
            .arg("0")
            .output()
            .expect("the binary should run");
        assert_refused(&output, value, expected);
        assert!(
            output.stdout.is_empty(),
            "--min-idle-days {value} still printed an explanation:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(tree.join("recent.bin").exists());
        fs::remove_dir_all(&root).ok();
    }
}

/// The refusal is narrow: an ordinary value still runs, and a valid idle gate still
/// protects a warm file. Without this, a fix that rejected everything, or one that
/// zeroed the gate, would pass the refusals above.
#[test]
fn valid_min_idle_days_still_runs_and_protects_a_warm_file() {
    let (root, tree, dest) = warm_tree("valid");
    let output = bin()
        .arg("--watch")
        .arg(&tree)
        .arg("--dest")
        .arg(&dest)
        .arg("--min-idle-days")
        .arg("1")
        .arg("--min-free-gb")
        .arg("0")
        .arg("--once")
        .output()
        .expect("the binary should run");
    assert!(
        output.status.success(),
        "a valid --min-idle-days should run cleanly:\n{}",
        stderr(&output)
    );
    assert!(
        tree.join("recent.bin").exists() && !dest.join("recent.bin").exists(),
        "a 1-day idle gate must leave a file touched a second ago in place"
    );
    fs::remove_dir_all(&root).ok();
}
