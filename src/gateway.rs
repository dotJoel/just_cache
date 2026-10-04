//! The gateway namespace provider (issue #47): the catalog served over a small,
//! read-mostly WebDAV endpoint, for backup apps and non-POSIX consumers that cannot
//! mount.
//!
//! # What it serves
//!
//! - `PROPFIND` (Depth `0` or `1`): listing and metadata — size, kind, and the
//!   catalog's lifecycle state (`present` / `offloaded`) as a `jc:state` property.
//! - `GET` / `HEAD`, with a single `Range: bytes=…`: the bytes of a **present** copy.
//! - `POST <path>?restore`: the S3 "restore request". It is the only request that
//!   places bytes, and it does so through [`crate::restore::restore`] — the same
//!   copy, read-back and hash against the catalog's recorded checksum that
//!   `just_cache restore` uses. A `GET` of an offloaded object is refused with
//!   `409` naming the restore request, rather than streamed from a slow tier: the
//!   §4 contract is that a gateway consumer gets *explicit* restore semantics, so no
//!   client timeout ever turns into a broken read.
//!
//! Every other method (`PUT`, `DELETE`, `MKCOL`, `MOVE`, …) is `405`. The gateway is
//! not a write path into the namespace; the mover and `catalog sync` own that.
//!
//! # A read mutates nothing
//!
//! The catalog is reopened for each request and only read; no row is written by a
//! listing, a stat or a read. Bytes are opened read-only and with `O_NOATIME` where
//! the kernel allows it, because the mover's idea of "used" is still atime on the
//! symlink provider's tree: a backup app reading everything through the gateway must
//! not make every file look hot and pin it on the fast disk.
//!
//! # Authentication and tier containment
//!
//! Every request, including `OPTIONS`, must carry the configured token, either as
//! `Authorization: Bearer <token>` or as the password of HTTP Basic (any user name),
//! because most WebDAV clients only speak Basic. The token is compared in constant
//! time and is never logged: the access log names method, path and status only, and
//! no header is ever echoed. A path whose bytes resolve outside the watch root and the
//! configured `--dest` roots is refused with `403` even when the catalog records it
//! under some other root: the catalog may know about a disk this gateway was not told
//! to expose, and exposing it would be serving outside the configured tiers.
//!
//! # Deliberately small
//!
//! The HTTP layer is hand-written over `std::net` (one request per connection,
//! handled sequentially) so the gateway adds no dependency and no daemon requirement
//! to anything else: nothing outside the `gateway` subcommand links or starts it.
//! Plain HTTP only — run it on loopback or behind a TLS terminator; the token is a
//! bearer credential and is exposed to anyone who can read the wire.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::catalog::{self, Catalog, ObjectRecord};
use crate::namespace::{Entry, Namespace, NamespaceError};
use crate::restore::{self, RestoreRequest};

/// Requests larger than this (request line plus headers) are refused: the gateway
/// never accepts a body it would have to buffer, and a header block this large is an
/// attack or a broken client, not a WebDAV request.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// A connection that stops sending is dropped rather than holding the (sequential)
/// server forever.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the gateway needs.
#[derive(Clone)]
pub struct GatewayConfig {
    /// The catalog to serve. Must already exist; the gateway never creates one.
    pub catalog_path: PathBuf,
    /// The watched tree: the hot tier and where a restore puts bytes back.
    pub watch: PathBuf,
    /// The cold-tier roots the operator chose to expose, fastest first.
    pub dests: Vec<PathBuf>,
    /// The shared secret every request must present. Never logged.
    pub token: String,
    /// Print one access-log line per request to stderr (method, path, status).
    pub log: bool,
}

// Hand-written so a `{:?}` of the config in an error path can never print the token.
impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("catalog_path", &self.catalog_path)
            .field("watch", &self.watch)
            .field("dests", &self.dests)
            .field("token", &"<redacted>")
            .field("log", &self.log)
            .finish()
    }
}

/// Why a gateway could not start.
#[derive(Debug)]
pub enum GatewayError {
    /// The catalog file is missing; serving one conjured empty would present an empty
    /// namespace, the failure every provider must not have.
    NoCatalog(PathBuf),
    Catalog(catalog::CatalogError),
    /// The token is empty: an endpoint that accepts an empty credential is not
    /// authenticated.
    EmptyToken,
    /// A configured root could not be resolved.
    Root {
        path: PathBuf,
        error: io::Error,
    },
    Bind(io::Error),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::NoCatalog(path) => write!(
                f,
                "--catalog {} does not exist (create it with `just_cache catalog sync`)",
                path.display()
            ),
            GatewayError::Catalog(err) => write!(f, "the catalog could not be read: {err}"),
            GatewayError::EmptyToken => write!(
                f,
                "the gateway token is empty; refusing to serve an endpoint anyone can read"
            ),
            GatewayError::Root { path, error } => {
                write!(f, "cannot resolve root {}: {error}", path.display())
            }
            GatewayError::Bind(err) => write!(f, "cannot listen: {err}"),
        }
    }
}

impl std::error::Error for GatewayError {}

/// A bound, not-yet-serving gateway.
pub struct Gateway {
    listener: TcpListener,
    config: GatewayConfig,
    /// Canonical watch root plus canonical `--dest` roots: the only places bytes are
    /// served from or restored out of.
    allowed: Vec<PathBuf>,
}

impl Gateway {
    /// Check the configuration and bind. Nothing is served until [`Gateway::serve`].
    pub fn bind(addr: &str, config: GatewayConfig) -> Result<Self, GatewayError> {
        if config.token.is_empty() {
            return Err(GatewayError::EmptyToken);
        }
        if !config.catalog_path.is_file() {
            return Err(GatewayError::NoCatalog(config.catalog_path.clone()));
        }
        // Open once up front so a broken catalog is a startup error, not a 500 on the
        // first request.
        Catalog::open(&config.catalog_path).map_err(GatewayError::Catalog)?;
        let mut allowed = Vec::new();
        for root in std::iter::once(&config.watch).chain(config.dests.iter()) {
            let canonical = fs::canonicalize(root).map_err(|error| GatewayError::Root {
                path: root.clone(),
                error,
            })?;
            allowed.push(canonical);
        }
        let listener = TcpListener::bind(addr).map_err(GatewayError::Bind)?;
        Ok(Self {
            listener,
            config,
            allowed,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until the process ends. One connection at a time; a failing connection
    /// never stops the server.
    pub fn serve(&self) -> io::Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => self.handle(stream),
                Err(err) => {
                    if self.config.log {
                        eprintln!("just_cache gateway: accept failed: {err}");
                    }
                }
            }
        }
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let (method, target, response) = match read_request(&mut stream) {
            Ok(request) => {
                let response = self.respond(&request);
                (request.method, request.target, response)
            }
            Err(_) => (
                "-".to_string(),
                "-".to_string(),
                Response::text(400, "malformed request\n"),
            ),
        };
        if self.config.log {
            // Method, target and status only: never a header, so never a credential.
            eprintln!("just_cache gateway: {method} {target} {}", response.status);
        }
        let _ = response.write_to(&mut stream, method == "HEAD");
    }

    fn respond(&self, request: &Request) -> Response {
        if !self.authorized(request) {
            let mut response = Response::text(401, "authentication required\n");
            response.headers.push((
                "WWW-Authenticate".into(),
                "Basic realm=\"just_cache\"".into(),
            ));
            return response;
        }

        let (raw_path, query) = match request.target.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (request.target.as_str(), None),
        };
        let Some(path) = decode_path(raw_path) else {
            return Response::text(400, "bad path\n");
        };

        match request.method.as_str() {
            "OPTIONS" => {
                let mut response = Response::empty(200);
                response
                    .headers
                    .push(("Allow".into(), "OPTIONS, PROPFIND, GET, HEAD, POST".into()));
                response.headers.push(("DAV".into(), "1".into()));
                response
            }
            "PROPFIND" => self.propfind(&path, request.header("depth")),
            "GET" | "HEAD" => self.get(&path, request.header("range")),
            "POST" if query.is_some_and(|q| q.split('&').any(|p| p == "restore")) => {
                self.restore(&path)
            }
            _ => {
                let mut response =
                    Response::text(405, "the gateway is read-only apart from ?restore\n");
                response
                    .headers
                    .push(("Allow".into(), "OPTIONS, PROPFIND, GET, HEAD, POST".into()));
                response
            }
        }
    }

    fn authorized(&self, request: &Request) -> bool {
        let Some(value) = request.header("authorization") else {
            return false;
        };
        let presented = if let Some(token) = value.strip_prefix("Bearer ") {
            token.trim().as_bytes().to_vec()
        } else if let Some(encoded) = value.strip_prefix("Basic ") {
            match base64_decode(encoded.trim()) {
                // `user:password`; the user name is not a credential here.
                Some(decoded) => match decoded.iter().position(|&b| b == b':') {
                    Some(colon) => decoded[colon + 1..].to_vec(),
                    None => return false,
                },
                None => return false,
            }
        } else {
            return false;
        };
        constant_time_eq(&presented, self.config.token.as_bytes())
    }

    fn open_catalog(&self) -> Result<(Catalog, Namespace), Response> {
        let catalog = Catalog::open(&self.config.catalog_path)
            .map_err(|_| Response::text(500, "the catalog could not be read\n"))?;
        let namespace = Namespace::new(&catalog)
            .map_err(|_| Response::text(500, "the catalog namespace is unusable\n"))?;
        Ok((catalog, namespace))
    }

    fn within_tiers(&self, path: &Path) -> bool {
        // Canonicalize so a symlink under an allowed root that points elsewhere is
        // judged by where its bytes actually are.
        match fs::canonicalize(path) {
            Ok(real) => self.allowed.iter().any(|root| real.starts_with(root)),
            Err(_) => false,
        }
    }

    /// Every recorded copy of an object sits under a configured root. A restore is
    /// refused otherwise: [`restore::restore`] may read from any root the catalog
    /// recorded, and this gateway may only reach the ones it was given.
    fn record_within_tiers(&self, record: &ObjectRecord) -> bool {
        record.locations.iter().all(|location| {
            let tier = Path::new(&location.tier);
            self.allowed.iter().any(|root| root == tier)
        })
    }

    fn propfind(&self, path: &str, depth: Option<&str>) -> Response {
        let (catalog, namespace) = match self.open_catalog() {
            Ok(pair) => pair,
            Err(response) => return response,
        };
        let entry = match namespace.lookup(&catalog, path) {
            Ok(entry) => entry,
            Err(err) => return namespace_error(err),
        };
        let mut body = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <D:multistatus xmlns:D=\"DAV:\" xmlns:jc=\"https://github.com/dotJoel/just_cache\">\n",
        );
        match &entry {
            Entry::File { .. } => {
                if let Err(response) = self.prop_file(&catalog, &entry, &mut body) {
                    return response;
                }
            }
            Entry::Directory { path, children } => {
                prop_dir(path, &mut body);
                if depth.map(str::trim) != Some("0") {
                    for child in children {
                        let child_path = if path.is_empty() {
                            child.clone()
                        } else {
                            format!("{path}/{child}")
                        };
                        match namespace.lookup(&catalog, &child_path) {
                            Ok(Entry::Directory { path, .. }) => prop_dir(&path, &mut body),
                            Ok(file) => {
                                // A child outside the configured tiers is omitted, not
                                // listed with bytes it could never serve.
                                let _ = self.prop_file(&catalog, &file, &mut body);
                            }
                            // An unresolvable child is left out of the listing; a direct
                            // PROPFIND or GET of it reports the failure.
                            Err(_) => {}
                        }
                    }
                }
            }
        }
        body.push_str("</D:multistatus>\n");
        let mut response = Response::bytes(207, body.into_bytes());
        response.headers.push((
            "Content-Type".into(),
            "application/xml; charset=utf-8".into(),
        ));
        response
    }

    fn prop_file(
        &self,
        catalog: &Catalog,
        entry: &Entry,
        body: &mut String,
    ) -> Result<(), Response> {
        let Entry::File { path, size, .. } = entry else {
            unreachable!("prop_file is only called with a file")
        };
        let served = self.servable(catalog, entry)?;
        let state = if served.is_some() {
            "present"
        } else {
            "offloaded"
        };
        body.push_str(&format!(
            "<D:response><D:href>/{}</D:href><D:propstat><D:prop>\
             <D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength>\
             <jc:state>{state}</jc:state></D:prop>\
             <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n",
            xml_escape(&encode_path(path))
        ));
        Ok(())
    }

    /// Where a file's bytes may be read from right now, or `None` when the object is
    /// offloaded and needs a restore request first. `Err` is a refusal.
    ///
    /// Catalog state `present` serves the tier of record. An object the catalog still
    /// calls offloaded but whose hot path is now a regular file was restored since the
    /// last `catalog sync` (a gateway restore does not write catalog rows); that
    /// verified file is what is served, so a restore is observable without a sync.
    fn servable(&self, catalog: &Catalog, entry: &Entry) -> Result<Option<PathBuf>, Response> {
        let Entry::File { path, bytes, .. } = entry else {
            return Err(Response::text(405, "a directory has no bytes\n"));
        };
        let state = catalog
            .state_for_path(path)
            .map_err(|_| Response::text(500, "the catalog could not be read\n"))?;
        let candidate = if state.as_deref() == Some("present") {
            Some(bytes.clone())
        } else {
            let hot = self.config.watch.join(path);
            match fs::symlink_metadata(&hot) {
                Ok(metadata) if metadata.is_file() => Some(hot),
                _ => None,
            }
        };
        match candidate {
            Some(path) if !self.within_tiers(&path) => Err(Response::text(
                403,
                "this object resolves outside the gateway's configured tiers\n",
            )),
            other => {
                // Offloaded: still refuse if the copy that a restore would read is not
                // one this gateway may expose, so a listing does not advertise it.
                if other.is_none() && !self.within_tiers(bytes) {
                    return Err(Response::text(
                        403,
                        "this object resolves outside the gateway's configured tiers\n",
                    ));
                }
                Ok(other)
            }
        }
    }

    fn get(&self, path: &str, range: Option<&str>) -> Response {
        let (catalog, namespace) = match self.open_catalog() {
            Ok(pair) => pair,
            Err(response) => return response,
        };
        let entry = match namespace.lookup(&catalog, path) {
            Ok(entry @ Entry::File { .. }) => entry,
            Ok(Entry::Directory { .. }) => {
                return Response::text(405, "a directory has no bytes; use PROPFIND\n")
            }
            Err(err) => return namespace_error(err),
        };
        let bytes = match self.servable(&catalog, &entry) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return Response::text(
                    409,
                    "this object is offloaded; send `POST <path>?restore` first\n",
                )
            }
            Err(response) => return response,
        };

        let mut file = match open_read_noatime(&bytes) {
            Ok(file) => file,
            Err(_) => return Response::text(500, "the stored copy could not be opened\n"),
        };
        let len = match file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(_) => return Response::text(500, "the stored copy could not be read\n"),
        };

        let (status, start, end) = match range {
            None => (200, 0, len),
            Some(spec) => match parse_range(spec, len) {
                Some((start, end)) => (206, start, end),
                None => {
                    let mut response = Response::text(416, "unsatisfiable range\n");
                    response
                        .headers
                        .push(("Content-Range".into(), format!("bytes */{len}")));
                    return response;
                }
            },
        };
        let mut body = Vec::with_capacity((end - start) as usize);
        if file.seek(SeekFrom::Start(start)).is_err()
            || file.take(end - start).read_to_end(&mut body).is_err()
        {
            return Response::text(500, "the stored copy could not be read\n");
        }
        let mut response = Response::bytes(status, body);
        response
            .headers
            .push(("Content-Type".into(), "application/octet-stream".into()));
        response
            .headers
            .push(("Accept-Ranges".into(), "bytes".into()));
        if status == 206 {
            response.headers.push((
                "Content-Range".into(),
                format!("bytes {start}-{}/{len}", end - 1),
            ));
        }
        response
    }

    fn restore(&self, path: &str) -> Response {
        let (catalog, namespace) = match self.open_catalog() {
            Ok(pair) => pair,
            Err(response) => return response,
        };
        match namespace.lookup(&catalog, path) {
            Ok(Entry::File { .. }) => {}
            Ok(Entry::Directory { .. }) => {
                return Response::text(405, "a directory cannot be restored\n")
            }
            Err(err) => return namespace_error(err),
        }
        let record = match catalog.record_for_path(path) {
            Ok(Some(record)) => record,
            Ok(None) => return Response::text(404, "not in the catalog\n"),
            Err(_) => return Response::text(500, "the catalog could not be read\n"),
        };
        if !self.record_within_tiers(&record) {
            return Response::text(
                403,
                "a copy of this object is on a tier this gateway was not configured with\n",
            );
        }
        let hot = self.config.watch.join(path);
        let request = RestoreRequest {
            path: &hot,
            watch: &self.config.watch,
            dests: &self.config.dests,
            remove_copy: false,
            catalog: Some(&catalog),
            object_tier_configs: &[],
            encryption_keys: &[],
            offline_tiers: &[],
        };
        match restore::restore(&request) {
            // Both outcomes are success; `AlreadyPresent` makes a repeated restore
            // request idempotent, the way S3's is.
            Ok(outcome) => Response::text(200, &format!("{}\n", outcome.describe(Path::new(path)))),
            Err(err) => Response::text(409, &format!("restore refused: {err}\n")),
        }
    }
}

fn namespace_error(err: NamespaceError) -> Response {
    match err {
        NamespaceError::NotFound { .. } => Response::text(404, "not in the catalog\n"),
        NamespaceError::Unresolvable { .. } => Response::text(
            502,
            "the catalog names this but its copy cannot be resolved\n",
        ),
        NamespaceError::Catalog(_) => Response::text(500, "the catalog could not be read\n"),
    }
}

fn prop_dir(path: &str, body: &mut String) {
    let href = if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}/", encode_path(path))
    };
    body.push_str(&format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>\
         <D:resourcetype><D:collection/></D:resourcetype></D:prop>\
         <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\n",
        xml_escape(&href)
    ));
}

/// Open for reading without bumping atime when the kernel lets us (`O_NOATIME`
/// requires owning the file; otherwise fall back to a plain read-only open — the
/// fallback is an atime update, never a write).
fn open_read_noatime(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let noatime = rustix::fs::OFlags::NOATIME.bits() as i32;
        if let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(noatime)
            .open(path)
        {
            return Ok(file);
        }
    }
    File::open(path)
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream.take(MAX_HEADER_BYTES as u64));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request line"));
    };
    let (method, target) = (method.to_string(), target.to_string());
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "headers"));
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((key, value)) = header.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(Request {
        method,
        target,
        headers,
    })
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn empty(status: u16) -> Self {
        Self::bytes(status, Vec::new())
    }

    fn text(status: u16, text: &str) -> Self {
        let mut response = Self::bytes(status, text.as_bytes().to_vec());
        response
            .headers
            .push(("Content-Type".into(), "text/plain; charset=utf-8".into()));
        response
    }

    fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    fn write_to(&self, stream: &mut TcpStream, head_only: bool) -> io::Result<()> {
        let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (key, value) in &self.headers {
            out.push_str(&format!("{key}: {value}\r\n"));
        }
        out.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            self.body.len()
        ));
        stream.write_all(out.as_bytes())?;
        if !head_only {
            stream.write_all(&self.body)?;
        }
        stream.flush()
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        207 => "Multi-Status",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        416 => "Range Not Satisfiable",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    }
}

/// Percent-decode a request path into a namespace path, refusing anything that could
/// name a location by traversal (`..`, `.`) or smuggle a NUL. The namespace only
/// answers to catalogued names, but a traversal is refused before it reaches a lookup
/// so no code path ever has to reason about one.
fn decode_path(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    if path.contains('\0') {
        return None;
    }
    let trimmed = path.trim_matches('/');
    if Path::new(trimmed)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    if trimmed
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One `bytes=` range as a half-open `[start, end)`. Multi-range requests are not
/// supported and are answered as unsatisfiable rather than with the whole body, so a
/// client never mistakes a full response for the slice it asked for.
fn parse_range(spec: &str, len: u64) -> Option<(u64, u64)> {
    let spec = spec.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        let suffix: u64 = last.parse().ok()?;
        if suffix == 0 || len == 0 {
            return None;
        }
        return Some((len.saturating_sub(suffix), len));
    }
    let start: u64 = first.parse().ok()?;
    if start >= len {
        return None;
    }
    let end = if last.is_empty() {
        len
    } else {
        let last: u64 = last.parse().ok()?;
        if last < start {
            return None;
        }
        last.saturating_add(1).min(len)
    };
    Some((start, end))
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let text = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= value(c)? << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

/// Compare without an early exit, so response timing does not reveal how many leading
/// bytes of a guessed token were right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8 | ((a.len() ^ b.len()) >> 8) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0 && a.len() == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_as_half_open() {
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 4)));
        assert_eq!(parse_range("bytes=4-", 10), Some((4, 10)));
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 10)));
        assert_eq!(parse_range("bytes=5-100", 10), Some((5, 10)));
        assert_eq!(parse_range("bytes=10-", 10), None);
        assert_eq!(parse_range("bytes=0-1,3-4", 10), None);
        assert_eq!(parse_range("bytes=4-2", 10), None);
    }

    #[test]
    fn base64_round_trips_basic_credentials() {
        assert_eq!(base64_decode("dXNlcjpzM2NyZXQ=").unwrap(), b"user:s3cret");
        assert_eq!(base64_decode("YQ").unwrap(), b"a");
        assert!(base64_decode("!!").is_none());
    }

    #[test]
    fn traversal_and_nul_are_refused() {
        assert_eq!(
            decode_path("/shows/a%20b.bin").as_deref(),
            Some("shows/a b.bin")
        );
        assert_eq!(decode_path("/").as_deref(), Some(""));
        assert!(decode_path("/shows/../etc").is_none());
        assert!(decode_path("/shows/%2E%2E/etc").is_none());
        assert!(decode_path("/a%00b").is_none());
    }

    #[test]
    fn token_comparison_needs_exact_match() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
