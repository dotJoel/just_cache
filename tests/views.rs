//! The read-only backend documents of the dashboard views (issue #171), driven through the
//! real binary: `tiers` prints the configuration and — when a catalog is present — only
//! the counts the catalog itself records (locations, damage marks, pending removals) and
//! the insert prompt an offline read would serve, verbatim; `catalog pending` lists the
//! releases a committed delete still owes an unlink; `schedule --json` prints the cadence
//! and next-run state of each pass. Nothing here computes policy: every number in every
//! document is one a table or a config file already held.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::CATALOG_NAME;

/// The envelope key for the offline tier, as 64 hex characters, handed over through the
/// environment exactly as a real operator's key would be (`encryption_key = "$VAR"`).
const KEY_ENV: &str = "JC_VIEWS_TEST_KEY";
const KEY_HEX: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0\
                       f0e1d2c3b4a5968778695a4b3c2d1e0f";

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_just_cache"));
    command.env(KEY_ENV, KEY_HEX);
    command
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

struct Fixture {
    _tmp: tempfile::TempDir,
    watch: PathBuf,
    fast: PathBuf,
    slow: PathBuf,
    vol: PathBuf,
    config: PathBuf,
    catalog: PathBuf,
}

/// A watched tree, two local tiers, and an offline tier whose mount is empty. The tree
/// carries one live file and one cold file per local tier, ingested by `catalog sync`.
/// Nothing is damaged, deleted or exported yet; each test decides that.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let fast = tmp.path().join("fast");
    let slow = tmp.path().join("slow");
    let vol = tmp.path().join("vol");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(fast.join("shows")).unwrap();
    fs::create_dir_all(slow.join("shows")).unwrap();
    fs::create_dir_all(&vol).unwrap();
    fs::write(fast.join("shows/fast.mkv"), b"fast movie").unwrap();
    fs::write(slow.join("shows/slow.mkv"), b"slow movie").unwrap();
    fs::write(watch.join("shows/live.mkv"), b"live movie").unwrap();
    // The mover's own shape: an offloaded name is a relative symlink into its tier.
    std::os::unix::fs::symlink(
        Path::new("../../fast/shows/fast.mkv"),
        watch.join("shows/fast.mkv"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        Path::new("../../slow/shows/slow.mkv"),
        watch.join("shows/slow.mkv"),
    )
    .unwrap();

    let config = tmp.path().join("tiers.toml");
    fs::write(
        &config,
        format!(
            "[tiers.fast]\n\
             kind = \"fs\"\n\
             path = \"{}\"\n\
             volatility = \"persistent\"\n\
             recall = \"ms\"\n\
             copies = 1\n\
             cost = \"$0.01\"\n\
             \n\
             [tiers.slow]\n\
             kind = \"fs\"\n\
             path = \"{}\"\n\
             volatility = \"persistent\"\n\
             recall = \"s\"\n\
             copies = 1\n\
             \n\
             [tiers.drawer]\n\
             kind = \"offline\"\n\
             path = \"{}\"\n\
             volatility = \"persistent\"\n\
             recall = \"hours\"\n\
             copies = 1\n\
             vaults = [\"shelf-a\", \"shelf-b\"]\n\
             encryption_key = \"${KEY_ENV}\"\n",
            fast.display(),
            slow.display(),
            vol.display()
        ),
    )
    .unwrap();

    let catalog = watch.join(CATALOG_NAME);
    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&fast)
        .arg("--dest")
        .arg(&slow)
        .arg("--tiers")
        .arg(&config)
        .output()
        .expect("catalog sync runs");
    assert_eq!(code(&sync), 0, "sync failed: {}", stderr(&sync));

    Fixture {
        _tmp: tmp,
        watch,
        fast,
        slow,
        vol,
        config,
        catalog,
    }
}

fn run(_fixture: &Fixture, args: &[&str]) -> Output {
    bin().args(args).output().expect("the command runs")
}

// The counts each live in the table that owns the fact: a scrub marks damage, a delete
// against an unmounted tier defers a release. Each test grows exactly one number so the
// assertion names the table it reads.

#[test]
fn tiers_reports_the_catalogs_recorded_counts_per_tier() {
    let fixture = fixture();

    // One damaged location on `fast`: a corrupted copy a scrub marked, with no verified
    // sibling to repair from.
    fs::write(fixture.fast.join("shows/fast.mkv"), b"rotten bytes").unwrap();
    let scrub = run(
        &fixture,
        &["scrub", "--catalog", fixture.catalog.to_str().unwrap()],
    );
    assert!(code(&scrub) != 0, "a scrub that found damage reports it");

    // One pending removal on `slow`: a delete whose copy lives on a tier that is not
    // mounted — here, a root the catalog records but whose directory is gone.
    fs::remove_dir_all(&fixture.slow).unwrap();
    let delete = run(
        &fixture,
        &[
            "catalog",
            "delete",
            "shows/slow.mkv",
            "--watch",
            fixture.watch.to_str().unwrap(),
            "--catalog",
            fixture.catalog.to_str().unwrap(),
        ],
    );
    // A deferred release is a finding, so the delete exits 1: the row is reported and
    // waits in `pending_removal`.
    assert_eq!(
        code(&delete),
        1,
        "the deferred release is reported: {}",
        stderr(&delete)
    );

    let json = run(
        &fixture,
        &[
            "tiers",
            "--tiers",
            fixture.config.to_str().unwrap(),
            "--catalog",
            fixture.catalog.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(code(&json), 0);
    let body = stdout(&json);
    assert!(
        body.contains("\"configured\":true"),
        "the config is reported as configured: {body}"
    );
    // `fast`: one recorded location, the damage the scrub marked, no pending release.
    assert!(
        body.contains("\"locations\":1,\"damaged\":1,\"pending_removals\":0"),
        "fast carries the catalog's own counts: {body}"
    );
    // `slow`: the delete released the name's last location (so the row is gone), and the
    // copy's release is deferred — recorded, not forgotten.
    assert!(
        body.contains("\"locations\":0,\"damaged\":0,\"pending_removals\":1"),
        "slow carries the catalog's own counts: {body}"
    );

    // The text form carries the same numbers, one line per tier.
    let text = run(
        &fixture,
        &[
            "tiers",
            "--tiers",
            fixture.config.to_str().unwrap(),
            "--catalog",
            fixture.catalog.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&text), 0);
    let body = stdout(&text);
    assert!(
        body.contains("locations=1 damaged=1 pending_removals=0"),
        "fast's line carries the same numbers: {body}"
    );
    // `slow`: the name's release removed its location, and the copy's release is deferred.
    assert!(
        body.contains("locations=0 damaged=0 pending_removals=1"),
        "slow's line carries the same numbers: {body}"
    );
}

#[test]
fn tiers_with_no_catalog_prints_no_counts_at_all() {
    let fixture = fixture();
    let json = run(
        &fixture,
        &[
            "tiers",
            "--tiers",
            fixture.config.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(code(&json), 0);
    let body = stdout(&json);
    assert!(
        !body.contains("\"locations\""),
        "no catalog, no counts — a number must be traceable to a document: {body}"
    );

    let text = run(
        &fixture,
        &["tiers", "--tiers", fixture.config.to_str().unwrap()],
    );
    assert_eq!(code(&text), 0);
    assert!(
        !stdout(&text).contains("locations="),
        "no catalog, no counts: {}",
        stdout(&text)
    );
}

#[test]
fn tiers_with_no_config_names_what_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest).unwrap();
    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&dest)
        .output()
        .expect("sync runs");
    assert_eq!(code(&sync), 0);

    // Run from a directory with no `tiers.toml` of its own, so the reported default is
    // the one beside the catalog.
    let text = bin()
        .args([
            "tiers",
            "--catalog",
            watch.join(CATALOG_NAME).to_str().unwrap(),
        ])
        .current_dir(tmp.path())
        .output()
        .expect("tiers runs");
    assert_eq!(code(&text), 0);
    let body = stdout(&text);
    assert!(
        body.contains("tiers.toml") && body.contains("no tier config"),
        "the empty state names the missing file: {body}"
    );
    let json = bin()
        .args(["tiers", "--json"])
        .current_dir(tmp.path())
        .output()
        .expect("tiers runs");
    let body = stdout(&json);
    assert!(
        body.contains("\"configured\":false") && body.contains("\"tiers\":[]"),
        "the JSON empty state names the missing file: {body}"
    );
}

#[test]
fn catalog_pending_lists_the_releases_a_delete_owes() {
    let fixture = fixture();
    fs::remove_dir_all(&fixture.slow).unwrap();
    let delete = run(
        &fixture,
        &[
            "catalog",
            "delete",
            "shows/slow.mkv",
            "--watch",
            fixture.watch.to_str().unwrap(),
            "--catalog",
            fixture.catalog.to_str().unwrap(),
        ],
    );
    // A deferred release is a finding: the delete reports it and exits 1, and the row
    // waits in `pending_removal`.
    assert_eq!(
        code(&delete),
        1,
        "the deferred release is reported: {}",
        stderr(&delete)
    );

    let text = run(
        &fixture,
        &[
            "catalog",
            "pending",
            "--catalog",
            fixture.catalog.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&text), 0);
    let body = stdout(&text);
    assert!(
        body.contains("pending_removal: 1 row(s)"),
        "the count is the row count: {body}"
    );
    assert!(
        body.contains(&format!("{}/shows/slow.mkv", fixture.slow.display())),
        "the path is the one the delete recorded: {body}"
    );
    assert!(
        body.contains("key shows/slow.mkv"),
        "the key is carried: {body}"
    );

    let json = run(
        &fixture,
        &[
            "catalog",
            "pending",
            "--catalog",
            fixture.catalog.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(code(&json), 0);
    let body = stdout(&json);
    assert!(
        body.contains("\"command\":\"pending\""),
        "the document is named: {body}"
    );
    assert!(
        body.contains(&format!(
            "\"path\":\"{}/shows/slow.mkv\"",
            fixture.slow.display()
        )),
        "the row's path: {body}"
    );
    assert!(
        body.contains("\"kind\":\"bytes\"")
            && body.contains("\"storage_key\":\"shows/slow.mkv\"")
            && body.contains("\"tier\":"),
        "the row's facts come straight from the table: {body}"
    );
    assert!(
        body.contains(&format!("\"tier\":\"{}\"", fixture.slow.display())),
        "the tier is the root the catalog recorded, verbatim: {body}"
    );
}

#[test]
fn catalog_pending_with_no_rows_is_an_empty_state_not_an_error() {
    let fixture = fixture();
    let text = run(
        &fixture,
        &[
            "catalog",
            "pending",
            "--catalog",
            fixture.catalog.to_str().unwrap(),
        ],
    );
    assert_eq!(code(&text), 0);
    assert!(
        stdout(&text).contains("no rows"),
        "the empty state says nothing is waiting: {}",
        stdout(&text)
    );
    let json = run(
        &fixture,
        &[
            "catalog",
            "pending",
            "--catalog",
            fixture.catalog.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(code(&json), 0);
    assert!(
        stdout(&json).contains("\"rows\":[]"),
        "the JSON empty state is an empty list: {}",
        stdout(&json)
    );
}

#[test]
fn tiers_serves_the_offline_insert_prompt_verbatim() {
    let fixture = fixture();
    let catalog = fixture.catalog.to_str().unwrap();

    let mount = bin()
        .args([
            "volume",
            "set",
            "drawer-01",
            "mounted",
            "--tier",
            "drawer",
            "--catalog",
        ])
        .arg(&fixture.catalog)
        .output()
        .expect("volume set runs");
    assert_eq!(code(&mount), 0, "volume set: {}", stderr(&mount));

    let sweep = bin()
        .args(["sweep", "--watch"])
        .arg(&fixture.watch)
        .arg("--dest")
        .arg(&fixture.vol)
        .arg("--tiers")
        .arg(&fixture.config)
        .arg("--min-idle-days")
        .arg("0")
        .arg("--once")
        .output()
        .expect("sweep runs");
    assert_eq!(code(&sweep), 0, "sweep: {}", stderr(&sweep));

    // The volume leaves the drive: a read of the exported object now names the prompt.
    let unmount = bin()
        .args(["volume", "set", "drawer-01", "in_vault", "--catalog"])
        .arg(&fixture.catalog)
        .output()
        .expect("volume set runs");
    assert_eq!(code(&unmount), 0, "volume set: {}", stderr(&unmount));
    let restore_path = fixture.watch.join("shows/live.mkv");
    let recall = bin()
        .args([
            "restore",
            restore_path.to_str().unwrap(),
            "--watch",
            fixture.watch.to_str().unwrap(),
            "--dest",
            fixture.vol.to_str().unwrap(),
            "--tiers",
            fixture.config.to_str().unwrap(),
            "--catalog",
        ])
        .arg(&fixture.catalog)
        .output()
        .expect("restore runs");
    assert!(
        code(&recall) != 0,
        "a read that cannot reach a volume refuses"
    );
    // The prompt is the read's own refusal — take the prompt itself out of the line the
    // read printed (its `just_cache: cannot restore ...` prefix is not the prompt), so
    // the tiers document is checked against what a read actually serves, byte for byte.
    let stderr_text = stderr(&recall);
    let line = stderr_text
        .lines()
        .find(|line| line.contains("insert volume"))
        .expect("the read names the insert prompt");
    let prompt = &line[line.find("insert volume").unwrap()..];
    assert!(!prompt.is_empty());

    let json = run(
        &fixture,
        &[
            "tiers",
            "--tiers",
            fixture.config.to_str().unwrap(),
            "--catalog",
            catalog,
            "--json",
        ],
    );
    assert_eq!(code(&json), 0);
    let body = stdout(&json);
    let quoted = format!("\"{prompt}\"");
    assert!(
        body.contains(&format!("\"insert_prompts\":[{quoted}]")),
        "the prompt is served verbatim, not as a generic error: {body}"
    );
    assert!(
        body.contains("\"locations\":1"),
        "the exported copy is the one location the catalog records on the tier: {body}"
    );

    let text = run(
        &fixture,
        &[
            "tiers",
            "--tiers",
            fixture.config.to_str().unwrap(),
            "--catalog",
            catalog,
        ],
    );
    assert_eq!(code(&text), 0);
    let body = stdout(&text);
    assert!(
        body.contains(prompt),
        "the text form carries the same prompt: {body}"
    );
}

#[test]
fn schedule_prints_the_next_runs_as_json() {
    let fixture = fixture();
    // schedule.toml sits beside the catalog by default; the catalog is beside the watch
    // root, so put it there.
    let schedule = fixture.watch.join("schedule.toml");
    fs::write(
        &schedule,
        "[scrub]\n\
         every = \"7d\"\n\
         rate = 4096\n",
    )
    .unwrap();

    let json = run(
        &fixture,
        &[
            "schedule",
            "--catalog",
            fixture.catalog.to_str().unwrap(),
            "--now",
            "2000000000",
            "--json",
        ],
    );
    assert_eq!(code(&json), 0);
    let body = stdout(&json);
    assert!(
        body.contains("\"command\":\"schedule\"") && body.contains("\"configured\":true"),
        "the document is named and configured: {body}"
    );
    assert!(
        body.contains("\"pass\":\"scrub\"")
            && body.contains("\"every\":\"7d\"")
            && body.contains("\"every_seconds\":604800")
            && body.contains("\"due\":true")
            && body.contains("\"last_run\":null")
            && body.contains("\"next_run\":\"2033-05-18T03:33:20Z\"")
            && body.contains("\"rate_kib_per_sec\":4096")
            && body.contains("\"min_free_bytes\":0")
            && body.contains("\"held_back_by\":[]"),
        "each fact is the config's or the state's, never invented: {body}"
    );
}

#[test]
fn schedule_with_no_config_prints_an_empty_document() {
    let fixture = fixture();
    let json = run(
        &fixture,
        &[
            "schedule",
            "--catalog",
            fixture.catalog.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(code(&json), 0);
    let body = stdout(&json);
    assert!(
        body.contains("\"configured\":false")
            && body.contains("\"config\":")
            && body.contains("\"passes\":[]"),
        "the JSON empty state names the missing config: {body}"
    );
}
