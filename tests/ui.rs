//! The dashboard server (issue #170), driven through the real binary: `ui` refuses to
//! serve without a token, answers `401` without one (and never leaks the token into a log),
//! replays the read-only commands' own `--json` documents byte for byte, answers a missing
//! catalog with the command's refusal rather than a stack trace, and streams events
//! appended to the events file to a connected client.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use just_cache::catalog::{Catalog, CATALOG_NAME};

const TOKEN: &str = "ui-s3cret-token-160";
const BIN: &str = env!("CARGO_BIN_EXE_just_cache");

struct Tree {
    _dir: tempfile::TempDir,
    hot: PathBuf,
    cold: PathBuf,
    catalog: PathBuf,
}

/// One present file and one the mover already offloaded (a relative symlink into the cold
/// root), ingested by `catalog sync`.
fn build() -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot");
    let cold = dir.path().join("cold");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    std::os::unix::fs::symlink(
        std::path::Path::new("../../cold/shows/moved.mkv"),
        hot.join("shows/moved.mkv"),
    )
    .unwrap();
    fs::write(hot.join("shows/live.bin"), b"0123456789").unwrap();
    let catalog = hot.join(CATALOG_NAME);
    let output = Command::new(BIN)
        .args(["catalog", "sync", "--watch"])
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(Catalog::open(&catalog).is_ok());
    // The tier config the dashboard's tiers view reads (issue #171): written after the
    // sync, like a config that already sat beside the tree when the server started.
    let config = hot.join("tiers.toml");
    fs::write(
        &config,
        format!(
            "[tiers.cold]\n\
             kind = \"fs\"\n\
             path = \"{}\"\n\
             volatility = \"persistent\"\n\
             recall = \"s\"\n\
             copies = 1\n",
            cold.display()
        ),
    )
    .unwrap();
    Tree {
        _dir: dir,
        hot,
        cold,
        catalog,
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A `ui` child process on a loopback port, with its stderr collected so a test can check
/// what the server said (and what it never said).
struct UiServer {
    child: Child,
    addr: SocketAddr,
    stderr: Arc<Mutex<String>>,
}

impl UiServer {
    fn start(tree: &Tree, extra: &[String]) -> UiServer {
        let port = free_port();
        let token_path = tree.hot.join("ui.token");
        fs::write(&token_path, format!("{TOKEN}\n")).unwrap();
        let mut args: Vec<String> = vec![
            "ui".into(),
            "--bind".into(),
            format!("127.0.0.1:{port}"),
            "--watch".into(),
            tree.hot.display().to_string(),
            "--dest".into(),
            tree.cold.display().to_string(),
            "--token-file".into(),
            token_path.display().to_string(),
        ];
        args.extend(extra.iter().cloned());

        let mut child = Command::new(BIN)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let pipe = child.stderr.take().unwrap();
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let mut guard = sink.lock().unwrap();
                guard.push_str(line.trim_end());
                guard.push('\n');
                line.clear();
            }
        });

        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(addr).is_err() {
            assert!(
                Instant::now() < deadline,
                "the ui server never came up; stderr:\n{}",
                stderr.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        UiServer {
            child,
            addr,
            stderr,
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One request; returns `(status, body)`. The connection is closed by the server, so the
/// whole reply is read to end.
fn request(addr: SocketAddr, target: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut req = format!("GET {target} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    if let Some(token) = token {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header block");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        String::from_utf8_lossy(&raw[split + 4..]).to_string(),
    )
}

fn direct(args: &[&str]) -> String {
    let output = Command::new(BIN).args(args).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn events_path(tree: &Tree) -> PathBuf {
    tree.hot.join(".just_cache-events.jsonl")
}

#[test]
fn ui_without_a_token_refuses_to_serve() {
    let tree = build();
    let output = Command::new(BIN)
        .args(["ui", "--watch"])
        .arg(&tree.hot)
        .arg("--dest")
        .arg(&tree.cold)
        .env_remove("JUST_CACHE_UI_TOKEN")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("token"),
        "the refusal names the missing token: {stderr}"
    );
}

#[test]
fn unauthenticated_requests_answer_401_and_the_token_never_reaches_a_log() {
    let tree = build();
    let server = UiServer::start(&tree, &[]);

    let (status, body) = request(server.addr, "/api/audit", None);
    assert_eq!(status, 401, "no token means 401");
    assert!(
        !body.contains(TOKEN),
        "a 401 body never echoes the token: {body}"
    );

    let (status, _) = request(server.addr, "/api/audit", Some("wrong-token"));
    assert_eq!(status, 401, "a wrong token means 401");

    let (status, _) = request(server.addr, "/api/audit", Some(TOKEN));
    assert_eq!(status, 200, "the token unlocks the view");

    // The shell is the one public request: it is inert and must load before a person can
    // present a token to it.
    let (status, body) = request(server.addr, "/", None);
    assert_eq!(status, 200);
    assert!(body.contains("just_cache"), "the shell is served");

    // The access log names method, target and status — never a header, so never a token.
    let logged = server.stderr();
    assert!(
        logged.contains("GET /api/audit"),
        "access log written: {logged}"
    );
    assert!(
        !logged.contains(TOKEN),
        "the token never appears in the log:\n{logged}"
    );
}

#[test]
fn endpoints_replay_the_commands_json_byte_for_byte() {
    let tree = build();
    let server = UiServer::start(
        &tree,
        &["--catalog".into(), tree.catalog.display().to_string()],
    );

    let expected = direct(&[
        "audit",
        "--watch",
        &tree.hot.display().to_string(),
        "--dest",
        &tree.cold.display().to_string(),
        "--catalog",
        &tree.catalog.display().to_string(),
        "--json",
    ]);
    let (status, body) = request(server.addr, "/api/audit", Some(TOKEN));
    assert_eq!(status, 200);
    assert_eq!(
        body.trim(),
        expected,
        "audit replays the command's document"
    );

    let expected = direct(&[
        "locate",
        "shows/moved.mkv",
        "--catalog",
        &tree.catalog.display().to_string(),
        "--json",
    ]);
    let (status, body) = request(
        server.addr,
        "/api/locate?query=shows%2Fmoved.mkv",
        Some(TOKEN),
    );
    assert_eq!(status, 200);
    assert_eq!(
        body.trim(),
        expected,
        "locate replays the command's document"
    );

    let expected = direct(&[
        "volume",
        "list",
        "--catalog",
        &tree.catalog.display().to_string(),
        "--json",
    ]);
    let (status, body) = request(server.addr, "/api/volumes", Some(TOKEN));
    assert_eq!(status, 200);
    assert_eq!(body.trim(), expected, "volume list replays its document");

    // The catalog-only summary is `audit --no-filesystem`.
    let expected = direct(&[
        "audit",
        "--watch",
        &tree.hot.display().to_string(),
        "--dest",
        &tree.cold.display().to_string(),
        "--catalog",
        &tree.catalog.display().to_string(),
        "--no-filesystem",
        "--json",
    ]);
    let (status, body) = request(server.addr, "/api/catalog", Some(TOKEN));
    assert_eq!(status, 200);
    assert_eq!(
        body.trim(),
        expected,
        "the catalog summary replays its document"
    );

    // `pin --list` has no --json document, so its text is wrapped, never recomputed.
    let (status, body) = request(server.addr, "/api/pins", Some(TOKEN));
    assert_eq!(status, 200);
    assert!(
        body.starts_with("{\"text\":") && body.trim_end().ends_with('}'),
        "the text is wrapped in one JSON object: {body}"
    );
    assert!(
        body.contains("pins: "),
        "the command's own text is carried through: {body}"
    );
}

#[test]
fn a_missing_catalog_answers_with_the_commands_refusal_not_a_stack_trace() {
    let tree = build();
    let missing = tree.hot.join("nope.sqlite");
    let server = UiServer::start(&tree, &["--catalog".into(), missing.display().to_string()]);

    let (status, body) = request(
        server.addr,
        "/api/locate?query=shows%2Fmoved.mkv",
        Some(TOKEN),
    );
    assert_eq!(
        status, 200,
        "a refusal is an answer, not a transport failure"
    );
    assert!(
        body.contains("\"error\""),
        "the body says what went wrong: {body}"
    );
    assert!(
        body.to_lowercase().contains("catalog"),
        "the command's own message is carried through: {body}"
    );
    assert!(
        !body.contains("panicked") && !body.contains("RUST_BACKTRACE"),
        "no stack trace: {body}"
    );
}

#[test]
fn an_sse_client_reads_events_appended_live() {
    let tree = build();
    let server = UiServer::start(
        &tree,
        &["--catalog".into(), tree.catalog.display().to_string()],
    );

    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request =
        format!("GET /api/events HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    // Read the header block first; the stream has no end, so it cannot be read to end.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).unwrap();
        assert_ne!(read, 0, "the stream stayed open");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    assert!(head.starts_with("HTTP/1.1 200"), "stream opened: {head}");
    assert!(
        head.to_lowercase().contains("text/event-stream"),
        "the content type is a stream: {head}"
    );

    // Append one event to the file the server tails; a connected client must receive it.
    let marker = "{\"moved\":\"live-marker-160\"}";
    just_cache::events::append_to(&events_path(&tree), "sweep", marker, 1 << 20).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = String::new();
    while Instant::now() < deadline {
        let mut buffer = [0u8; 1024];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                seen.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if seen.contains(marker) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        seen.contains("data: ") && seen.contains(marker),
        "the appended event reached the connected client:\n{seen}"
    );
}

#[test]
fn the_surface_lists_the_routes_and_their_roots() {
    let tree = build();
    let server = UiServer::start(&tree, &[]);
    let (status, body) = request(server.addr, "/api/surface", Some(TOKEN));
    assert_eq!(status, 200);
    assert!(body.contains("\"name\":\"audit\""), "{body}");
    assert!(
        body.contains("\"available\":false"),
        "a route whose roots were not given is marked unavailable: {body}"
    );
    // The views this issue added are on the same terms: `pending` follows the catalog the
    // server was given, and `tiers` follows the tier config it was given.
    assert!(body.contains("\"name\":\"pending\""), "{body}");
    assert!(body.contains("\"name\":\"tiers\""), "{body}");
}

#[test]
fn the_tier_pending_and_schedule_views_replay_their_documents() {
    let tree = build();
    let config = tree.hot.join("tiers.toml");
    let server = UiServer::start(
        &tree,
        &[
            "--catalog".into(),
            tree.catalog.display().to_string(),
            "--tiers".into(),
            config.display().to_string(),
        ],
    );

    // `/api/tiers` replays `tiers --tiers <config> --catalog <catalog> --json`.
    let expected = direct(&[
        "tiers",
        "--tiers",
        &config.display().to_string(),
        "--catalog",
        &tree.catalog.display().to_string(),
        "--json",
    ]);
    let (status, body) = request(server.addr, "/api/tiers", Some(TOKEN));
    assert_eq!(status, 200);
    assert_eq!(
        body.trim(),
        expected,
        "tiers replays the command's document"
    );
    assert!(
        expected.contains("\"configured\":true"),
        "the fixture's config is served, not an empty state: {expected}"
    );
    assert!(
        expected.contains("\"locations\":1"),
        "the counts come from the catalog the server was given: {expected}"
    );

    // `/api/pending` replays `catalog pending --catalog <catalog> --json`.
    let expected = direct(&[
        "catalog",
        "pending",
        "--catalog",
        &tree.catalog.display().to_string(),
        "--json",
    ]);
    let (status, body) = request(server.addr, "/api/pending", Some(TOKEN));
    assert_eq!(status, 200);
    assert_eq!(
        body.trim(),
        expected,
        "pending replays the command's document"
    );

    // `/api/schedule` replays `schedule --catalog <catalog> --json` — now a document.
    let expected = direct(&[
        "schedule",
        "--catalog",
        &tree.catalog.display().to_string(),
        "--json",
    ]);
    let (status, body) = request(server.addr, "/api/schedule", Some(TOKEN));
    assert_eq!(status, 200);
    assert_eq!(
        body.trim(),
        expected,
        "schedule replays the command's document"
    );
    assert!(
        expected.contains("\"configured\":false") && expected.contains("\"passes\":[]"),
        "no schedule is the empty state, named the same way in both places: {expected}"
    );
}

#[test]
fn the_tiers_view_refuses_without_a_tier_config_like_the_command_would() {
    let tree = build();
    let server = UiServer::start(
        &tree,
        &["--catalog".into(), tree.catalog.display().to_string()],
    );

    let (status, body) = request(server.addr, "/api/tiers", Some(TOKEN));
    assert_eq!(
        status, 200,
        "a refusal is an answer, not a transport failure"
    );
    assert!(
        body.contains("\"error\"") && body.contains("--tiers"),
        "the refusal names the root the server was not given: {body}"
    );
    assert!(
        !body.contains("\"configured\""),
        "no document is invented in its place: {body}"
    );
}

#[test]
fn a_pass_running_against_the_server_reaches_an_attached_sse_client() {
    let tree = build();
    let server = UiServer::start(
        &tree,
        &["--catalog".into(), tree.catalog.display().to_string()],
    );

    // Attach before the pass starts, so the client is connected while the pass runs.
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /api/events HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).unwrap();
        assert_ne!(read, 0, "the stream stayed open");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    assert!(
        String::from_utf8_lossy(&head)
            .to_lowercase()
            .contains("text/event-stream"),
        "the client is attached: {}",
        String::from_utf8_lossy(&head)
    );

    // The pass: a real `catalog sync` against the tree the server serves, with a file
    // added so the pass has something to ingest. It appends its own report when done.
    fs::write(tree.hot.join("shows/late.bin"), b"late bytes").unwrap();
    let mut pass = Command::new(BIN)
        .args([
            "catalog",
            "sync",
            "--watch",
            &tree.hot.display().to_string(),
            "--dest",
            &tree.cold.display().to_string(),
            "--catalog",
            &tree.catalog.display().to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the pass starts");
    let status = pass.wait().unwrap();
    assert!(status.success(), "the pass ran clean");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = String::new();
    while Instant::now() < deadline {
        let mut buffer = [0u8; 1024];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                seen.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if seen.contains("\"pass\":\"catalog-sync\"") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        seen.contains("data: ") && seen.contains("\"pass\":\"catalog-sync\""),
        "the pass's event reached the client that was attached while it ran:\n{seen}"
    );
}

// ---- The views layer (issue #171): what the served page itself promises. ----

#[test]
fn the_served_shell_carries_the_views_their_empty_states_and_no_data() {
    let tree = build();
    let server = UiServer::start(&tree, &[]);

    let (status, body) = request(server.addr, "/", None);
    assert_eq!(status, 200, "the shell is the one public route");
    // The five views' scaffolding is in the served page.
    for marker in [
        "view-tiers",
        "view-activity",
        "view-audit",
        "view-schedule",
        "explain-panel",
    ] {
        assert!(body.contains(marker), "the shell names {marker}");
    }
    // The empty states are the page's own wording, named for what is missing.
    for wording in [
        "no tier map is configured",
        "no schedule is configured",
        "no maintenance events have arrived",
    ] {
        assert!(body.contains(wording), "the shell carries: {wording}");
    }
    // The shell is inert: no token, no catalog data.
    assert!(!body.contains(TOKEN), "the shell never carries the token");
    assert!(
        !body.contains("movie bytes") && !body.contains("moved.mkv"),
        "the shell carries no catalog data"
    );
}

#[test]
fn the_shell_references_only_routes_the_surface_serves() {
    let tree = build();
    let config = tree.hot.join("tiers.toml");
    let server = UiServer::start(
        &tree,
        &[
            "--catalog".into(),
            tree.catalog.display().to_string(),
            "--tiers".into(),
            config.display().to_string(),
        ],
    );
    let (_, surface_body) = request(server.addr, "/api/surface", Some(TOKEN));
    let surface: serde_json::Value = serde_json::from_str(surface_body.trim()).unwrap();
    let served: Vec<String> = surface["routes"]
        .as_array()
        .expect("the surface lists routes")
        .iter()
        .map(|route| route["path"].as_str().unwrap().to_string())
        .collect();

    let (_, shell) = request(server.addr, "/", None);
    // Every `/api/…` the page names must be a route the server actually serves, so no
    // view can fetch its way into a 404.
    let mut rest = shell.as_str();
    while let Some(at) = rest.find("\"/api/") {
        let after = &rest[at + 1..];
        let end = after.find('"').unwrap();
        let path = &after[..end];
        assert!(
            served.iter().any(|served_path| served_path == path)
                || path == "/api/surface"
                || path.starts_with("/api/explain?path="),
            "the shell names {path}, which the surface does not serve"
        );
        rest = &rest[at + end + 1..];
    }
    // And the routes the views render are among them.
    for path in ["/api/tiers", "/api/pending", "/api/schedule", "/api/audit"] {
        assert!(
            served.iter().any(|served_path| served_path == path),
            "the surface serves {path}"
        );
    }
}

/// The explain-on-click view renders `/api/explain`'s sections; this pins that the route
/// answers with the document carrying them, over HTTP, for a namespace path.
#[test]
fn the_explain_route_a_view_click_answers_with_the_four_gate_document() {
    let tree = build();
    let server = UiServer::start(
        &tree,
        &["--catalog".into(), tree.catalog.display().to_string()],
    );

    let (status, body) = request(
        server.addr,
        "/api/explain?path=shows%2Flive.bin",
        Some(TOKEN),
    );
    assert_eq!(status, 200);
    let document: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    for section in ["scope", "guards", "policy", "verdict", "catalog"] {
        assert!(
            document.get(section).is_some(),
            "the document carries {section}: {document}"
        );
    }
}
