//! The gateway namespace provider (issue #47), exercised over real HTTP against a real
//! catalog: a listing and a range read of a present object, a restore request for an
//! offloaded one, and the refusals (no credential, wrong credential, a tier the
//! gateway was not configured with).

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use just_cache::catalog::{Catalog, CATALOG_NAME};
use just_cache::{Gateway, GatewayConfig};

const TOKEN: &str = "s3cret-token";

struct Tree {
    _dir: tempfile::TempDir,
    hot: PathBuf,
    cold: PathBuf,
    catalog: PathBuf,
}

/// One present file and one the mover already offloaded (a relative symlink into the
/// cold root), ingested by `catalog sync`.
fn build() -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot");
    let cold = dir.path().join("cold");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    std::os::unix::fs::symlink(
        Path::new("../../cold/shows/moved.mkv"),
        hot.join("shows/moved.mkv"),
    )
    .unwrap();
    fs::write(hot.join("shows/live.bin"), b"0123456789").unwrap();
    let catalog = hot.join(CATALOG_NAME);
    let output = Command::new(env!("CARGO_BIN_EXE_just_cache"))
        .args(["catalog", "sync", "--watch"])
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    Tree {
        _dir: dir,
        hot,
        cold,
        catalog,
    }
}

fn start(tree: &Tree, dests: Vec<PathBuf>) -> SocketAddr {
    let config = GatewayConfig {
        catalog_path: tree.catalog.clone(),
        watch: tree.hot.clone(),
        dests,
        token: TOKEN.to_string(),
        log: false,
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let gateway = Gateway::bind("127.0.0.1:0", config).expect("gateway binds");
        tx.send(gateway.local_addr().unwrap()).unwrap();
        gateway.serve().unwrap();
    });
    rx.recv().unwrap()
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

fn request(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut stream = TcpStream::connect(addr).unwrap();
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: test\r\n");
    for (key, value) in headers {
        text.push_str(&format!("{key}: {value}\r\n"));
    }
    text.push_str("\r\n");
    stream.write_all(text.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(raw[..split].to_vec()).unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        head,
        body: raw[split + 4..].to_vec(),
    }
}

const BEARER: (&str, &str) = ("Authorization", "Bearer s3cret-token");

/// Every row the catalog holds plus every byte in both roots — what "a read mutates
/// nothing" is checked against.
/// File path and, for a regular file, its bytes (`None` for a symlink).
type FileState = (PathBuf, Option<Vec<u8>>);

fn snapshot(tree: &Tree) -> (String, Vec<FileState>) {
    let catalog = Catalog::open(&tree.catalog).unwrap();
    let rows = format!(
        "{:?}{:?}{:?}",
        catalog.all_objects().unwrap(),
        catalog.all_names().unwrap(),
        catalog.all_locations().unwrap()
    );
    let mut files = Vec::new();
    for root in [&tree.hot, &tree.cold] {
        for entry in fs::read_dir(root.join("shows")).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = meta.is_file().then(|| fs::read(&path).unwrap());
            files.push((path, bytes));
        }
    }
    files.sort();
    (rows, files)
}

#[test]
fn listing_and_range_read_of_a_present_object_mutate_nothing() {
    let tree = build();
    let addr = start(&tree, vec![tree.cold.clone()]);
    let before = snapshot(&tree);

    let listing = request(addr, "PROPFIND", "/shows", &[BEARER, ("Depth", "1")]);
    assert_eq!(listing.status, 207, "{}", listing.head);
    let xml = String::from_utf8(listing.body).unwrap();
    assert!(xml.contains("<D:href>/shows/live.bin</D:href>"), "{xml}");
    assert!(
        xml.contains("<D:getcontentlength>10</D:getcontentlength>"),
        "{xml}"
    );
    assert!(xml.contains("<D:href>/shows/moved.mkv</D:href>"), "{xml}");
    assert!(xml.contains("<jc:state>offloaded</jc:state>"), "{xml}");
    assert!(xml.contains("<jc:state>present</jc:state>"), "{xml}");

    let range = request(
        addr,
        "GET",
        "/shows/live.bin",
        &[BEARER, ("Range", "bytes=2-5")],
    );
    assert_eq!(range.status, 206, "{}", range.head);
    assert_eq!(range.body, b"2345");
    assert!(
        range.head.contains("Content-Range: bytes 2-5/10"),
        "{}",
        range.head
    );

    let full = request(addr, "GET", "/shows/live.bin", &[BEARER]);
    assert_eq!(full.status, 200);
    assert_eq!(full.body, b"0123456789");

    // An offloaded object is not streamed: the consumer is told to restore it.
    let offloaded = request(addr, "GET", "/shows/moved.mkv", &[BEARER]);
    assert_eq!(offloaded.status, 409);

    // Nothing the gateway answered left a trace in the catalog or the tiers.
    assert_eq!(snapshot(&tree), before);

    // The gateway is not a write path.
    assert_eq!(
        request(addr, "PUT", "/shows/new.bin", &[BEARER]).status,
        405
    );
    assert_eq!(
        request(addr, "DELETE", "/shows/live.bin", &[BEARER]).status,
        405
    );
    assert_eq!(snapshot(&tree), before);
}

#[test]
fn a_restore_request_places_verified_bytes_for_an_offloaded_object() {
    let tree = build();
    let addr = start(&tree, vec![tree.cold.clone()]);
    let rows_before = snapshot(&tree).0;

    let restored = request(addr, "POST", "/shows/moved.mkv?restore", &[BEARER]);
    assert_eq!(
        restored.status,
        200,
        "{}",
        String::from_utf8_lossy(&restored.body)
    );
    let hot = tree.hot.join("shows/moved.mkv");
    assert!(fs::symlink_metadata(&hot).unwrap().is_file());
    assert_eq!(fs::read(&hot).unwrap(), b"movie bytes");
    // The cold copy stays: a gateway restore never drops a copy.
    assert_eq!(
        fs::read(tree.cold.join("shows/moved.mkv")).unwrap(),
        b"movie bytes"
    );

    // Now readable through the gateway, and a repeat request is idempotent.
    let read = request(addr, "GET", "/shows/moved.mkv", &[BEARER]);
    assert_eq!(read.status, 200);
    assert_eq!(read.body, b"movie bytes");
    let again = request(addr, "POST", "/shows/moved.mkv?restore", &[BEARER]);
    assert_eq!(again.status, 200);

    // The restore placed bytes; it wrote no catalog rows (the next sync records them).
    assert_eq!(snapshot(&tree).0, rows_before);
}

#[test]
fn a_restore_that_does_not_verify_places_nothing() {
    let tree = build();
    let addr = start(&tree, vec![tree.cold.clone()]);
    // Rot the cold copy: the restore must hash it against the recorded checksum and
    // refuse, leaving the symlink exactly as it was.
    fs::write(tree.cold.join("shows/moved.mkv"), b"movie bytez").unwrap();
    let refused = request(addr, "POST", "/shows/moved.mkv?restore", &[BEARER]);
    assert_eq!(refused.status, 409);
    assert!(fs::symlink_metadata(tree.hot.join("shows/moved.mkv"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn requests_without_the_token_are_refused() {
    let tree = build();
    let addr = start(&tree, vec![tree.cold.clone()]);
    let before = snapshot(&tree);

    let none = request(addr, "GET", "/shows/live.bin", &[]);
    assert_eq!(none.status, 401);
    assert!(none.head.contains("WWW-Authenticate: Basic"));
    assert!(!none.body.windows(4).any(|w| w == b"0123"));

    let wrong = request(
        addr,
        "PROPFIND",
        "/",
        &[("Authorization", "Bearer s3cret-tokem")],
    );
    assert_eq!(wrong.status, 401);
    // `user:wrong`
    let basic_wrong = request(
        addr,
        "GET",
        "/shows/live.bin",
        &[("Authorization", "Basic dXNlcjp3cm9uZw==")],
    );
    assert_eq!(basic_wrong.status, 401);
    let restore = request(addr, "POST", "/shows/moved.mkv?restore", &[]);
    assert_eq!(restore.status, 401);
    assert!(fs::symlink_metadata(tree.hot.join("shows/moved.mkv"))
        .unwrap()
        .file_type()
        .is_symlink());

    // HTTP Basic with the token as password (`any:s3cret-token`) is accepted, since
    // that is what most WebDAV clients send.
    let basic = request(
        addr,
        "GET",
        "/shows/live.bin",
        &[("Authorization", "Basic YW55OnMzY3JldC10b2tlbg==")],
    );
    assert_eq!(basic.status, 200);
    assert_eq!(snapshot(&tree), before);
}

#[test]
fn an_object_on_a_tier_the_gateway_was_not_given_is_refused() {
    let tree = build();
    // A second root the gateway *is* given, so the cold root the catalog recorded is a
    // tier outside its configuration.
    let other = tree._dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    let addr = start(&tree, vec![other]);

    let read = request(addr, "PROPFIND", "/shows/moved.mkv", &[BEARER]);
    assert_eq!(read.status, 403);
    let restore = request(addr, "POST", "/shows/moved.mkv?restore", &[BEARER]);
    assert_eq!(restore.status, 403);
    assert!(fs::symlink_metadata(tree.hot.join("shows/moved.mkv"))
        .unwrap()
        .file_type()
        .is_symlink());
    // The hot tier is always configured, so a present object still serves.
    assert_eq!(
        request(addr, "GET", "/shows/live.bin", &[BEARER]).status,
        200
    );
}

#[test]
fn the_gateway_refuses_to_start_with_an_empty_token() {
    let tree = build();
    let config = GatewayConfig {
        catalog_path: tree.catalog.clone(),
        watch: tree.hot.clone(),
        dests: vec![tree.cold.clone()],
        token: String::new(),
        log: false,
    };
    assert!(Gateway::bind("127.0.0.1:0", config).is_err());
}

#[test]
fn the_cli_never_prints_the_token() {
    let tree = build();
    let token_file = tree._dir.path().join("token");
    fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    // A missing catalog is a startup error; its message must name the file, not the
    // secret.
    let output = Command::new(env!("CARGO_BIN_EXE_just_cache"))
        .args(["gateway", "--listen", "127.0.0.1:0", "--watch"])
        .arg(&tree.hot)
        .arg("--dest")
        .arg(&tree.cold)
        .arg("--catalog")
        .arg(tree._dir.path().join("missing.sqlite"))
        .arg("--token-file")
        .arg(&token_file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!all.contains(TOKEN), "{all}");
}
