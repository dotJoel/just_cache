//! End-to-end behaviour of the offline-volume tier driver (#143) through the real binary.
//!
//! The whole point of an offline tier is that the filesystem cannot be asked where the
//! bytes are: they are on a disk a person inserts. So these tests drive the binary the way
//! an operator would — record a volume, sweep into it, read it back — and assert what the
//! user sees. The three claims that matter: a sweep exports to the mounted volume and
//! records it in the catalog; a read of the offloaded object comes back decrypt-verified;
//! and a read that cannot reach the volume names the tier, the volume and the mount point
//! rather than reporting a bare missing file (§3).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::CATALOG_NAME;

/// The envelope key for the test tiers, as 64 hex characters. It is never written to a
/// catalog row or a log line; it is only ever handed to a command through the environment,
/// exactly as a real operator's key would be (`encryption_key = "$VAR"`).
const KEY_ENV: &str = "JC_OFFLINE_TEST_KEY";
const KEY_HEX: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0\
                       f0e1d2c3b4a5968778695a4b3c2d1e0f";
const PLAINTEXT: &[u8] = b"the cold movie bytes, in plain text";

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
    vol: PathBuf,
    config: PathBuf,
    catalog: PathBuf,
}

/// A watched tree with one cold file, an empty volume mount, and a tier config naming the
/// mount as an `offline` tier. Nothing is synced or recorded yet; each test decides that.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let vol = tmp.path().join("vol");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(&vol).unwrap();
    fs::write(watch.join("shows/movie.mkv"), PLAINTEXT).unwrap();

    let config = tmp.path().join("tiers.toml");
    fs::write(
        &config,
        format!(
            "[tiers.drawer]\n\
             kind = \"offline\"\n\
             path = \"{}\"\n\
             volatility = \"persistent\"\n\
             recall = \"hours\"\n\
             copies = 1\n\
             vaults = [\"shelf-a\", \"shelf-b\"]\n\
             encryption_key = \"${KEY_ENV}\"\n",
            vol.display()
        ),
    )
    .unwrap();

    Fixture {
        _tmp: tmp,
        watch: watch.clone(),
        vol,
        config,
        catalog: watch.join(CATALOG_NAME),
    }
}

/// `catalog sync` the tree and record a volume as mounted in the tier — the two steps that
/// must precede a sweep into a volume.
fn sync_and_mount(fixture: &Fixture) -> Output {
    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&fixture.watch)
        .arg("--dest")
        .arg(&fixture.vol)
        .arg("--tiers")
        .arg(&fixture.config)
        .output()
        .expect("catalog sync runs");
    assert_eq!(code(&sync), 0, "sync failed: {}", stderr(&sync));

    bin()
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
        .expect("volume set runs")
}

/// Run a sweep into the volume.
fn sweep(fixture: &Fixture) -> Output {
    bin()
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
        .expect("sweep runs")
}

fn restore(fixture: &Fixture) -> Output {
    bin()
        .arg("restore")
        .arg(fixture.watch.join("shows/movie.mkv"))
        .arg("--watch")
        .arg(&fixture.watch)
        .arg("--dest")
        .arg(&fixture.vol)
        .arg("--tiers")
        .arg(&fixture.config)
        .arg("--catalog")
        .arg(&fixture.catalog)
        .output()
        .expect("restore runs")
}

/// The whole loop: sweep exports and records, locate names the tier, restore brings the
/// bytes back and verifies them against the recorded checksum.
#[test]
fn a_sweep_exports_to_the_volume_and_restore_brings_it_back() {
    let fixture = fixture();
    let mounted = sync_and_mount(&fixture);
    assert_eq!(code(&mounted), 0, "volume set failed: {}", stderr(&mounted));

    let swept = sweep(&fixture);
    assert_eq!(code(&swept), 0, "sweep failed: {}", stderr(&swept));

    // The hot file is retired, and the copy is on the volume — encrypted, never the
    // plaintext (rule 2: the volume leaves the host).
    assert!(
        !fixture.watch.join("shows/movie.mkv").exists(),
        "the source must be retired after a verified export"
    );
    let stored = fixture.vol.join("shows/movie.mkv");
    assert!(
        stored.is_file(),
        "the export must land at the mirrored path"
    );
    let blob = fs::read(&stored).unwrap();
    assert_ne!(
        blob, PLAINTEXT,
        "the volume holds ciphertext, not plaintext"
    );

    // The catalog names the tier of record.
    let located = bin()
        .args(["locate", "shows/movie.mkv", "--catalog"])
        .arg(&fixture.catalog)
        .arg("--tiers")
        .arg(&fixture.config)
        .output()
        .expect("locate runs");
    assert_eq!(code(&located), 0, "locate failed: {}", stderr(&located));
    assert!(
        stdout(&located).contains("tier drawer"),
        "locate must name the offline tier: {}",
        stdout(&located)
    );

    // Restore reads through the envelope, verifies the plaintext, and swaps it in.
    let restored = restore(&fixture);
    assert_eq!(code(&restored), 0, "restore failed: {}", stderr(&restored));
    assert_eq!(
        fs::read(fixture.watch.join("shows/movie.mkv")).unwrap(),
        PLAINTEXT,
        "restore must return the original bytes"
    );
}

/// A read that cannot reach the volume is the insert prompt (§3): it names the tier, the
/// volume and the mount point, and says how to record the volume — never a bare ENOENT.
#[test]
fn a_restore_without_the_volume_names_it() {
    let fixture = fixture();
    let mounted = sync_and_mount(&fixture);
    assert_eq!(code(&mounted), 0, "volume set failed: {}", stderr(&mounted));
    let swept = sweep(&fixture);
    assert_eq!(code(&swept), 0, "sweep failed: {}", stderr(&swept));

    // The volume leaves the drive: the ledger says so, and the mount is gone.
    let unmounted = bin()
        .args(["volume", "set", "drawer-01", "in_vault", "--catalog"])
        .arg(&fixture.catalog)
        .output()
        .expect("volume set runs");
    assert_eq!(code(&unmounted), 0, "{}", stderr(&unmounted));
    fs::remove_dir_all(&fixture.vol).unwrap();

    let refused = restore(&fixture);
    assert_eq!(code(&refused), 1, "a refused restore is a finding");
    let text = stderr(&refused);
    assert!(
        text.contains("insert volume `drawer-01`"),
        "insert prompt: {text}"
    );
    assert!(text.contains("drawer"), "the tier must be named: {text}");
    assert!(
        text.contains(&fixture.vol.display().to_string()),
        "the mount point must be named: {text}"
    );
    assert!(
        text.contains("shelf-a"),
        "where the volume lives must be named: {text}"
    );
    // Nothing was created at the hot path.
    assert!(!fixture.watch.join("shows/movie.mkv").exists());
}

/// A sweep with no volume recorded mounted refuses per file with the same prompt rather
/// than moving anything.
#[test]
fn a_sweep_without_a_mounted_volume_refuses_per_file() {
    let fixture = fixture();
    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&fixture.watch)
        .arg("--dest")
        .arg(&fixture.vol)
        .arg("--tiers")
        .arg(&fixture.config)
        .output()
        .expect("catalog sync runs");
    assert_eq!(code(&sync), 0, "sync failed: {}", stderr(&sync));

    let swept = sweep(&fixture);
    assert_eq!(code(&swept), 1, "a failed move is a nonzero exit");
    assert!(
        stderr(&swept).contains("has no volume recorded mounted"),
        "the refusal must say no volume is mounted: {}",
        stderr(&swept)
    );
    assert!(
        fixture.watch.join("shows/movie.mkv").is_file(),
        "nothing may move when no volume is mounted"
    );
}

/// The vault ledger through the CLI: record, list (text and JSON), refuse a typo and a
/// second mounted volume for one tier.
#[test]
fn the_volume_ledger_is_managed_through_the_cli() {
    let fixture = fixture();
    // A catalog is all `volume` needs; create it the way `catalog sync` would.
    just_cache::catalog::Catalog::open(&fixture.catalog).unwrap();

    let empty = bin()
        .args(["volume", "list", "--catalog"])
        .arg(&fixture.catalog)
        .arg("--json")
        .output()
        .expect("volume list runs");
    assert_eq!(code(&empty), 0, "{}", stderr(&empty));
    assert_eq!(stdout(&empty).trim(), "[]");

    let set = bin()
        .args([
            "volume",
            "set",
            "drawer-07",
            "mounted",
            "--tier",
            "drawer",
            "--note",
            "in the drive",
            "--catalog",
        ])
        .arg(&fixture.catalog)
        .output()
        .expect("volume set runs");
    assert_eq!(code(&set), 0, "{}", stderr(&set));
    assert!(
        stdout(&set).contains("drawer-07 state=mounted tier=drawer"),
        "{}",
        stdout(&set)
    );

    let listed = bin()
        .args(["volume", "list", "--catalog"])
        .arg(&fixture.catalog)
        .output()
        .expect("volume list runs");
    assert!(
        stdout(&listed).contains("drawer-07 state=mounted"),
        "{}",
        stdout(&listed)
    );

    let json = bin()
        .args(["volume", "list", "--catalog"])
        .arg(&fixture.catalog)
        .arg("--json")
        .output()
        .expect("volume list --json runs");
    let body = stdout(&json);
    assert!(body.contains("\"id\":\"drawer-07\""), "{body}");
    assert!(body.contains("\"state\":\"mounted\""), "{body}");
    assert!(body.contains("\"tier\":\"drawer\""), "{body}");

    // A typo in the state is a usage error that lists the states.
    let typo = bin()
        .args(["volume", "set", "drawer-07", "monted", "--catalog"])
        .arg(&fixture.catalog)
        .output()
        .expect("volume set runs");
    assert_eq!(code(&typo), 2, "a bad state is a usage error");
    assert!(
        stderr(&typo).contains("in_vault, mounted, loaned, lost"),
        "{}",
        stderr(&typo)
    );

    // Only one volume may be mounted per tier.
    let second = bin()
        .args([
            "volume",
            "set",
            "drawer-08",
            "mounted",
            "--tier",
            "drawer",
            "--catalog",
        ])
        .arg(&fixture.catalog)
        .output()
        .expect("volume set runs");
    assert_eq!(code(&second), 2, "a second mounted volume is a usage error");
    assert!(
        stderr(&second).contains("already recorded mounted"),
        "{}",
        stderr(&second)
    );
}

/// With the volume in a drawer, a sweep still runs (the mount point is not required to
/// exist for a configured offline tier) and refuses every candidate with the prompt. This
/// is the difference between a real `offline` tier and a bare directory: the destination
/// root's absence is the normal state, not a usage error.
#[test]
fn an_absent_offline_mount_is_not_a_usage_error() {
    let fixture = fixture();
    // Record the volume mounted, create the catalog, then take the mount away entirely.
    just_cache::catalog::Catalog::open(&fixture.catalog).unwrap();
    let set = bin()
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
    assert_eq!(code(&set), 0, "{}", stderr(&set));
    fs::remove_dir_all(&fixture.vol).unwrap();

    let swept = sweep(&fixture);
    assert_ne!(
        code(&swept),
        2,
        "an absent offline mount is not a usage error"
    );
    assert_eq!(code(&swept), 1, "the move fails, per file");
    assert!(
        stderr(&swept).contains("insert volume `drawer-01`"),
        "the insert prompt names the volume: {}",
        stderr(&swept)
    );
    let _ = Path::new(&fixture.vol);
}
