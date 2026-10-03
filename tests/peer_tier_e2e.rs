//! End-to-end LAN-peer tier tests (#142): drive the real binary (`just_cache sweep`,
//! `restore`, `locate`) against a real `just_cache object-server` subprocess over
//! loopback standing in for the LAN peer.
//!
//! Unlike the object-tier e2e tests (#141), which use an in-repo fake S3, these tests
//! use the actual object-server binary (#155) as the far end — because that is the
//! whole point of #142: a peer runs `just_cache object-server`, and the `peer` driver
//! is the object-store client aimed at it. The peer root is a local directory, so
//! "two local roots over loopback" is exactly what the issue's acceptance criteria
//! call for.
//!
//! What these tests prove:
//!   (a) A sweep moves a file to a peer tier (a real object-server), records the
//!       catalog location, and removes the source (no symlink — the catalog is the
//!       source of truth).
//!   (b) A restore brings the file back, verified against the recorded digest.
//!   (c) An unreachable peer is a named refusal: the sweep fails, the source is kept,
//!       and nothing is adopted.
//!   (d) A refused credential (the server and the config disagree) is a named refusal
//!       that keeps the source.
//!   (e) A transfer interrupted by the peer going down is retried idempotently: the
//!       source survives the failed attempt, and a later sweep completes the move.
//!   (f) A file larger than one chunk (the 8 MiB upload chunk) moves to a peer via a
//!       single PUT — the object-server has no multipart endpoints, so the peer driver
//!       never uses the multipart path.
//!   (g) A `hours`-class peer tier refuses inline recall. That refusal is the same
//!       code path regardless of kind (see `tests/inline_recall.rs`), so it is covered
//!       there; this file pins that a peer tier parses with a slow recall class.
//!
//! What these tests CANNOT prove (and why):
//!   - **Resumable multipart uploads.** The peer driver deliberately uses single PUT
//!     (the object-server accepts single-object PUT/GET/HEAD/DELETE only), so there is
//!     no multipart/resume state to test — docs/design.md §9 records this as a gap.
//!   - **Real TLS transport.** Loopback HTTP via `insecure = true` skips the rustls
//!     path; server-side TLS is recorded as the follow-up peer-tier wiring work.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

const TEST_ENC_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BUCKET: &str = "peer-bucket";

/// nextest runs tests in this file in parallel within one process, and a child
/// process inherits the parent's environment at spawn time. A shared env var name
/// would let one test's `set_creds` corrupt another's children, so every test gets
/// its own uniquely-named variables.
fn unique_var(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{}_{}", prefix, COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn binary() -> Command {
    let cargo = std::env::var("CARGO_BIN_EXE_just_cache").unwrap_or_else(|_| {
        // Fallback for non-cargo-nextest runs
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        format!(
            "{}/target/{}/just_cache",
            env!("CARGO_MANIFEST_DIR"),
            profile
        )
    });
    Command::new(cargo)
}

/// Spawn a real `just_cache object-server` over loopback, standing in for the LAN peer.
/// The child is killed when the guard drops, even on test panic.
struct PeerServer {
    child: Child,
    port: u16,
    root: PathBuf,
}

impl Drop for PeerServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_peer_server(peer_root: &Path, creds_var: &str) -> PeerServer {
    let port = free_loopback_port();
    let child = spawn_peer_child(peer_root, port, creds_var);
    PeerServer {
        child,
        port,
        root: peer_root.to_path_buf(),
    }
}

/// Spawn a `just_cache object-server` on `port` and wait until it accepts
/// connections. Returns the child handle; used both by `start_peer_server` and to
/// bring a peer back on the same address after an interruption.
fn spawn_peer_child(peer_root: &Path, port: u16, creds_var: &str) -> Child {
    fs::create_dir_all(peer_root).unwrap();
    let child = binary()
        .args(["object-server"])
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--bucket")
        .arg(BUCKET)
        .arg("--root")
        .arg(peer_root)
        .arg("--insecure")
        .arg("--credential-source")
        .arg(format!("${creds_var}"))
        .spawn()
        .expect("failed to spawn object-server");

    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "object-server did not come up on port {port}");
    child
}

/// Pick a free loopback port by binding a listener, reading its port, and dropping
/// the listener. The race window is acceptable for tests.
fn free_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Write a `tiers.toml` with one `peer` tier pointing at `endpoint`. A peer tier has
/// the same S3-compatible fields as an `object` tier and no region (it defaults to
/// `peer`); `recall` is configurable because one test needs a slow recall class.
fn write_tiers(
    tiers_path: &Path,
    scratch_path: &Path,
    endpoint: &str,
    recall: &str,
    creds_var: &str,
    enc_var: &str,
) {
    let tiers = format!(
        "[tiers.peer]\n\
         kind = \"peer\"\n\
         path = \"{}\"\n\
         volatility = \"persistent\"\n\
         recall = \"{recall}\"\n\
         copies = 1\n\
         endpoint = \"{endpoint}\"\n\
         bucket = \"{BUCKET}\"\n\
         insecure = true\n\
         credential_source = \"${creds_var}\"\n\
         encryption_key = \"${enc_var}\"\n",
        scratch_path.display()
    );
    fs::write(tiers_path, tiers).unwrap();
}

/// Set this test's credential and envelope-key env vars (uniquely named) and return
/// their names.
fn set_creds(access_key: &str, secret_key: &str) -> (String, String) {
    let creds_var = unique_var("JC_TEST_CREDS");
    let enc_var = unique_var("JC_TEST_ENC_KEY");
    std::env::set_var(&creds_var, format!("{access_key}:{secret_key}"));
    std::env::set_var(&enc_var, TEST_ENC_KEY);
    (creds_var, enc_var)
}

/// Create a watch directory with one file, a catalog, and a tiers.toml pointing at a
/// peer endpoint. Returns (watch, tiers_path, catalog_path, temp_dir).
fn setup_e2e(
    endpoint: &str,
    recall: &str,
    creds_var: &str,
    enc_var: &str,
) -> (PathBuf, PathBuf, PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("watch");
    fs::create_dir_all(&watch).unwrap();

    fs::write(
        watch.join("data.bin"),
        b"this is cold storage data for the LAN-peer tier test\n",
    )
    .unwrap();

    let tiers_path = tmp.path().join("tiers.toml");
    let scratch = tmp.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    write_tiers(&tiers_path, &scratch, endpoint, recall, creds_var, enc_var);

    let catalog_path = watch.join(".just_cache-catalog.sqlite");
    let status = binary()
        .args(["catalog", "sync"])
        .arg("--watch")
        .arg(&watch)
        .arg("--catalog")
        .arg(&catalog_path)
        .arg("--dest")
        .arg(&scratch)
        .status()
        .expect("catalog sync failed");
    assert!(status.success(), "catalog sync failed");

    (watch, tiers_path, catalog_path, tmp)
}

fn run_sweep(watch: &Path, tiers_path: &Path, scratch: &Path) -> std::process::Output {
    binary()
        .arg("sweep")
        .arg("--watch")
        .arg(watch)
        .arg("--dest")
        .arg(scratch)
        .arg("--tiers")
        .arg(tiers_path)
        .arg("--min-idle-days")
        .arg("0")
        .arg("--limit")
        .arg("1")
        .arg("--once")
        .arg("--interval")
        .arg("0")
        .output()
        .expect("sweep failed")
}

// ---------- tests ---------------------------------------------------------------

/// (a) A sweep moves a file to a peer tier — a real object-server — records the
/// catalog location, and removes the source. The object lands on the peer's root
/// (encrypted; never the plaintext).
#[test]
fn sweep_moves_file_to_peer_tier_and_retires_source_after_verification() {
    let (creds_var, enc_var) = set_creds("peer-access-key", "peer-secret-key");
    let tmp = tempfile::tempdir().unwrap();
    let server = start_peer_server(&tmp.path().join("peer-root"), &creds_var);

    let endpoint = format!("127.0.0.1:{}", server.port);
    let (watch, tiers_path, catalog_path, _t) = setup_e2e(&endpoint, "ms", &creds_var, &enc_var);
    let scratch = _t.path().join("scratch");

    let output = run_sweep(&watch, &tiers_path, &scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "sweep should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The source is gone (removed after the verified upload; no symlink).
    let source = watch.join("data.bin");
    assert!(
        !source.exists(),
        "source should be removed after peer-tier sweep: {} still exists",
        source.display()
    );

    // The peer root holds exactly one stored object — encrypted, never the plaintext.
    let stored: Vec<PathBuf> = fs::read_dir(&server.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(stored.len(), 1, "the peer should hold one stored object");
    let bytes = fs::read(&stored[0]).unwrap();
    assert!(
        !bytes
            .windows(b"cold storage data".len())
            .any(|w| w == b"cold storage data"),
        "the stored bytes must be the envelope ciphertext, never the plaintext"
    );

    // The catalog records the location as offloaded on the peer tier.
    let locate_out = binary()
        .arg("locate")
        .arg("--catalog")
        .arg(&catalog_path)
        .arg("--tiers")
        .arg(&tiers_path)
        .arg("data.bin")
        .output()
        .expect("locate failed");
    let locate_stdout = String::from_utf8_lossy(&locate_out.stdout);
    assert!(
        locate_out.status.success(),
        "locate should succeed after peer-tier sweep: {locate_stdout}"
    );
    assert!(
        locate_stdout.contains("peer"),
        "locate should report the peer tier: {locate_stdout}"
    );
    assert!(
        locate_stdout.contains("offloaded"),
        "locate should report state offloaded: {locate_stdout}"
    );

    assert!(
        stdout.contains("1 moved"),
        "sweep should report 1 moved, got stdout: {stdout}"
    );
}

/// (b) A restore brings the file back from a peer tier: download, decrypt, verify
/// against the recorded digest, and place on disk with the original content.
#[test]
fn restore_brings_file_back_from_peer_tier() {
    let (creds_var, enc_var) = set_creds("peer-access-key", "peer-secret-key");
    let tmp = tempfile::tempdir().unwrap();
    let server = start_peer_server(&tmp.path().join("peer-root"), &creds_var);

    let endpoint = format!("127.0.0.1:{}", server.port);
    let (watch, tiers_path, catalog_path, _t) = setup_e2e(&endpoint, "ms", &creds_var, &enc_var);
    let scratch = _t.path().join("scratch");

    let sweep = run_sweep(&watch, &tiers_path, &scratch);
    assert!(
        sweep.status.success(),
        "sweep should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&sweep.stdout),
        String::from_utf8_lossy(&sweep.stderr)
    );

    let source = watch.join("data.bin");
    assert!(
        !source.exists(),
        "source should be offloaded before restore"
    );

    let restore = binary()
        .arg("restore")
        .arg("--catalog")
        .arg(&catalog_path)
        .arg("--tiers")
        .arg(&tiers_path)
        .arg("--dest")
        .arg(&scratch)
        .arg("--watch")
        .arg(&watch)
        .arg(&source)
        .output()
        .expect("restore failed");
    let restore_stdout = String::from_utf8_lossy(&restore.stdout);
    let restore_stderr = String::from_utf8_lossy(&restore.stderr);
    assert!(
        restore.status.success(),
        "restore should succeed\nstdout: {restore_stdout}\nstderr: {restore_stderr}"
    );

    assert!(
        source.is_file(),
        "restored file should exist at {}",
        source.display()
    );
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "this is cold storage data for the LAN-peer tier test\n",
        "restored file should have the original content"
    );
}

/// (c) An unreachable peer is a named failure: the sweep keeps the source, adopts
/// nothing, and leaves no half-moved file or orphan row behind.
#[test]
fn unreachable_peer_keeps_source_and_adopts_nothing() {
    let (creds_var, enc_var) = set_creds("peer-access-key", "peer-secret-key");

    // Point at a port nothing listens on (nothing is bound to port 1).
    let (watch, tiers_path, _catalog_path, _t) =
        setup_e2e("127.0.0.1:1", "ms", &creds_var, &enc_var);
    let scratch = _t.path().join("scratch");

    let output = run_sweep(&watch, &tiers_path, &scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a sweep against an unreachable peer should fail (exit non-zero): {stdout} {stderr}"
    );

    let source = watch.join("data.bin");
    assert!(
        !source.is_symlink(),
        "source should still be a regular file (not adopted)"
    );
    assert!(source.is_file(), "source file should still exist");
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "this is cold storage data for the LAN-peer tier test\n",
        "file content should be unchanged"
    );
    assert!(
        !stderr.is_empty(),
        "the refusal should be named on stderr, never silent: {stderr}"
    );
}

/// (d) A refused credential — the peer's object-server verifies a different access
/// key than the config signs with — fails the sweep and keeps the source.
#[test]
fn refused_credential_keeps_source() {
    let tmp = tempfile::tempdir().unwrap();
    // The server knows one credential pair...
    let (server_creds, _server_enc) = set_creds("server-access-key", "server-secret-key");
    let server = start_peer_server(&tmp.path().join("peer-root"), &server_creds);

    // ...but the config signs with a different one, so the server answers 403.
    let (wrong_creds, enc_var) = set_creds("wrong-access-key", "wrong-secret-key");
    let endpoint = format!("127.0.0.1:{}", server.port);
    let (watch, tiers_path, _catalog_path, _t) = setup_e2e(&endpoint, "ms", &wrong_creds, &enc_var);
    let scratch = _t.path().join("scratch");

    let output = run_sweep(&watch, &tiers_path, &scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a sweep with refused credentials should fail: {stdout} {stderr}"
    );

    let source = watch.join("data.bin");
    assert!(
        source.is_file(),
        "source should be kept when the peer refuses the credential"
    );
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        "this is cold storage data for the LAN-peer tier test\n",
        "file content should be unchanged"
    );
}

/// (e) A transfer interrupted by the peer going down is retried idempotently: the
/// first sweep fails and keeps the source, the peer comes back, and the next sweep
/// completes the move. Nothing is lost, only re-sent.
#[test]
fn transfer_resumed_after_interruption() {
    let (creds_var, enc_var) = set_creds("peer-access-key", "peer-secret-key");
    let tmp = tempfile::tempdir().unwrap();
    let server = start_peer_server(&tmp.path().join("peer-root"), &creds_var);
    let endpoint = format!("127.0.0.1:{}", server.port);
    let (watch, tiers_path, _catalog_path, _t) = setup_e2e(&endpoint, "ms", &creds_var, &enc_var);
    let scratch = _t.path().join("scratch");

    // Interrupt: kill the peer before the first sweep. The source must survive.
    let mut server = server;
    server.child.kill().unwrap();
    let _ = server.child.wait();

    let first = run_sweep(&watch, &tiers_path, &scratch);
    assert!(
        !first.status.success(),
        "a sweep against a downed peer should fail"
    );
    let source = watch.join("data.bin");
    assert!(
        source.is_file(),
        "the source must survive the interrupted transfer"
    );

    // The peer comes back on the same port; the next sweep completes the move.
    server.child = spawn_peer_child(&tmp.path().join("peer-root"), server.port, &creds_var);

    let second = run_sweep(&watch, &tiers_path, &scratch);
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second.status.success(),
        "the resumed sweep should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !source.exists(),
        "the source should be retired after the resumed sweep"
    );
}

/// (f) A file larger than one upload chunk (8 MiB) moves to a peer via a single PUT:
/// the object-server has no multipart endpoints, so the peer driver never splits the
/// upload. This exercises the peer single-PUT path at size.
#[test]
fn large_file_moves_to_peer_via_single_put() {
    let (creds_var, enc_var) = set_creds("peer-access-key", "peer-secret-key");
    let tmp = tempfile::tempdir().unwrap();
    let server = start_peer_server(&tmp.path().join("peer-root"), &creds_var);
    let endpoint = format!("127.0.0.1:{}", server.port);

    // A file bigger than the 8 MiB upload chunk, with recognizable content.
    let tmp2 = tempfile::tempdir().unwrap();
    let watch = tmp2.path().join("watch");
    fs::create_dir_all(&watch).unwrap();
    let big = vec![0xABu8; 9 * 1024 * 1024];
    fs::write(watch.join("big.bin"), &big).unwrap();

    let tiers_path = tmp2.path().join("tiers.toml");
    let scratch = tmp2.path().join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    write_tiers(&tiers_path, &scratch, &endpoint, "ms", &creds_var, &enc_var);

    let catalog_path = watch.join(".just_cache-catalog.sqlite");
    let sync = binary()
        .args(["catalog", "sync"])
        .arg("--watch")
        .arg(&watch)
        .arg("--catalog")
        .arg(&catalog_path)
        .arg("--dest")
        .arg(&scratch)
        .status()
        .expect("catalog sync failed");
    assert!(sync.success());

    let output = run_sweep(&watch, &tiers_path, &scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the large-file sweep should succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !watch.join("big.bin").exists(),
        "the large source should be retired after the sweep"
    );

    // The peer root holds one stored object, and its size is the envelope ciphertext
    // of a 9 MiB file (larger than the plaintext, so it was genuinely one PUT).
    let stored: Vec<PathBuf> = fs::read_dir(&server.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(stored.len(), 1, "the peer should hold one stored object");
    let len = fs::metadata(&stored[0]).unwrap().len();
    assert!(
        len > 9 * 1024 * 1024,
        "the stored ciphertext ({len} bytes) must be a single PUT of the whole file"
    );

    // And it restores to exactly the original bytes.
    let restore = binary()
        .arg("restore")
        .arg("--catalog")
        .arg(&catalog_path)
        .arg("--tiers")
        .arg(&tiers_path)
        .arg("--dest")
        .arg(&scratch)
        .arg("--watch")
        .arg(&watch)
        .arg(watch.join("big.bin"))
        .output()
        .expect("restore failed");
    assert!(
        restore.status.success(),
        "restore of the large file should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&restore.stdout),
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(fs::read(watch.join("big.bin")).unwrap(), big);
}

/// (g) A peer tier parses with a slow (`hours`) recall class — the inline-recall
/// refusal for that class is the same code path regardless of kind, exercised in
/// `tests/inline_recall.rs` (`a_slow_tier_refuses_inline_recall_and_names_restore`).
/// This test pins that a slow peer tier is a valid config.
#[test]
fn slow_peer_tier_parses_and_is_a_valid_destination() {
    let (creds_var, enc_var) = set_creds("peer-access-key", "peer-secret-key");
    let tmp = tempfile::tempdir().unwrap();
    let server = start_peer_server(&tmp.path().join("peer-root"), &creds_var);
    let endpoint = format!("127.0.0.1:{}", server.port);

    // `hours` recall on a peer tier: parse succeeds, and a sweep still moves a file
    // (restore, not inline recall, is how a slow tier's files come back).
    let (watch, tiers_path, _catalog_path, _t) =
        setup_e2e(&endpoint, "hours", &creds_var, &enc_var);
    let scratch = _t.path().join("scratch");
    let output = run_sweep(&watch, &tiers_path, &scratch);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "a sweep to a slow peer tier should still succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(!watch.join("data.bin").exists());
}

// ---------- helpers -------------------------------------------------------------
