//! The dashboard server (issue #170): `just_cache ui` serves a static frontend embedded in
//! the binary, JSON views over the read-only commands' own `--json` documents, and a
//! server-sent stream of the maintenance events file (#169).
//!
//! # The exposed surface is the policy
//!
//! This is the MCP rule (#137), applied to a browser. The routes are a fixed, *read-only*
//! set chosen at compile time — there is no route that moves, repairs, deletes, pins or
//! clears anything, and no way for a request to widen its own scope: the roots are chosen
//! once at invocation (exactly like `mcp`), and a request names a *query* (a namespace
//! path, a digest, a catalog), never a root. A route whose roots were not configured
//! answers with a plain refusal rather than guessing.
//!
//! # Views are thin over the commands' own documents
//!
//! Every JSON route re-runs this same binary as the equivalent read-only command with
//! `--json` and returns what it printed, byte for byte — the `mcp` approach, for the same
//! reason: an answer a browser shows is the answer a shell shows, with nothing in between
//! that could drift from it. A command that refuses (a catalog that is not there, roots
//! that were not given) answers with its own message, never a stack trace.
//!
//! # Deliberately small
//!
//! The HTTP layer is hand-written over `std::net`, extending the approach `gateway` and
//! `object-server` already take, so `ui` adds no web-framework dependency and nothing
//! outside this module links it. Unlike the gateway's strictly sequential accept loop, a
//! `ui` connection is served on its own thread: the SSE stream (#169) holds a connection
//! open for the life of the viewer, while every other route answers and closes. A thread
//! per viewer is the smallest thing that lets one viewer stream while another polls.
//!
//! # The token
//!
//! Read from a file (`--token-file`) or the environment (`JUST_CACHE_UI_TOKEN`), never
//! from the command line, where it would show up in `ps` and shell history. It is
//! compared in constant time and never logged: an access-log line names the method, the
//! target and the status, and never a header, so never a credential. An empty token is
//! refused — an endpoint that accepts an empty credential is not authenticated.
//!
//! # The static shell is public, the data is not
//!
//! `GET /` serves the embedded shell without a token. The shell is inert — HTML, CSS and
//! JavaScript with no catalog data in it — and it has to load *before* a person can
//! present a token to the page, so it is the one request that cannot demand one. Every
//! route that answers from a catalog or the events file, including the stream, requires
//! the bearer token and answers `401` without it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::events;

/// The frontend: one self-contained page, embedded in the binary so the cargo-only build
/// stays true — no node, no build step, no framework, no release artifact.
const INDEX_HTML: &str = include_str!("ui/index.html");

/// Requests larger than this (request line plus headers) are refused. A `GET` has no body,
/// so a header block this large is an attack or a broken client, not a request.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// A connection that stops sending is dropped rather than holding a worker thread forever.
/// The SSE stream resets the write timer with a keep-alive comment well inside this.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the stream checks the events file for new lines.
const SSE_POLL: Duration = Duration::from_millis(200);

/// How often an idle stream sends a comment line, so a proxy does not drop it and the
/// browser's connection stays warm.
const SSE_PING: Duration = Duration::from_secs(15);

/// Environment variable the token may come from, the sibling of the gateway's.
pub const TOKEN_ENV: &str = "JUST_CACHE_UI_TOKEN";

/// Everything the `ui` server needs. The roots are fixed at invocation; a request never
/// supplies one.
#[derive(Clone)]
pub struct UiConfig {
    /// The catalog to answer from, when one was named or defaulted.
    pub catalog: Option<PathBuf>,
    /// The watched tree, when one was named.
    pub watch: Option<PathBuf>,
    /// The cold-tier roots the operator chose to expose, fastest first.
    pub dest: Vec<PathBuf>,
    /// Tier configuration, passed through to the commands that accept it.
    pub tiers: Option<PathBuf>,
    /// Lifecycle policy, passed through to `explain`.
    pub policy: Option<PathBuf>,
    /// The shared secret every request must present. Never logged.
    pub token: String,
    /// This same binary, re-run to produce each route's document.
    pub program: PathBuf,
    /// Print one access-log line per request to stderr (method, target, status).
    pub log: bool,
}

// Hand-written so a `{:?}` of the config in an error path can never print the token.
impl std::fmt::Debug for UiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiConfig")
            .field("catalog", &self.catalog)
            .field("watch", &self.watch)
            .field("dest", &self.dest)
            .field("tiers", &self.tiers)
            .field("policy", &self.policy)
            .field("token", &"<redacted>")
            .field("program", &self.program)
            .field("log", &self.log)
            .finish()
    }
}

impl UiConfig {
    /// Where the events file the stream tails lives: beside the catalog when one was named,
    /// else beside the watch root — the same resolution `events::append` uses for a pass, so
    /// the server watches exactly the file the passes write.
    pub fn events_path(&self) -> PathBuf {
        let watch = self.watch.clone().unwrap_or_else(|| PathBuf::from("."));
        events::events_path(self.catalog.as_deref(), &watch)
    }
}

/// Why a `ui` server could not start.
#[derive(Debug)]
pub enum UiError {
    /// The token is empty: an endpoint that accepts an empty credential is not authenticated.
    EmptyToken,
    Bind(io::Error),
}

impl std::fmt::Display for UiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiError::EmptyToken => write!(
                f,
                "the ui token is empty; refusing to serve an endpoint anyone can read"
            ),
            UiError::Bind(err) => write!(f, "cannot listen: {err}"),
        }
    }
}

impl std::error::Error for UiError {}

/// A bound, not-yet-serving dashboard server.
pub struct UiServer {
    listener: TcpListener,
    config: Arc<UiConfig>,
}

impl UiServer {
    /// Check the configuration and bind. Nothing is served until [`UiServer::serve`].
    pub fn bind(addr: &str, config: UiConfig) -> Result<Self, UiError> {
        if config.token.trim().is_empty() {
            return Err(UiError::EmptyToken);
        }
        let listener = TcpListener::bind(addr).map_err(UiError::Bind)?;
        Ok(UiServer {
            listener,
            config: Arc::new(config),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections forever, one worker thread each, so a streaming viewer never
    /// blocks a polling one.
    pub fn serve(&self) -> io::Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    let config = Arc::clone(&self.config);
                    thread::spawn(move || handle_connection(stream, config));
                }
                Err(err) => {
                    if self.config.log {
                        eprintln!("just_cache ui: accept failed: {err}");
                    }
                }
            }
        }
        Ok(())
    }
}

/// One parsed request line plus the headers the server reads (only `Authorization`).
struct Request {
    method: String,
    target: String,
    authorization: Option<String>,
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || target.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }
    let mut authorization = None;
    let mut consumed = line.len();
    loop {
        let mut header = String::new();
        let read = reader.read_line(&mut header)?;
        if read == 0 {
            break;
        }
        consumed += read;
        if consumed > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header block too large",
            ));
        }
        let trimmed = header.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_string());
            }
        }
    }
    Ok(Request {
        method,
        target,
        authorization,
    })
}

/// The presented bearer token, if the request carried one.
fn presented_token(request: &Request) -> Option<&str> {
    request
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
}

/// Constant-time comparison: a wrong token never leaks its shared prefix through timing.
fn token_matches(expected: &str, presented: &str) -> bool {
    let a = expected.as_bytes();
    let b = presented.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn authorized(config: &UiConfig, request: &Request) -> bool {
    match presented_token(request) {
        Some(presented) => token_matches(&config.token, presented),
        None => false,
    }
}

/// A parsed request target: the path and its decoded query parameters.
struct Target {
    path: String,
    query: BTreeMap<String, String>,
}

fn parse_target(raw: &str) -> Target {
    let (path, query) = match raw.split_once('?') {
        Some((path, query)) => (path, query),
        None => (raw, ""),
    };
    let mut params = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        params.insert(percent_decode(key), percent_decode(value));
    }
    Target {
        path: percent_decode(path),
        query: params,
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
            {
                (Some(high), Some(low)) => {
                    out.push((high << 4) | low);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// An HTTP response, hand-built like the gateway's.
struct Response {
    status: u16,
    reason: &'static str,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Response {
    fn json(body: String) -> Self {
        Response {
            status: 200,
            reason: "OK",
            content_type: "application/json; charset=utf-8".into(),
            body: body.into_bytes(),
            headers: Vec::new(),
        }
    }

    fn html(body: &str) -> Self {
        Response {
            status: 200,
            reason: "OK",
            content_type: "text/html; charset=utf-8".into(),
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    fn text(status: u16, reason: &'static str, body: &str) -> Self {
        Response {
            status,
            reason,
            content_type: "text/plain; charset=utf-8".into(),
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    fn unauthorized() -> Self {
        let mut response = Response::text(401, "Unauthorized", "authentication required\n");
        response.headers.push((
            "WWW-Authenticate".into(),
            "Bearer realm=\"just_cache\"".into(),
        ));
        response
    }

    fn write_to(&self, stream: &mut TcpStream) -> io::Result<()> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        for (key, value) in &self.headers {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(&self.body)?;
        stream.flush()
    }
}

/// Serve one connection: read the request, authenticate, route, and (for the stream) keep
/// writing until the client goes away.
fn handle_connection(mut stream: TcpStream, config: Arc<UiConfig>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => {
            log_access(&config, "-", "-", 400);
            let _ = Response::text(400, "Bad Request", "malformed request\n").write_to(&mut stream);
            return;
        }
    };

    let target = parse_target(&request.target);

    // The shell is public and inert; every data route, including the stream, is not.
    if target.path != "/" && !authorized(&config, &request) {
        log_access(&config, &request.method, &request.target, 401);
        let _ = Response::unauthorized().write_to(&mut stream);
        return;
    }

    if target.path == "/api/events" {
        log_access(&config, &request.method, &request.target, 200);
        let _ = serve_events(stream, &config);
        return;
    }

    let response = route(&config, &request.method, &target);
    log_access(&config, &request.method, &request.target, response.status);
    let _ = response.write_to(&mut stream);
}

fn log_access(config: &UiConfig, method: &str, target: &str, status: u16) {
    if config.log {
        // Method, target and status only: never a header, so never a credential.
        eprintln!("just_cache ui: {method} {target} {status}");
    }
}

/// The fixed route table. An unknown path is `404`; a non-GET is `405`.
fn route(config: &UiConfig, method: &str, target: &Target) -> Response {
    if method != "GET" && method != "HEAD" {
        return Response::text(405, "Method Not Allowed", "only GET is served\n");
    }
    match target.path.as_str() {
        "/" => Response::html(INDEX_HTML),
        "/favicon.ico" => Response::text(204, "No Content", ""),
        "/api/surface" => Response::json(surface_json(config)),
        "/api/audit" => Response::json(json_or_error("audit", audit_document(config, false))),
        "/api/catalog" => Response::json(json_or_error("catalog", audit_document(config, true))),
        "/api/explain" => match target.query.get("path") {
            Some(path) => Response::json(json_or_error("explain", explain_document(config, path))),
            None => Response::json(error_json(
                "explain",
                "the request needs ?path=<namespace path>",
            )),
        },
        "/api/locate" => match target.query.get("query") {
            Some(query) => Response::json(json_or_error("locate", locate_document(config, query))),
            None => Response::json(error_json(
                "locate",
                "the request needs ?query=<namespace path or digest>",
            )),
        },
        "/api/pins" => Response::json(json_or_error("pins", pins_document(config))),
        "/api/volumes" => Response::json(json_or_error("volumes", volumes_document(config))),
        "/api/schedule" => Response::json(json_or_error("schedule", schedule_document(config))),
        _ => Response::text(404, "Not Found", "not found\n"),
    }
}

/// A JSON route's answer: the command's own document, or a refusal in the same shape.
///
/// Both are `200`: the route answered, and the body says what happened. A view checks for
/// an `error` field rather than a transport failure. Keeping the produced document *byte
/// for byte* is the point — a request here and the same command in a shell print the same
/// bytes.
fn json_or_error(command: &str, result: Result<String, String>) -> String {
    match result {
        Ok(document) => document,
        Err(message) => error_json(command, &message),
    }
}

fn error_json(command: &str, message: &str) -> String {
    format!(
        "{{\"command\":{},\"error\":{}}}",
        events::json_string(command),
        events::json_string(message)
    )
}

/// The routes, their availability, and the roots they need — the server describing its own
/// fixed surface, so a view can render only what this invocation can answer.
fn surface_json(config: &UiConfig) -> String {
    let has_watch = config.watch.is_some() && !config.dest.is_empty();
    let has_catalog = config.catalog.is_some();
    let entries = [
        ("audit", "/api/audit", has_watch, "watch, dest"),
        (
            "catalog",
            "/api/catalog",
            has_watch && has_catalog,
            "watch, dest, catalog",
        ),
        ("explain", "/api/explain", has_watch, "watch, dest"),
        ("locate", "/api/locate", has_catalog, "catalog"),
        ("pins", "/api/pins", has_catalog, "catalog"),
        ("volumes", "/api/volumes", has_catalog, "catalog"),
        ("schedule", "/api/schedule", has_catalog, "catalog"),
        ("events", "/api/events", true, "catalog or watch"),
    ];
    let mut out = String::from("{\"routes\":[");
    for (index, (name, path, available, needs)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":{},\"path\":{},\"available\":{},\"needs\":{}}}",
            events::json_string(name),
            events::json_string(path),
            available,
            events::json_string(needs)
        ));
    }
    out.push_str("]}");
    out
}

/// The args to re-run `audit`, shared by the walk audit and the catalog-only summary.
///
/// `--no-filesystem` is the catalog-only mode: it reads the recorded state, the per-tier
/// floors, the damage marks and the scrub summary without walking or touching a tier.
fn audit_args(config: &UiConfig, catalog_only: bool) -> Result<Vec<OsString>, String> {
    let watch = config
        .watch
        .as_ref()
        .ok_or("this server was not started with --watch and --dest")?;
    if config.dest.is_empty() {
        return Err("this server was not started with --watch and --dest".into());
    }
    let mut args = vec![
        "audit".into(),
        "--watch".into(),
        watch.clone().into_os_string(),
    ];
    for dest in &config.dest {
        args.push("--dest".into());
        args.push(dest.clone().into_os_string());
    }
    if catalog_only {
        let catalog = config
            .catalog
            .as_ref()
            .ok_or("this server was not started with --catalog")?;
        args.push("--catalog".into());
        args.push(catalog.clone().into_os_string());
        args.push("--no-filesystem".into());
    } else if let Some(catalog) = &config.catalog {
        args.push("--catalog".into());
        args.push(catalog.clone().into_os_string());
    }
    if let Some(tiers) = &config.tiers {
        args.push("--tiers".into());
        args.push(tiers.clone().into_os_string());
    }
    args.push("--json".into());
    Ok(args)
}

fn audit_document(config: &UiConfig, catalog_only: bool) -> Result<String, String> {
    replay_document(config, audit_args(config, catalog_only)?)
}

fn explain_document(config: &UiConfig, path: &str) -> Result<String, String> {
    let watch = config
        .watch
        .as_ref()
        .ok_or("this server was not started with --watch and --dest")?;
    if config.dest.is_empty() {
        return Err("this server was not started with --watch and --dest".into());
    }
    let mut args = vec![
        "explain".into(),
        OsString::from(path),
        "--watch".into(),
        watch.clone().into_os_string(),
    ];
    for dest in &config.dest {
        args.push("--dest".into());
        args.push(dest.clone().into_os_string());
    }
    if let Some(tiers) = &config.tiers {
        args.push("--tiers".into());
        args.push(tiers.clone().into_os_string());
    }
    if let Some(policy) = &config.policy {
        args.push("--policy".into());
        args.push(policy.clone().into_os_string());
    }
    if let Some(catalog) = &config.catalog {
        args.push("--catalog".into());
        args.push(catalog.clone().into_os_string());
    }
    args.push("--json".into());
    replay_document(config, args)
}

fn locate_document(config: &UiConfig, query: &str) -> Result<String, String> {
    let catalog = config
        .catalog
        .as_ref()
        .ok_or("this server was not started with --catalog")?;
    let mut args = vec![
        "locate".into(),
        OsString::from(query),
        "--catalog".into(),
        catalog.clone().into_os_string(),
    ];
    if let Some(tiers) = &config.tiers {
        args.push("--tiers".into());
        args.push(tiers.clone().into_os_string());
    }
    args.push("--json".into());
    replay_document(config, args)
}

/// `pin --list` has no `--json` document, so its text is wrapped in one JSON string field.
/// Nothing is computed: the text is the command's own output, byte for byte, inside
/// `{"text": ...}`.
fn pins_document(config: &UiConfig) -> Result<String, String> {
    let catalog = config
        .catalog
        .as_ref()
        .ok_or("this server was not started with --catalog")?;
    let args = vec![
        "pin".into(),
        "--list".into(),
        "--catalog".into(),
        catalog.clone().into_os_string(),
    ];
    replay_text(config, args)
}

fn volumes_document(config: &UiConfig) -> Result<String, String> {
    let catalog = config
        .catalog
        .as_ref()
        .ok_or("this server was not started with --catalog")?;
    let mut args = vec![
        "volume".into(),
        "list".into(),
        "--catalog".into(),
        catalog.clone().into_os_string(),
    ];
    if let Some(watch) = &config.watch {
        args.push("--watch".into());
        args.push(watch.clone().into_os_string());
    }
    args.push("--json".into());
    replay_document(config, args)
}

/// `schedule` has no `--json` document either; like `pins`, its text is wrapped.
fn schedule_document(config: &UiConfig) -> Result<String, String> {
    let catalog = config
        .catalog
        .as_ref()
        .ok_or("this server was not started with --catalog")?;
    let args = vec![
        "schedule".into(),
        "--catalog".into(),
        catalog.clone().into_os_string(),
    ];
    replay_text(config, args)
}

/// Run this binary and return what it printed to stdout, verbatim.
///
/// Empty stdout means the command refused and said why on stderr; that message is returned
/// as the error, so the route answers with the command's own refusal.
fn replay_document(config: &UiConfig, args: Vec<OsString>) -> Result<String, String> {
    let output = Command::new(&config.program)
        .args(&args)
        .output()
        .map_err(|err| format!("could not run just_cache: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            "the command produced no output".into()
        } else {
            stderr
        })
    } else {
        Ok(stdout)
    }
}

/// Run a text-only command and wrap its stdout as `{"text": ...}`.
fn replay_text(config: &UiConfig, args: Vec<OsString>) -> Result<String, String> {
    let stdout = replay_document(config, args)?;
    Ok(format!("{{\"text\":{}}}", events::json_string(&stdout)))
}

/// A reader over the rotating events file, positioned to stream only what is new.
///
/// The writer keeps one live file and one rotated segment (`EVENTS_SEGMENT_NAME`); a
/// rotation renames the live file to the segment and drops whatever segment was there
/// before. A reader that attaches *after* a rotation would otherwise start in the present
/// and miss the events the previous live file held, so [`Tail::backlog`] replays the
/// retained ring first — the segment, then the live file — giving a new viewer the bounded
/// history the two files still hold. A reader already tailing when a rotation happens has
/// the live file's tail moved out from under it; [`Tail::poll`] notices the shorter file
/// and drains the segment from where it had got to before restarting on the fresh live
/// file, so a caught-up viewer loses nothing. The events the *dropped* segment held are
/// gone by design (the ring is bounded at roughly twice the budget); that gap is recorded
/// in `docs/design.md` §9, not hidden.
struct Tail {
    live: PathBuf,
    segment: PathBuf,
    offset: u64,
    pending: Vec<u8>,
}

impl Tail {
    fn new(live: PathBuf) -> Self {
        let segment = live.with_file_name(events::EVENTS_SEGMENT_NAME);
        Tail {
            live,
            segment,
            offset: 0,
            pending: Vec::new(),
        }
    }

    /// Replay the retained ring (segment then live) and leave the offset at the end of the
    /// live file, carrying any torn final line until it is completed.
    fn backlog(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(bytes) = fs::read(&self.segment) {
            let mut carry = Vec::new();
            take_lines(&bytes, &mut carry, &mut out);
        }
        if let Ok(bytes) = fs::read(&self.live) {
            let mut carry = Vec::new();
            take_lines(&bytes, &mut carry, &mut out);
            self.offset = bytes.len() as u64 - carry.len() as u64;
            self.pending = carry;
        }
        out
    }

    /// Every complete line appended since the last poll, in order.
    fn poll(&mut self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let live_len = fs::metadata(&self.live).map(|meta| meta.len()).unwrap_or(0);
        if live_len < self.offset {
            // The live file shrank, which only a rotation does: it is now the segment. Drain
            // whatever we had not yet read of it, then start over on the fresh live file.
            if let Ok(bytes) = read_from(&self.segment, self.offset) {
                take_lines(&bytes, &mut self.pending, &mut out);
            }
            self.offset = 0;
            self.pending.clear();
        }
        if let Ok(bytes) = read_from(&self.live, self.offset) {
            take_lines(&bytes, &mut self.pending, &mut out);
            self.offset += bytes.len() as u64;
        }
        Ok(out)
    }
}

fn read_from(path: &Path, offset: u64) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Split a byte window into complete event lines, carrying a trailing partial line so a
/// torn tail is held back until it is whole — the reader-side half of the writer's
/// "a torn final line is skipped, not corrupt" contract (#169).
fn take_lines(bytes: &[u8], carry: &mut Vec<u8>, out: &mut Vec<String>) {
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let mut line = std::mem::take(carry);
        line.extend_from_slice(&bytes[start..index]);
        start = index + 1;
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if !text.is_empty() && events::is_complete_object(text) {
            out.push(text.to_string());
        }
    }
    carry.extend_from_slice(&bytes[start..]);
}

/// Stream the events file as `text/event-stream` until the client disconnects.
fn serve_events(mut stream: TcpStream, config: &UiConfig) -> io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream; charset=utf-8\r\n\
          Cache-Control: no-cache\r\n\
          Connection: close\r\n\
          X-Accel-Buffering: no\r\n\
          \r\n",
    )?;
    stream.flush()?;

    let mut tail = Tail::new(config.events_path());
    for line in tail.backlog() {
        write_event(&mut stream, &line)?;
    }
    stream.flush()?;

    let mut last_ping = Instant::now();
    loop {
        thread::sleep(SSE_POLL);
        if let Ok(lines) = tail.poll() {
            if !lines.is_empty() {
                for line in lines {
                    write_event(&mut stream, &line)?;
                }
                stream.flush()?;
            }
        }
        if last_ping.elapsed() >= SSE_PING {
            stream.write_all(b": keep-alive\n\n")?;
            stream.flush()?;
            last_ping = Instant::now();
        }
    }
}

/// One SSE frame. The event lines are single-line JSON by construction, so a `data:` field
/// is exactly one line.
fn write_event(stream: &mut TcpStream, line: &str) -> io::Result<()> {
    stream.write_all(b"data: ")?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_escapes_and_plus() {
        assert_eq!(percent_decode("shows%2Fmovie.mkv"), "shows/movie.mkv");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%25"), "100%");
    }

    #[test]
    fn a_wrong_token_of_any_length_is_refused() {
        assert!(token_matches("s3cret", "s3cret"));
        assert!(!token_matches("s3cret", "s3cre"));
        assert!(!token_matches("s3cret", "s3cret!"));
        assert!(!token_matches("s3cret", ""));
    }

    #[test]
    fn take_lines_holds_a_torn_tail_and_emits_only_complete_objects() {
        let mut carry = Vec::new();
        let mut out = Vec::new();
        take_lines(b"{\"a\":1}\n{\"b\":", &mut carry, &mut out);
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
        take_lines(b"2}\n", &mut carry, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], "{\"b\":2}");
    }

    #[test]
    fn a_rotation_drains_the_segment_before_the_fresh_live_file() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join(events::EVENTS_NAME);
        let segment = tmp.path().join(events::EVENTS_SEGMENT_NAME);
        fs::write(&live, "{\"n\":1}\n{\"n\":2}\n").unwrap();

        let mut tail = Tail::new(live.clone());
        let backlog = tail.backlog();
        assert_eq!(backlog.len(), 2, "the live file is replayed on attach");

        // A rotation: the live file becomes the segment, and the fresh live file gets a
        // new line already appended (the writer rotates before writing).
        fs::rename(&live, &segment).unwrap();
        fs::write(&live, "{\"n\":3}\n").unwrap();
        let lines = tail.poll().unwrap();
        assert!(
            lines.contains(&"{\"n\":3}".to_string()),
            "the fresh live file is read from the start after a rotation: {lines:?}"
        );
    }
}
