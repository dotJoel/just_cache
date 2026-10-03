//! End-to-end behaviour of `tiers.toml` through the real binary.
//!
//! A tier config is a description of *where bytes live*, and the whole risk of a config is
//! that it silently does nothing: a file that was not read looks exactly like a file with
//! nothing to say. So these tests drive the binary and assert what a user sees — a tier is
//! named in the output, a volatile tier is refused outright, a malformed file names the
//! line it broke on, and with no file at all the tool behaves exactly as it did before
//! tiers existed (a destination root's path is its own tier name, and nothing is created
//! in the watched tree — invariant 9).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::CATALOG_NAME;

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

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn symlink(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

struct Tree {
    _tmp: tempfile::TempDir,
    watch: PathBuf,
    cold: PathBuf,
}

/// A tree with one hot file, one migrated file in `cold`, and a fresh catalog. The config
/// is deliberately *not* written: each test decides whether tiers exist.
fn synced_tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(watch.join("live.bin"), b"still hot").unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    symlink(
        Path::new("../../cold/shows/moved.mkv"),
        &watch.join("shows/moved.mkv"),
    );

    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .output()
        .expect("catalog sync runs");
    assert_eq!(code(&output), 0, "sync failed: {}", stderr(&output));
    Tree {
        _tmp: tmp,
        watch,
        cold,
    }
}

/// A config naming the watch root `ssd` (ms) and the cold root `hdd_parked` (s).
fn two_tier_config(tree: &Tree) -> String {
    format!(
        "[tiers.ssd]\n\
         kind = \"fs\"\n\
         path = \"{}\"\n\
         volatility = \"persistent\"\n\
         recall = \"ms\"\n\
         copies = 1\n\
         \n\
         [tiers.hdd_parked]\n\
         kind = \"fs\"\n\
         path = \"{}\"\n\
         volatility = \"persistent\"\n\
         recall = \"s\"\n\
         copies = 2\n",
        tree.watch.display(),
        tree.cold.display()
    )
}

fn locate(catalog: &Path, query: &str, tiers: Option<&Path>) -> Output {
    let mut command = bin();
    command.args(["locate", query, "--catalog"]).arg(catalog);
    if let Some(tiers) = tiers {
        command.arg("--tiers").arg(tiers);
    }
    command.output().expect("locate runs")
}

/// Two configured tiers are named in the output — one per copy — and `locate` states the
/// copy's recall class even though nothing acts on it yet.
#[test]
fn two_configured_tiers_are_named_and_carry_their_recall() {
    let tree = synced_tree();
    let catalog = tree.watch.join(CATALOG_NAME);
    let config = tree._tmp.path().join("tiers.toml");
    fs::write(&config, two_tier_config(&tree)).unwrap();

    // The hot copy lives on the `ssd` tier (the watch root), at ms recall.
    let hot = locate(&catalog, "live.bin", Some(&config));
    assert_eq!(code(&hot), 0, "stderr: {}", stderr(&hot));
    let text = stdout(&hot);
    assert!(
        text.contains("tier ssd, recall ms"),
        "the configured tier and its recall must be named: {text}"
    );

    // The offloaded copy lives on the `hdd_parked` tier, at s recall.
    let cold = locate(&catalog, "shows/moved.mkv", Some(&config));
    assert_eq!(code(&cold), 0, "stderr: {}", stderr(&cold));
    let text = stdout(&cold);
    assert!(
        text.contains("tier hdd_parked, recall s"),
        "the cold copy's tier and recall must be named: {text}"
    );

    // JSON carries the same answer for a machine: the configured name and the recall
    // class, so a consumer never has to join paths to names itself.
    let json = bin()
        .args(["locate", "live.bin", "--catalog"])
        .arg(&catalog)
        .arg("--tiers")
        .arg(&config)
        .arg("--json")
        .output()
        .unwrap();
    let body = stdout(&json);
    assert!(body.contains("\"tier_name\":\"ssd\""), "{body}");
    assert!(body.contains("\"recall\":\"ms\""), "{body}");
}

/// `audit` names the configured tiers in its output too — the key to the names its findings
/// use, printed once rather than repeated per finding.
#[test]
fn audit_names_the_configured_tiers() {
    let tree = synced_tree();
    let config = tree._tmp.path().join("tiers.toml");
    fs::write(&config, two_tier_config(&tree)).unwrap();

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&tree.watch)
        .arg("--dest")
        .arg(&tree.cold)
        .arg("--tiers")
        .arg(&config)
        .output()
        .expect("audit runs");
    let text = stdout(&output);
    assert!(text.contains("tiers: 2 configured"), "{text}");
    assert!(text.contains("ssd: kind=fs"), "{text}");
    assert!(text.contains("hdd_parked: kind=fs"), "{text}");
    assert!(text.contains("recall=s"), "{text}");
    assert!(
        text.contains("copies=2"),
        "the configured copy floor must be shown: {text}"
    );
}

/// A volatile tier is refused as a `--dest` (§2.1): a destination is a home, and a volatile
/// tier is a mirror. The refusal is a usage error and names the tier.
#[test]
fn a_volatile_tier_is_refused_as_a_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let ramdisk = tmp.path().join("ramdisk");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&ramdisk).unwrap();
    fs::write(watch.join("file.bin"), b"payload").unwrap();

    let config = tmp.path().join("tiers.toml");
    fs::write(
        &config,
        format!(
            "[tiers.ram]\n\
             kind = \"fs\"\n\
             path = \"{}\"\n\
             volatility = \"volatile\"\n\
             recall = \"us\"\n\
             copies = 1\n",
            ramdisk.display()
        ),
    )
    .unwrap();

    let output = bin()
        .args(["sweep", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&ramdisk)
        .arg("--tiers")
        .arg(&config)
        .arg("--once")
        .output()
        .expect("sweep runs");
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("volatile"), "{err}");
    assert!(err.contains("ram"), "the tier must be named: {err}");
    // Nothing moved: the file is still a real file, not a symlink into the cache.
    assert!(fs::symlink_metadata(watch.join("file.bin"))
        .unwrap()
        .is_file());
}

/// A volatile tier is refused as a policy target through `catalog sync` too — the same
/// destination conflation, caught before any catalog state is written.
#[test]
fn a_volatile_tier_is_refused_by_catalog_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let ramdisk = tmp.path().join("ramdisk");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&ramdisk).unwrap();

    let config = tmp.path().join("tiers.toml");
    fs::write(
        &config,
        format!(
            "[tiers.ram]\n\
             kind = \"fs\"\n\
             path = \"{}\"\n\
             volatility = \"volatile\"\n\
             recall = \"us\"\n\
             copies = 1\n",
            ramdisk.display()
        ),
    )
    .unwrap();

    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&ramdisk)
        .arg("--tiers")
        .arg(&config)
        .output()
        .expect("catalog sync runs");
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("volatile"), "{}", stderr(&output));
    // No catalog was created: the refusal happened before any state was written.
    assert!(!watch.join(CATALOG_NAME).exists());
}

/// A malformed config names the line. A syntax error and a bad enum value both point at the
/// line, because a config error you cannot locate is a config error you cannot fix.
#[test]
fn a_malformed_config_names_its_line() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let catalog = watch.join(CATALOG_NAME);
    // A catalog does not even have to exist: the config is read first, and it is broken.
    fs::create_dir_all(&watch).unwrap();

    // `copies =` is an unclosed value on line 6.
    let syntax = tmp.path().join("syntax.toml");
    fs::write(
        &syntax,
        "[tiers.ssd]\nkind = \"fs\"\npath = \"/mnt/ssd\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies =\n",
    )
    .unwrap();
    let output = locate(&catalog, "anything.bin", Some(&syntax));
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("line 6"), "{}", stderr(&output));

    // An impossible enum value is not a syntax error; it still names its line.
    let semantic = tmp.path().join("semantic.toml");
    fs::write(
        &semantic,
        "[tiers.ssd]\nkind = \"fs\"\npath = \"/mnt/ssd\"\nvolatility = \"sometimes\"\nrecall = \"ms\"\ncopies = 1\n",
    )
    .unwrap();
    let output = locate(&catalog, "anything.bin", Some(&semantic));
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("line 4"), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("volatility"),
        "{}",
        stderr(&output)
    );
}

/// An explicitly named `--tiers` file that does not exist is a usage error, never a silent
/// fallback to path-as-tier-name: the operator asked for that file.
#[test]
fn an_explicitly_named_missing_config_is_a_usage_error() {
    let tree = synced_tree();
    let catalog = tree.watch.join(CATALOG_NAME);
    let missing = tree._tmp.path().join("nope.toml");
    let output = locate(&catalog, "live.bin", Some(&missing));
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("does not exist"),
        "{}",
        stderr(&output)
    );
}

/// With no config file the tool behaves exactly as before: the destination root's path is
/// its own tier name, the recall is unknown (not zero), and no config is created in the
/// watched tree (invariant 9).
#[test]
fn with_no_config_the_path_is_the_tier_name_and_nothing_is_created() {
    let tree = synced_tree();
    let catalog = tree.watch.join(CATALOG_NAME);

    let output = locate(&catalog, "shows/moved.mkv", None);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    let cold = canonical(&tree.cold);
    assert!(
        text.contains(&format!("(tier {})", cold.display())),
        "the cold root's path must be its own tier name: {text}"
    );
    assert!(
        !text.contains("recall"),
        "with no config there is no recall class to state: {text}"
    );

    let json = bin()
        .args(["locate", "shows/moved.mkv", "--catalog"])
        .arg(&catalog)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        stdout(&json).contains("\"recall\":null"),
        "an unknown recall is null, not a class: {}",
        stdout(&json)
    );

    // A sweep with no config must not create `tiers.toml` beside the watch root.
    let sweep = bin()
        .args(["sweep", "--watch"])
        .arg(&tree.watch)
        .arg("--dest")
        .arg(&tree.cold)
        .arg("--once")
        .output()
        .expect("sweep runs");
    assert!(
        !tree.watch.join("tiers.toml").exists(),
        "a sweep must never create a tier config: {}",
        stderr(&sweep)
    );
}
