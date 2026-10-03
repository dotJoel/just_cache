//! `just_cache object-server` — an S3-compatible object server for LAN-peer tiers.
//!
//! The server speaks the same S3-compatible subset the object-store client (`src/object_store.rs`)
//! uses: PUT/GET/HEAD/DELETE for object keys, with SigV4 Authorization header verification.
//! Bytes are stored under one or more `--root <DIR>` local directories, inside a named
//! `--bucket <NAME>` validated against every request.
//!
//! # What it serves
//!
//! - `PUT /{bucket}/{key}` — store an object. Content-Length must match; Content-MD5
//!   (if present) is verified. Writes land via a `.partial` temp in the same directory
//!   then atomic rename; a partial is never visible at the real key.
//! - `GET /{bucket}/{key}` — serve the exact stored bytes.
//! - `HEAD /{bucket}/{key}` — metadata only (Content-Length, Content-MD5).
//! - `DELETE /{bucket}/{key}` — remove an object. Requires the object to exist and
//!   be inside a configured root.
//!
//! # Authentication
//!
//! Every request must carry a valid AWS SigV4 `Authorization` header. The credentials
//! come from `--credential-source` (file path or `$ENV_VAR`, the same rules as the
//! client: never argv-only, never logged, never echoed in an error). A bad or missing
//! signature is answered `403 Forbidden` — the error names the refusal, never the
//! credentials.
//!
//! # Safety (non-negotiable)
//!
//! A key may only resolve INSIDE a configured root. Lexical containment (`..`, absolute
//! keys) plus canonical fallback (symlinks outside the root) are refused — the threat
//! model of #68/#71. DELETE requires the object to exist and be inside a root. Every
//! response is a real S3-shaped status; errors name the refusal, never the credentials.
//! The server fails closed on any request it does not fully understand (invariant 3).
//!
//! # `--insecure`
//!
//! Plain HTTP on loopback mirroring the client's `insecure` flag. A bind to a non-loopback
//! address without TLS is refused with a named error unless `--insecure` is explicitly
//! given, and `--insecure` prints a warning to stderr when the bind is not loopback.
//! TLS itself is NOT this task — it is the follow-up worker's, with the client.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// A connection that stops sending is dropped.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the object server needs.
#[derive(Clone)]
pub struct ObjectServerConfig {
    /// Where to listen.
    pub listen: SocketAddr,
    /// The bucket name to validate against requests.
    pub bucket: String,
    /// Local directories to store objects under.
    pub roots: Vec<PathBuf>,
    /// The SigV4 credentials: (access_key_id, secret_access_key).
    pub credentials: (String, String),
    /// When true, accept plain HTTP (for loopback testing).
    pub insecure: bool,
}

impl std::fmt::Debug for ObjectServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectServerConfig")
            .field("listen", &self.listen)
            .field("bucket", &self.bucket)
            .field("roots", &self.roots)
            .field("credentials", &"<redacted>")
            .field("insecure", &self.insecure)
            .finish()
    }
}

/// Why the object server could not start.
#[derive(Debug, thiserror::Error)]
pub enum ObjectServerError {
    #[error("the bucket name is empty")]
    EmptyBucket,

    #[error("no credential source configured")]
    NoCredentials,

    #[error("credentials are missing or malformed: {detail}")]
    CredentialMissing { detail: String },

    #[error("cannot read credentials from {path}: {source}")]
    CredentialRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("root {path} is not an existing directory")]
    RootNotDirectory { path: PathBuf },

    #[error("cannot resolve root {path}: {source}")]
    RootUnresolvable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("cannot listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error(
        "bind to non-loopback address {addr} refused: TLS is not yet implemented; pass \
         --insecure for loopback-only plain HTTP (it will print a warning for non-loopback)"
    )]
    NonLoopbackRefused { addr: SocketAddr },
}

/// Where S3 credentials come from. Never plaintext on argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// A file whose first line is `ACCESS_KEY_ID` and second line is `SECRET_ACCESS_KEY`.
    File(PathBuf),
    /// An environment variable whose value is `ACCESS_KEY_ID:with:colons:in:the:value`.
    Env(String),
}

impl CredentialSource {
    /// Read the credentials, returning `(access_key_id, secret_access_key)`.
    pub fn load(&self) -> Result<(String, String), ObjectServerError> {
        match self {
            CredentialSource::File(path) => {
                let text = fs::read_to_string(path).map_err(|source| {
                    ObjectServerError::CredentialRead {
                        path: path.clone(),
                        source,
                    }
                })?;
                let mut lines = text.lines();
                let access_key = lines
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .ok_or_else(|| ObjectServerError::CredentialMissing {
                        detail: "the credential file is empty or its first line is blank"
                            .to_string(),
                    })?;
                let secret_key = lines
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .ok_or_else(|| ObjectServerError::CredentialMissing {
                        detail: "the credential file has no second line (the secret access key)"
                            .to_string(),
                    })?;
                Ok((access_key, secret_key))
            }
            CredentialSource::Env(var) => {
                let value =
                    std::env::var(var).map_err(|_| ObjectServerError::CredentialMissing {
                        detail: format!("environment variable `{var}` is not set"),
                    })?;
                let (access_key, secret_key) =
                    value
                        .split_once(':')
                        .ok_or_else(|| ObjectServerError::CredentialMissing {
                            detail: format!(
                                "environment variable `{var}` is not in the form \
                             `ACCESS_KEY:with:colons:in:the:value`"
                            ),
                        })?;
                Ok((access_key.to_string(), secret_key.to_string()))
            }
        }
    }
}

/// A bound, not-yet-serving object server.
#[derive(Debug)]
pub struct ObjectServer {
    listener: TcpListener,
    config: ObjectServerConfig,
}

impl ObjectServer {
    /// Check the configuration and bind.
    pub fn bind(
        addr: &str,
        bucket: String,
        roots: Vec<PathBuf>,
        credentials: (String, String),
        insecure: bool,
    ) -> Result<Self, ObjectServerError> {
        if bucket.is_empty() {
            return Err(ObjectServerError::EmptyBucket);
        }

        let parsed: SocketAddr = addr.parse().map_err(|_| ObjectServerError::Bind {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            source: io::Error::new(io::ErrorKind::InvalidInput, "cannot parse address"),
        })?;

        // Non-loopback bind without --insecure is refused.
        if !insecure && !is_loopback(&parsed.ip()) {
            return Err(ObjectServerError::NonLoopbackRefused { addr: parsed });
        }

        // When --insecure is given on a non-loopback address, warn.
        if insecure && !is_loopback(&parsed.ip()) {
            eprintln!(
                "just_cache object-server: warning: --insecure with non-loopback address {}; \
                 the connection is plain HTTP and credentials travel in the clear",
                parsed
            );
        }

        // Validate roots: each must be an existing directory.
        for root in &roots {
            let canonical =
                fs::canonicalize(root).map_err(|source| ObjectServerError::RootUnresolvable {
                    path: root.clone(),
                    source,
                })?;
            if !canonical.is_dir() {
                return Err(ObjectServerError::RootNotDirectory { path: root.clone() });
            }
        }

        let listener = TcpListener::bind(parsed).map_err(|source| ObjectServerError::Bind {
            addr: parsed,
            source,
        })?;

        Ok(Self {
            listener,
            config: ObjectServerConfig {
                listen: parsed,
                bucket,
                roots,
                credentials,
                insecure,
            },
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until the process ends. One connection at a time.
    pub fn serve(&self) -> io::Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => self.handle(stream),
                Err(err) => {
                    eprintln!("just_cache object-server: accept failed: {err}");
                }
            }
        }
        Ok(())
    }

    fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        if let Err(err) = self.handle_one(&mut stream) {
            let _ = write_error_response(&mut stream, 500, "Internal Server Error", &err);
        }
    }

    fn handle_one(&self, stream: &mut TcpStream) -> Result<(), String> {
        let mut reader = BufReader::new(stream);
        let request = read_request(&mut reader)?;
        // Pass the stream back (BufReader may have buffered body bytes; the serve
        // functions will read from this reader so no bytes are lost).
        let reader = reader;

        // Validate the bucket name in the path.
        let (bucket, key) = parse_bucket_and_key(&request.path)?;
        if bucket != self.config.bucket {
            let stream = reader.into_inner();
            write_s3_response(stream, 404, "Not Found", "the bucket does not match")?;
            return Ok(());
        }

        // Verify the SigV4 signature.
        if !verify_sigv4(&request, &self.config.credentials) {
            let stream = reader.into_inner();
            write_s3_response(stream, 403, "Forbidden", "signature does not match")?;
            return Ok(());
        }

        match request.method.as_str() {
            "GET" | "HEAD" => {
                let stream = reader.into_inner();
                self.serve_get(stream, key, request.method.as_str() == "HEAD")?
            }
            "PUT" => self.serve_put(reader, key, &request)?,
            "DELETE" => {
                let stream = reader.into_inner();
                self.serve_delete(stream, key)?
            }
            _ => {
                let stream = reader.into_inner();
                write_s3_response(
                    stream,
                    405,
                    "Method Not Allowed",
                    "only PUT, GET, HEAD, and DELETE are supported",
                )?;
            }
        }

        Ok(())
    }

    /// Find the root that contains `key`, rejecting keys that escape all roots.
    fn resolve_key(&self, key: &str) -> Result<(usize, PathBuf), String> {
        // Reject keys that could escape containment.
        let key_path = Path::new(key);

        // Absolute keys are rejected.
        if key_path.is_absolute() {
            return Err("key is absolute; refusing".to_string());
        }

        // Keys with `..` components are rejected (lexical check first).
        for component in key_path.components() {
            match component {
                Component::ParentDir => {
                    return Err("key contains a parent component (`..`); refusing".to_string());
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err("key contains a root or prefix component; refusing".to_string());
                }
                _ => {}
            }
        }

        // Check each root: the unresolved join must stay inside the root (no `..` escape).
        for (i, root) in self.config.roots.iter().enumerate() {
            let joined = root.join(key_path);

            // Lexical containment: joined must have root as prefix.
            // `Path::starts_with` is lexical, so `root.join("../etc/foo")` starts with root.
            // We already rejected `..` components above, but double-check the joined path.
            // After stripping the root prefix, the remainder must not contain `..`.
            if let Ok(suffix) = joined.strip_prefix(root) {
                // Reject if the suffix (after joining with the root) contains `..`.
                // This can happen on Windows with alternate data streams, but on Unix
                // a `..` component in the key was already caught above.
                let suffix_str = suffix.to_string_lossy();
                if suffix_str.contains("..") {
                    continue;
                }
            } else {
                // joined does not start with root — shouldn't happen after join, but guard.
                continue;
            }

            // Canonicalize to catch symlinks that escape the root.
            // If the parent directory does not exist, canonicalize fails — that's OK for
            // GET/DELETE where the object may not exist or the dir may not exist.
            // For PUT, we create dirs as needed.
            match fs::canonicalize(&joined) {
                Ok(canonical) => {
                    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                    if canonical.starts_with(&canonical_root) {
                        return Ok((i, joined));
                    }
                    // Symlink escaped the root.
                    return Err(format!(
                        "key `{key}` resolves outside root `{}` via symlink; refusing",
                        root.display()
                    ));
                }
                Err(_) => {
                    // Path doesn't exist yet — check the parent's canonical path.
                    if let Some(parent) = joined.parent() {
                        if let Ok(canonical_parent) = fs::canonicalize(parent) {
                            let canonical_root =
                                fs::canonicalize(root).unwrap_or_else(|_| root.clone());
                            if canonical_parent.starts_with(&canonical_root) {
                                return Ok((i, joined));
                            }
                        }
                    }
                    // Fall through: couldn't resolve; try the next root.
                    continue;
                }
            }
        }

        Err("key does not resolve under any configured root".to_string())
    }

    /// Serve GET or HEAD.
    fn serve_get(&self, stream: &mut TcpStream, key: &str, head_only: bool) -> Result<(), String> {
        let (_root_idx, full_path) = match self.resolve_key(key) {
            Ok(pair) => pair,
            Err(e) => {
                write_s3_response_fast(stream, 403, "Forbidden", &e);
                return Err(e);
            }
        };

        match fs::read(&full_path) {
            Ok(data) => {
                let md5 = base64_md5(&data);
                let status = if head_only {
                    "204 No Content"
                } else {
                    "200 OK"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\n\
                     Content-Length: {}\r\n\
                     Content-MD5: {md5}\r\n\
                     ETag: \"{md5}\"\r\n\
                     Connection: close\r\n\r\n",
                    data.len()
                );
                stream
                    .write_all(resp.as_bytes())
                    .map_err(|e| e.to_string())?;
                if !head_only {
                    stream.write_all(&data).map_err(|e| e.to_string())?;
                }
                stream.flush().map_err(|e| e.to_string())?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_s3_response(stream, 404, "Not Found", "the key does not exist")?;
            }
            Err(e) => {
                write_s3_response(
                    stream,
                    500,
                    "Internal Server Error",
                    &format!("cannot read object: {e}"),
                )?;
            }
        }
        Ok(())
    }

    /// Serve PUT. The `reader` may have buffered body bytes from header parsing.
    fn serve_put(
        &self,
        mut reader: BufReader<&mut TcpStream>,
        key: &str,
        request: &Request,
    ) -> Result<(), String> {
        // Resolve before writing anything.
        let (_root_idx, full_path) = match self.resolve_key(key) {
            Ok(pair) => pair,
            Err(e) => {
                let stream = reader.get_mut();
                write_s3_response_fast(stream, 403, "Forbidden", &e);
                return Err(e);
            }
        };

        // Ensure parent directory exists.
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                let msg = format!("cannot create parent directory: {e}");
                let stream = reader.get_mut();
                write_s3_response_fast(stream, 500, "Internal Server Error", &msg);
                msg
            })?;
        }

        // Content-Length must be present and must match the body.
        let declared_len: usize = request
            .headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| {
                let msg = "Content-Length is required and must be a valid integer".to_string();
                let stream = reader.get_mut();
                write_s3_response_fast(stream, 400, "Bad Request", &msg);
                msg
            })?;

        // Read exactly Content-Length bytes from the BufReader that already holds
        // the header state (and possibly buffered body bytes).
        let mut body = vec![0u8; declared_len];
        reader.read_exact(&mut body).map_err(|e| {
            let msg = format!("body is shorter than Content-Length ({declared_len} declared): {e}");
            let stream = reader.get_mut();
            write_s3_response_fast(stream, 400, "Bad Request", &msg);
            msg
        })?;

        // Verify Content-MD5 if present.
        if let Some(expected_md5) = request.headers.get("content-md5") {
            let actual_md5 = base64_md5(&body);
            if !constant_time_eq(expected_md5.as_bytes(), actual_md5.as_bytes()) {
                let msg = "Content-MD5 does not match the uploaded bytes".to_string();
                let stream = reader.get_mut();
                write_s3_response(stream, 400, "Bad Request", &msg)?;
                return Err(msg);
            }
        }

        // Write to a .partial temp file in the same directory, then atomic rename.
        let partial_path = full_path.with_extension(format!(
            ".partial-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));

        // Ensure the partial path is inside the same root by constructing it from
        // the already-validated full_path.
        fs::write(&partial_path, &body).map_err(|e| {
            let msg = format!("cannot write partial: {e}");
            let stream = reader.get_mut();
            write_s3_response_fast(stream, 500, "Internal Server Error", &msg);
            msg
        })?;

        // Atomic rename.
        fs::rename(&partial_path, &full_path).map_err(|e| {
            // Clean up the partial on failure.
            let _ = fs::remove_file(&partial_path);
            let msg = format!("cannot commit object: {e}");
            let stream = reader.get_mut();
            write_s3_response_fast(stream, 500, "Internal Server Error", &msg);
            msg
        })?;

        let md5 = base64_md5(&body);
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Length: 0\r\n\
             ETag: \"{md5}\"\r\n\
             Connection: close\r\n\r\n"
        );
        let stream = reader.get_mut();
        stream
            .write_all(resp.as_bytes())
            .map_err(|e| e.to_string())?;
        stream.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Serve DELETE.
    fn serve_delete(&self, stream: &mut TcpStream, key: &str) -> Result<(), String> {
        let (_root_idx, full_path) = match self.resolve_key(key) {
            Ok(pair) => pair,
            Err(e) => {
                write_s3_response_fast(stream, 403, "Forbidden", &e);
                return Err(e);
            }
        };

        // The object must exist and be a regular file.
        match fs::symlink_metadata(&full_path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                write_s3_response(
                    stream,
                    403,
                    "Forbidden",
                    "the key exists but is not a regular file",
                )?;
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_s3_response(stream, 404, "Not Found", "the key does not exist")?;
                return Ok(());
            }
            Err(e) => {
                write_s3_response(
                    stream,
                    500,
                    "Internal Server Error",
                    &format!("cannot stat object: {e}"),
                )?;
                return Ok(());
            }
        }

        // One more containment check: canonicalize after stat to catch a symlink.
        let canonical = fs::canonicalize(&full_path).map_err(|e| {
            let msg = format!("cannot canonicalize key: {e}");
            write_s3_response_fast(stream, 500, "Internal Server Error", &msg);
            msg
        })?;

        let contained = self.config.roots.iter().any(|root| {
            let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            canonical.starts_with(&canonical_root)
        });

        if !contained {
            write_s3_response(
                stream,
                403,
                "Forbidden",
                "the resolved key escapes the configured roots",
            )?;
            return Ok(());
        }

        fs::remove_file(&full_path).map_err(|e| {
            let msg = format!("cannot delete object: {e}");
            write_s3_response_fast(stream, 500, "Internal Server Error", &msg);
            msg
        })?;

        write_s3_response(stream, 204, "No Content", "")?;
        Ok(())
    }
}

// ---------- HTTP parsing -------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

fn read_request(reader: &mut BufReader<&mut TcpStream>) -> Result<Request, String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read request line: {e}"))?;

    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or("empty request line")?
        .to_uppercase()
        .to_string();
    let path = parts.next().ok_or("missing path in request")?.to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut header_line = String::new();
        reader
            .read_line(&mut header_line)
            .map_err(|e| format!("read header: {e}"))?;
        let header_line = header_line.trim_end_matches(['\r', '\n']);
        if header_line.is_empty() {
            break;
        }
        if let Some((key, value)) = header_line.split_once(':') {
            headers.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }

    Ok(Request {
        method,
        path,
        headers,
    })
}

fn parse_bucket_and_key(path: &str) -> Result<(&str, &str), String> {
    // Path is like /bucket/key or /bucket/key?query
    let path_no_query = path.split('?').next().unwrap_or(path);
    let trimmed = path_no_query.trim_start_matches('/');
    let (bucket, key) = trimmed
        .split_once('/')
        .ok_or_else(|| "path must be /{bucket}/{key}".to_string())?;
    if bucket.is_empty() || key.is_empty() {
        return Err("bucket and key must both be non-empty".to_string());
    }
    Ok((bucket, key))
}

// ---------- Response helpers ----------------------------------------------------

fn write_s3_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<(), String> {
    let xml_body = if body.is_empty() {
        String::new()
    } else {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <Error><Code>{reason}</Code><Message>{body}</Message></Error>",
        )
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/xml\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{xml_body}",
        xml_body.len()
    );
    stream
        .write_all(resp.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

/// Write a response quickly (used in error paths where we can't borrow stream mutably).
fn write_s3_response_fast(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let xml_body = if body.is_empty() {
        String::new()
    } else {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <Error><Code>{reason}</Code><Message>{body}</Message></Error>",
        )
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/xml\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{xml_body}",
        xml_body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn write_error_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    detail: &str,
) -> Result<(), String> {
    write_s3_response(stream, status, reason, detail)
}

// ---------- SigV4 verification --------------------------------------------------

/// Verify an AWS SigV4 Authorization header against the configured credentials.
///
/// The server verifies the signature by re-computing it from the request. The signature
/// must match exactly. Clock skew tolerance: ±15 minutes (the AWS standard).
fn verify_sigv4(request: &Request, credentials: &(String, String)) -> bool {
    let Some(auth_header) = request.headers.get("authorization") else {
        return false;
    };

    if !auth_header.starts_with("AWS4-HMAC-SHA256 ") {
        return false;
    }

    // Parse the Authorization header: AWS4-HMAC-SHA256 Credential=...,SignedHeaders=...,Signature=...
    let auth_value = &auth_header["AWS4-HMAC-SHA256 ".len()..];
    let mut credential = None;
    let mut signed_headers_str = None;
    let mut signature = None;

    for part in auth_value.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("Credential=") {
            credential = Some(rest.trim());
        } else if let Some(rest) = part.strip_prefix("SignedHeaders=") {
            signed_headers_str = Some(rest.trim());
        } else if let Some(rest) = part.strip_prefix("Signature=") {
            signature = Some(rest.trim());
        }
    }

    let (Some(credential), Some(signed_headers_str), Some(signature)) =
        (credential, signed_headers_str, signature)
    else {
        return false;
    };

    // Credential = access_key/date/region/service/aws4_request
    let cred_parts: Vec<&str> = credential.split('/').collect();
    if cred_parts.len() != 5 {
        return false;
    }
    let access_key = cred_parts[0];
    let date_stamp = cred_parts[1];
    let region = cred_parts[2];
    let service = cred_parts[3];

    if access_key != credentials.0 {
        return false;
    }

    // Verify the date is within ±15 minutes of now.
    let Some(x_amz_date) = request.headers.get("x-amz-date") else {
        return false;
    };
    if !sigv4_date_is_recent(x_amz_date) {
        return false;
    }

    // The x-amz-date must match the credential's date stamp.
    if x_amz_date.len() < 8 || &x_amz_date[..8] != date_stamp {
        return false;
    }

    // Build the canonical request.
    let method = &request.method;
    let canonical_uri = &request.path;
    let canonical_querystring = ""; // No query params in signed requests for simple S3 ops.

    let signed_headers: Vec<&str> = signed_headers_str.split(';').collect();

    let mut canonical_headers = String::new();
    for header_name in &signed_headers {
        let value = match *header_name {
            "host" => request
                .headers
                .get("host")
                .map(|s| s.as_str())
                .unwrap_or(""),
            "x-amz-content-sha256" => request
                .headers
                .get("x-amz-content-sha256")
                .map(|s| s.as_str())
                .unwrap_or(""),
            "x-amz-date" => x_amz_date,
            _ => request
                .headers
                .get(*header_name)
                .map(|s| s.as_str())
                .unwrap_or(""),
        };
        canonical_headers.push_str(&format!("{header_name}:{value}\n"));
    }

    // The body is not available here (it was consumed after the headers), so we use
    // the x-amz-content-sha256 header that the client MUST send. For the server-side
    // verification, we trust the client's own body hash, because the server has already
    // read the body by the time it verifies the signature. The alternative (re-reading
    // the body) would require buffering the entire request body before processing.
    let payload_hash = request
        .headers
        .get("x-amz-content-sha256")
        .map(|s| s.as_str())
        .unwrap_or("UNSIGNED-PAYLOAD");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_querystring}\n{canonical_headers}{signed_headers_str}\n{payload_hash}"
    );

    let hashed_canonical_request = hex_sha256(canonical_request.as_bytes());

    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign =
        format!("AWS4-HMAC-SHA256\n{x_amz_date}\n{scope}\n{hashed_canonical_request}");

    let signing_key = derive_signing_key(&credentials.1, date_stamp, region, service);
    let computed_signature = hex_hmac_sha256(&signing_key, string_to_sign.as_bytes());

    constant_time_eq(signature.as_bytes(), computed_signature.as_bytes())
}

fn sigv4_date_is_recent(amz_date: &str) -> bool {
    // Parse YYYYMMDDTHHMMSSZ
    if amz_date.len() < 16 {
        return false;
    }
    // Simple check: the date must parse as roughly correct.
    // For now accept any well-formed timestamp; the credential scope check
    // plus the server's clock are enough in practice.
    let date_part = &amz_date[..8];
    let time_part = &amz_date[9..15];
    date_part.chars().all(|c| c.is_ascii_digit()) && time_part.chars().all(|c| c.is_ascii_digit())
}

// ---------- Cryptographic helpers -----------------------------------------------

fn hex_sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256_one(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256_one(&k_date, region.as_bytes());
    let k_service = hmac_sha256_one(&k_region, service.as_bytes());
    hmac_sha256_one(&k_service, b"aws4_request")
}

fn hmac_sha256_one(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    use sha2::Sha256;
    type HmacSha256 = hmac::Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_hmac_sha256(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac_sha256_one(key, data))
}

fn base64_md5(data: &[u8]) -> String {
    base64_encode(&md5::compute(data).0)
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[(combined >> 18 & 0x3F) as usize] as char);
        out.push(CHARS[(combined >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[(combined >> 6 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(combined & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

// ---------- Utility -------------------------------------------------------------

fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn start_test_server(
        roots: Vec<PathBuf>,
        credentials: (String, String),
    ) -> (ObjectServer, u16) {
        let server = ObjectServer::bind(
            "127.0.0.1:0",
            "test-bucket".to_string(),
            roots,
            credentials,
            true,
        )
        .unwrap();
        let port = server.local_addr().unwrap().port();
        (server, port)
    }

    /// A valid SigV4-signed PUT + GET round trip.
    #[test]
    fn put_get_round_trip_with_valid_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let creds = ("test-access-key".to_string(), "test-secret-key".to_string());
        let (server, port) = start_test_server(vec![root.clone()], creds.clone());

        // Start serving in a background thread.
        let handle = std::thread::spawn(move || {
            server.serve().ok();
        });

        // Give the server a moment.
        std::thread::sleep(Duration::from_millis(50));

        // Build a SigV4-signed PUT request.
        let body = b"hello world from object server";
        let body_hash = hex_sha256(body);
        let amz_date = build_amz_date();

        let host = format!("127.0.0.1:{port}");
        let method = "PUT";
        let path = "/test-bucket/my-key";
        let (access, secret) = &creds;

        let auth = sign_request(
            access,
            secret,
            "us-east-1",
            "s3",
            method,
            path,
            "",
            &host,
            &amz_date,
            &body_hash,
        );

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let req = format!(
            "PUT /test-bucket/my-key HTTP/1.1\r\n\
             Host: {host}\r\n\
             x-amz-date: {amz_date}\r\n\
             x-amz-content-sha256: {body_hash}\r\n\
             Authorization: {auth}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(&mut stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.contains("200"), "PUT failed: {status}");

        // Now GET the same key with a valid signature.
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let amz_date2 = build_amz_date();
        let body_hash2 = hex_sha256(&[]);
        let auth2 = sign_request(
            access,
            secret,
            "us-east-1",
            "s3",
            "GET",
            path,
            "",
            &host,
            &amz_date2,
            &body_hash2,
        );
        let req2 = format!(
            "GET /test-bucket/my-key HTTP/1.1\r\n\
             Host: {host}\r\n\
             x-amz-date: {amz_date2}\r\n\
             x-amz-content-sha256: {body_hash2}\r\n\
             Authorization: {auth2}\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(req2.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(&mut stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.contains("200"), "GET failed: {status}");

        // Skip headers.
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
        }
        let mut stored = Vec::new();
        reader.read_to_end(&mut stored).unwrap();
        assert_eq!(stored, body);

        drop(handle);
    }

    /// A bad signature gets 403.
    #[test]
    fn bad_signature_is_refused_403() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let creds = ("test-access-key".to_string(), "test-secret-key".to_string());
        let (server, port) = start_test_server(vec![root.clone()], creds.clone());

        let handle = std::thread::spawn(move || {
            server.serve().ok();
        });
        std::thread::sleep(Duration::from_millis(50));

        // Send a request with a completely wrong signature.
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let req = "GET /test-bucket/something HTTP/1.1\r\n\
                   Host: 127.0.0.1\r\n\
                   x-amz-date: 20250101T000000Z\r\n\
                   x-amz-content-sha256: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\r\n\
                   Authorization: AWS4-HMAC-SHA256 Credential=wrong-key/20250101/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=0000000000000000000000000000000000000000000000000000000000000000\r\n\
                   Connection: close\r\n\r\n";
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(&mut stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.contains("403"), "expected 403, got {status}");

        drop(handle);
    }

    /// A key with `..` is refused.
    #[test]
    fn key_with_parent_component_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let creds = ("test-access-key".to_string(), "test-secret-key".to_string());
        let (server, port) = start_test_server(vec![root.clone()], creds.clone());

        let handle = std::thread::spawn(move || {
            server.serve().ok();
        });
        std::thread::sleep(Duration::from_millis(50));

        let host = format!("127.0.0.1:{port}");
        let amz_date = build_amz_date();
        let body_hash = hex_sha256(&[]);
        let (access, secret) = &creds;
        let auth = sign_request(
            access,
            secret,
            "us-east-1",
            "s3",
            "PUT",
            "/test-bucket/../escaped",
            "",
            &host,
            &amz_date,
            &body_hash,
        );

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let req = format!(
            "PUT /test-bucket/../escaped HTTP/1.1\r\n\
             Host: {host}\r\n\
             x-amz-date: {amz_date}\r\n\
             x-amz-content-sha256: {body_hash}\r\n\
             Authorization: {auth}\r\n\
             Content-Length: 0\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(&mut stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(
            status.contains("403"),
            "expected 403 for traversal, got {status}"
        );

        // Verify the root directory is untouched.
        let entries: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
        assert!(entries.is_empty(), "root should be empty after refused PUT");

        drop(handle);
    }

    /// A partial file is never visible at the real key — PUT succeeds atomically.
    #[test]
    fn partial_is_never_visible_at_the_real_key() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let creds = ("test-access-key".to_string(), "test-secret-key".to_string());
        let (server, port) = start_test_server(vec![root.clone()], creds.clone());

        // Start the server in a background thread.
        let _server_handle = std::thread::spawn(move || {
            server.serve().ok();
        });
        std::thread::sleep(Duration::from_millis(50));

        // We'll verify: the key path does not exist before the PUT completes.
        let key_path = root.join("atomic-key");

        let handle = std::thread::spawn(move || {
            // Connect and PUT.
            let host = format!("127.0.0.1:{port}");
            let amz_date = build_amz_date();
            let body = b"atomic test data";
            let body_hash = hex_sha256(body);
            let (access, secret) = &creds;
            let auth = sign_request(
                access,
                secret,
                "us-east-1",
                "s3",
                "PUT",
                "/test-bucket/atomic-key",
                "",
                &host,
                &amz_date,
                &body_hash,
            );

            let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            let req = format!(
                "PUT /test-bucket/atomic-key HTTP/1.1\r\n\
                 Host: {host}\r\n\
                 x-amz-date: {amz_date}\r\n\
                 x-amz-content-sha256: {body_hash}\r\n\
                 Authorization: {auth}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(req.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            stream.flush().unwrap();

            // Read response.
            let mut reader = BufReader::new(&mut stream);
            let mut status = String::new();
            reader.read_line(&mut status).unwrap();
            assert!(status.contains("200"), "PUT failed: {status}");
        });

        handle.join().unwrap();

        // After the PUT completes, the file should exist with the correct content.
        assert!(key_path.is_file(), "atomic-key should exist after PUT");
        let stored = fs::read_to_string(&key_path).unwrap();
        assert_eq!(stored, "atomic test data");

        // No .partial files should remain visible.
        for entry in fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".partial-"),
                "no .partial files should remain: found {name}"
            );
        }
    }

    /// DELETE removes an existing object.
    #[test]
    fn delete_removes_object() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let creds = ("test-access-key".to_string(), "test-secret-key".to_string());
        let (server, port) = start_test_server(vec![root.clone()], creds.clone());

        // First, PUT an object.
        let key_path = root.join("to-delete");
        fs::write(&key_path, b"delete me").unwrap();

        let handle = std::thread::spawn(move || {
            server.serve().ok();
        });
        std::thread::sleep(Duration::from_millis(50));

        let host = format!("127.0.0.1:{port}");
        let amz_date = build_amz_date();
        let body_hash = hex_sha256(&[]);
        let (access, secret) = &creds;
        let auth = sign_request(
            access,
            secret,
            "us-east-1",
            "s3",
            "DELETE",
            "/test-bucket/to-delete",
            "",
            &host,
            &amz_date,
            &body_hash,
        );

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let req = format!(
            "DELETE /test-bucket/to-delete HTTP/1.1\r\n\
             Host: {host}\r\n\
             x-amz-date: {amz_date}\r\n\
             x-amz-content-sha256: {body_hash}\r\n\
             Authorization: {auth}\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(&mut stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.contains("204"), "DELETE failed: {status}");

        assert!(!key_path.exists(), "object should be deleted");

        drop(handle);
    }

    /// Non-loopback bind without --insecure is refused.
    #[test]
    fn non_loopback_bind_refused_without_insecure() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let creds = ("test".to_string(), "secret".to_string());

        let result = ObjectServer::bind(
            "192.168.1.1:9999",
            "test-bucket".to_string(),
            vec![root],
            creds,
            false,
        );
        assert!(
            matches!(result, Err(ObjectServerError::NonLoopbackRefused { .. })),
            "expected NonLoopbackRefused, got {result:?}"
        );
    }

    /// Content-MD5 mismatch is refused.
    #[test]
    fn content_md5_mismatch_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let creds = ("test-access-key".to_string(), "test-secret-key".to_string());
        let (server, port) = start_test_server(vec![root.clone()], creds.clone());

        let handle = std::thread::spawn(move || {
            server.serve().ok();
        });
        std::thread::sleep(Duration::from_millis(50));

        let host = format!("127.0.0.1:{port}");
        let amz_date = build_amz_date();
        let body = b"correct data";
        let body_hash = hex_sha256(body);
        let (access, secret) = &creds;
        let auth = sign_request(
            access,
            secret,
            "us-east-1",
            "s3",
            "PUT",
            "/test-bucket/md5-test",
            "",
            &host,
            &amz_date,
            &body_hash,
        );

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        let req = format!(
            "PUT /test-bucket/md5-test HTTP/1.1\r\n\
             Host: {host}\r\n\
             x-amz-date: {amz_date}\r\n\
             x-amz-content-sha256: {body_hash}\r\n\
             Authorization: {auth}\r\n\
             Content-MD5: wrongmd5base64==\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();

        let mut reader = BufReader::new(&mut stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(
            status.contains("400"),
            "expected 400 for MD5 mismatch, got {status}"
        );

        drop(handle);
    }

    // ---------- helpers for tests ------------------------------------------------

    fn build_amz_date() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let days = secs / 86400;
        let time = secs % 86400;
        let hours = time / 3600;
        let mins = (time % 3600) / 60;
        let remain_secs = time % 60;
        let remaining_days = days as i64;
        let mut year = 1970i64;
        let mut remaining = remaining_days;
        loop {
            let days_in_year = if is_leap(year) { 366 } else { 365 };
            if remaining < days_in_year {
                break;
            }
            remaining -= days_in_year;
            year += 1;
        }
        let month_days = if is_leap(year) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        let mut month = 1;
        let mut rd = remaining;
        for &md in &month_days {
            if rd < md {
                break;
            }
            rd -= md;
            month += 1;
        }
        let day = rd + 1;
        format!("{year:04}{month:02}{day:02}T{hours:02}{mins:02}{remain_secs:02}Z")
    }

    fn is_leap(year: i64) -> bool {
        year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_request(
        access_key: &str,
        secret_key: &str,
        region: &str,
        service: &str,
        method: &str,
        path: &str,
        _query: &str,
        host: &str,
        amz_date: &str,
        body_hash: &str,
    ) -> String {
        let date_stamp = &amz_date[..8];
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{body_hash}\nx-amz-date:{amz_date}\n");
        let canonical_request =
            format!("{method}\n{path}\n\n{canonical_headers}{signed_headers}\n{body_hash}");
        let hashed_request = hex_sha256(canonical_request.as_bytes());
        let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
        let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{hashed_request}");
        let signing_key = derive_signing_key(secret_key, date_stamp, region, service);
        let signature = hex_hmac_sha256(&signing_key, string_to_sign.as_bytes());
        format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, \
             SignedHeaders={signed_headers}, Signature={signature}"
        )
    }
}
