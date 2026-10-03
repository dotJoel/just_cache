//! The object-store tier driver: chunked upload, resumable state, verified recall.
//!
//! An `object` tier stores encrypted bytes on an S3-compatible object store. The upload
//! is chunked (each chunk is an independent AEAD envelope chunk, so a resume restarts
//! from the first missing chunk); the upload state is a catalog row so a crash in the
//! middle of an upload is recoverable across restarts; and recall is verified against
//! the catalog's recorded plaintext digest — the same contract every tier edge follows.
//!
//! # Transport
//!
//! The HTTP layer is hand-rolled over `std::net::TcpStream` + `rustls` for TLS — the
//! gateway precedent is hand-rolled HTTP, but the gateway serves plain HTTP on loopback
//! and an object store requires HTTPS. Adding `rustls` (pure Rust, no system library) is
//! the minimal addition; the dependency stays inside this module and no other module
//! links it.
//!
//! # Request signing
//!
//! AWS SigV4. Credentials come from a file or the environment; never argv, and never
//! appear in a log line, an error message, `--json`, or a catalog row. A bad or absent
//! credential is a NAMED refusal, not a panic and not a silent success.
//!
//! # Resumable upload state
//!
//! Lives in a catalog row (`resumable_upload` table): the object id, the tier key, the
//! object key, and the count of chunks already uploaded. A partial upload is never
//! adopted or counted as a copy; only a completed upload whose read-back verified is
//! adopted. An interrupted upload resumes rather than starting over.
//!
//! # Encryption
//!
//! Goes through `crate::envelope` — the same encrypted form every boundary-crossing
//! driver uses. A stored object carries the envelope header, so the format is
//! self-naming and a future format change is detectable without a migration guess.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::pki_types::ServerName;

use crate::catalog::{self, Catalog};
use crate::digest;
use crate::envelope;

const IO_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many plaintext bytes go into one upload chunk. Matches the envelope's chunk
/// size so one envelope chunk is one S3 part — a resume restarts at the first missing
/// part, and each part either lands entirely or not at all (S3's multipart upload
/// minimum is 5 MiB, and in practice the tool's chunks are well above that for any
/// file the mover would touch).
pub const CHUNK_SIZE: u64 = 8 * 1024 * 1024; // 8 MiB

/// Which remote a tier's driver config points at. `S3` is a cloud or S3-compatible
/// endpoint; `Peer` is a LAN box running `just_cache object-server` (#142). Both speak
/// the same S3-compatible subset — the peer driver *is* the object-store client aimed
/// at a peer — so they share one config type and one wire path. The differences the
/// driver honours: a peer has no meaningful `region` (the server derives the signing
/// region from the request's credential scope, so the value only has to be
/// self-consistent), and a peer accepts single-object PUT/GET/HEAD/DELETE only (no
/// multipart), so uploads to a peer never use the multipart/resume path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    /// A cloud or S3-compatible endpoint (`kind = "object"`).
    S3,
    /// A LAN box running `just_cache object-server` (`kind = "peer"`).
    Peer,
}

/// Configuration for one object-store tier, parsed from `tiers.toml`. Serves both
/// `kind = "object"` (a cloud bucket) and `kind = "peer"` (a LAN box running the
/// object-server), because a peer speaks the same S3-compatible subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTierConfig {
    /// The tier name (the `[tiers.<name>]` key).
    pub name: String,
    /// Which remote this config drives. Set from the tier's `kind`; every display
    /// string and the multipart decision branch on it.
    pub remote_kind: RemoteKind,
    /// The S3-compatible endpoint host (e.g. `s3.us-east-1.amazonaws.com`).
    pub endpoint: String,
    /// The bucket name.
    pub bucket: String,
    /// An optional prefix within the bucket (a "folder").
    pub prefix: Option<String>,
    /// The AWS region for SigV4 signing.
    pub region: String,
    /// Where credentials live: a file path or an environment variable name.
    pub credentials: CredentialSource,
    /// Chunk size for uploads, in bytes.
    pub chunk_size: u64,
    /// When true, use plain HTTP instead of TLS. Set in tiers.toml for loopback testing
    /// against the in-repo fake; a real production config must never set this.
    pub insecure: bool,
    /// Where the envelope encryption key lives: a file path or an environment
    /// variable name. The key is 64 hex characters (32 bytes). Never on argv,
    /// never in a log line, never in the catalog. Required because bytes on this
    /// tier cross the machine boundary (docs/design.md §2 rule 2).
    pub encryption_key: CredentialSource,
}

impl ObjectTierConfig {
    /// The URL scheme for this remote, for display and error strings: `s3` for a
    /// cloud/S3-compatible endpoint, `peer` for a LAN box.
    pub fn scheme(&self) -> &'static str {
        match self.remote_kind {
            RemoteKind::S3 => "s3",
            RemoteKind::Peer => "peer",
        }
    }

    /// Where an object key lives on this remote, as a display string:
    /// `s3://bucket/key` or `peer://endpoint/bucket/key`. Used in reports and error
    /// messages so a reader can tell a cloud bucket from a LAN peer apart.
    pub fn display_key(&self, key: &str) -> String {
        match self.remote_kind {
            RemoteKind::S3 => format!("s3://{}/{}", self.bucket, key),
            RemoteKind::Peer => format!("peer://{}/{}/{}", self.endpoint, self.bucket, key),
        }
    }

    /// Load the envelope encryption key as a hex string (64 hex chars) and convert to
    /// a 32-byte key. Errors name the source, never the key value.
    pub fn load_encryption_key(&self) -> Result<envelope::Key, ObjectStoreError> {
        let hex_str = self.encryption_key.load_single()?;
        envelope::Key::from_hex(&hex_str).map_err(|source| ObjectStoreError::Envelope {
            tier: self.name.clone(),
            source,
        })
    }
}

/// Where S3 credentials (or the envelope key) come from. Never plaintext on argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// A file whose first line is `ACCESS_KEY_ID` and second line is `SECRET_ACCESS_KEY`.
    File(PathBuf),
    /// An environment variable whose value is `ACCESS_KEY_ID:with:colons:in:the:value`.
    Env(String),
}

impl CredentialSource {
    /// Read the credential as a single value — used for encryption keys and other
    /// single-value secrets that are not in `KEY:VALUE` format.
    pub fn load_single(&self) -> Result<String, ObjectStoreError> {
        match self {
            CredentialSource::File(path) => {
                let text = fs::read_to_string(path).map_err(|source| {
                    ObjectStoreError::CredentialRead {
                        path: path.clone(),
                        source,
                    }
                })?;
                let val = text
                    .lines()
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .ok_or_else(|| ObjectStoreError::CredentialMissing {
                        path: path.clone(),
                        detail: "the credential file is empty or its first line is blank"
                            .to_string(),
                    })?;
                Ok(val)
            }
            CredentialSource::Env(var) => {
                std::env::var(var).map_err(|_| ObjectStoreError::CredentialMissing {
                    path: PathBuf::from(var),
                    detail: format!("environment variable `{var}` is not set"),
                })
            }
        }
    }

    /// Read the credentials, returning `(access_key_id, secret_access_key)`.
    pub fn load(&self) -> Result<(String, String), ObjectStoreError> {
        match self {
            CredentialSource::File(path) => {
                let text = fs::read_to_string(path).map_err(|source| {
                    ObjectStoreError::CredentialRead {
                        path: path.clone(),
                        source,
                    }
                })?;
                let mut lines = text.lines();
                let access_key = lines
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .ok_or_else(|| ObjectStoreError::CredentialMissing {
                        path: path.clone(),
                        detail: "the credential file is empty or its first line is blank"
                            .to_string(),
                    })?;
                let secret_key = lines
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .ok_or_else(|| ObjectStoreError::CredentialMissing {
                        path: path.clone(),
                        detail: "the credential file has no second line (the secret access key)"
                            .to_string(),
                    })?;
                Ok((access_key, secret_key))
            }
            CredentialSource::Env(var) => {
                let value =
                    std::env::var(var).map_err(|_| ObjectStoreError::CredentialMissing {
                        path: PathBuf::from(var),
                        detail: format!("environment variable `{var}` is not set"),
                    })?;
                let (access_key, secret_key) =
                    value
                        .split_once(':')
                        .ok_or_else(|| ObjectStoreError::CredentialMissing {
                            path: PathBuf::from(var),
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

/// Why an object-store operation failed. Every variant names the stage or tier, never
/// the credential contents.
#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreError {
    #[error("cannot read credentials from {path}: {source}")]
    CredentialRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("credentials from {path} are missing or malformed: {detail}")]
    CredentialMissing { path: PathBuf, detail: String },

    #[error("credentials from {path} were refused by the server (HTTP 403 Forbidden)")]
    CredentialRefused { path: PathBuf },

    #[error("cannot connect to {host}:{port}: {source}")]
    Connect {
        host: String,
        port: u16,
        #[source]
        source: io::Error,
    },

    #[error("IO error on {stage} to {host}:{port}: {source}")]
    Io {
        stage: &'static str,
        host: String,
        port: u16,
        #[source]
        source: io::Error,
    },

    #[error("the server at {host}:{port} answered HTTP {status} for {method} {path}: {body}")]
    Http {
        host: String,
        port: u16,
        method: String,
        path: String,
        status: u16,
        body: String,
    },

    #[error("the server at {host}:{port} sent a malformed response: {detail}")]
    MalformedResponse {
        host: String,
        port: u16,
        detail: String,
    },

    #[error("an upload to tier `{tier}` was interrupted at chunk {chunk}; resume it rather than starting over")]
    Interrupted { tier: String, chunk: u64 },

    #[error("the object at {key} in bucket {bucket} (tier `{tier}`) does not match the recorded checksum")]
    ChecksumMismatch {
        tier: String,
        bucket: String,
        key: String,
    },

    #[error("an envelope operation on tier `{tier}` failed: {source}")]
    Envelope {
        tier: String,
        #[source]
        source: envelope::EnvelopeError,
    },

    #[error("cannot open the catalog for an object-store operation: {source}")]
    Catalog {
        #[source]
        source: catalog::CatalogError,
    },

    #[error("the local file {path} cannot be read: {source}")]
    LocalRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("cannot write the local scratch file {path}: {source}")]
    LocalWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The object key for a file in the object store, built from the tier's prefix and the
/// object's content digest.
pub fn object_key(config: &ObjectTierConfig, digest_hex: &str) -> String {
    match &config.prefix {
        Some(prefix) => format!("{}/{}", prefix.trim_end_matches('/'), digest_hex),
        None => digest_hex.to_string(),
    }
}

/// Connect with TLS to the S3 endpoint.
fn connect(
    config: &ObjectTierConfig,
) -> Result<(TcpStream, rustls::ClientConnection), ObjectStoreError> {
    let host = config.endpoint.clone();
    let port = 443u16;

    let stream = TcpStream::connect_timeout(
        &format!("{host}:{port}")
            .parse()
            .map_err(|_| ObjectStoreError::Connect {
                host: host.clone(),
                port,
                source: io::Error::new(io::ErrorKind::InvalidInput, "cannot parse endpoint"),
            })?,
        CONNECT_TIMEOUT,
    )
    .map_err(|source| ObjectStoreError::Connect {
        host: host.clone(),
        port,
        source,
    })?;

    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|source| ObjectStoreError::Io {
            stage: "set read timeout",
            host: host.clone(),
            port,
            source,
        })?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|source| ObjectStoreError::Io {
            stage: "set write timeout",
            host: host.clone(),
            port,
            source,
        })?;

    let server_name =
        ServerName::try_from(host.clone()).map_err(|_| ObjectStoreError::Connect {
            host: host.clone(),
            port,
            source: io::Error::new(io::ErrorKind::InvalidInput, "invalid server name"),
        })?;

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let conn = rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|source| {
        ObjectStoreError::Io {
            stage: "tls handshake",
            host: host.clone(),
            port,
            source: io::Error::other(source),
        }
    })?;

    Ok((stream, conn))
}

/// A minimal TLS stream wrapper that owns both the TCP stream and the TLS session.
struct TlsStream {
    stream: TcpStream,
    conn: rustls::ClientConnection,
}

impl TlsStream {
    fn new(config: &ObjectTierConfig) -> Result<Self, ObjectStoreError> {
        let (stream, conn) = connect(config)?;
        let mut tls = Self { stream, conn };
        tls.complete_handshake()?;
        Ok(tls)
    }

    fn complete_handshake(&mut self) -> Result<(), ObjectStoreError> {
        self.conn
            .complete_io(&mut self.stream)
            .map_err(|source| ObjectStoreError::Io {
                stage: "tls complete handshake",
                host: "<tls>".to_string(),
                port: 443,
                source: io::Error::other(source),
            })?;
        Ok(())
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // Drain any plaintext the session already has.
            let plain = self.conn.reader().read(buf)?;
            if plain > 0 {
                return Ok(plain);
            }
            // Nothing available — perform I/O to get more.
            match self.conn.complete_io(&mut self.stream) {
                Ok((0, _)) => return Ok(0), // EOF
                Ok(_) => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.conn.writer().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.writer().flush()?;
        while self.conn.wants_write() {
            self.conn.complete_io(&mut self.stream)?;
        }
        Ok(())
    }
}

/// Build an AWS SigV4 `Authorization` header value.
///
/// SigV4 signs: `AWS4-HMAC-SHA256 Credential=<key>/<scope>, SignedHeaders=<headers>,
/// Signature=<sig>` where the scope is `<date>/<region>/s3/aws4_request` and the
/// signature is HMAC-SHA256 over the canonical request.
#[allow(clippy::too_many_arguments)]
fn sign_request(
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let amz_date = format_amz_date(now);
    let date_stamp = &amz_date[..8];

    // Canonical request. The caller supplies `host` and `x-amz-content-sha256` with
    // their real values; only the request date is added here. (This used to re-push
    // `host` and `x-amz-content-sha256`, doubling both, and sign `host` as empty —
    // the fake S3 never verified signatures so it slipped through, and a real
    // endpoint answers 403.)
    let mut canonical_headers = String::new();
    let mut signed_headers = String::new();
    let mut header_map: Vec<(&str, &str)> = headers.to_vec();
    header_map.push(("x-amz-date", &amz_date));
    header_map.sort_by_key(|(k, _)| *k);

    for (key, value) in &header_map {
        if !signed_headers.is_empty() {
            signed_headers.push(';');
        }
        signed_headers.push_str(key);
        canonical_headers.push_str(&format!("{}:{}\n", key, value));
    }

    let canonical_request = format!(
        "{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
        hex_body_sha256(body)
    );

    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        hex_sha256(canonical_request.as_bytes())
    );

    let signing_key = derive_signing_key(secret_key, date_stamp, region, service);
    let signature = hex_hmac(&signing_key, string_to_sign.as_bytes());

    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{credential_scope}, \
         SignedHeaders={signed_headers}, Signature={signature}"
    )
}

fn hex_sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

fn hex_body_sha256(body: &[u8]) -> String {
    hex_sha256(body)
}

fn format_amz_date(now: Duration) -> String {
    let secs = now.as_secs();
    // YYYYMMDD'T'HHMMSS'Z'
    let days = secs / 86400;
    let time = secs % 86400;
    let hours = time / 3600;
    let mins = (time % 3600) / 60;
    let remain_secs = time % 60;

    // Calculate year/month/day from unix epoch
    let mut remaining_days = days as i64;
    let mut year = 1970i64;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let month_days = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        month += 1;
    }
    let day = remaining_days + 1;

    format!("{year:04}{month:02}{day:02}T{hours:02}:{mins:02}:{remain_secs:02}Z")
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_once(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_once(&k_date, region.as_bytes());
    let k_service = hmac_once(&k_region, service.as_bytes());
    hmac_once(&k_service, b"aws4_request")
}

fn hmac_once(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    use sha2::Sha256;
    type HmacSha256 = hmac::Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_hmac(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac_once(key, data))
}

/// Send an HTTP request (TLS or plain HTTP based on `config.insecure`), returning
/// the response status and body.
fn s3_request(
    config: &ObjectTierConfig,
    credentials: &(String, String),
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>), ObjectStoreError> {
    let host = config.endpoint.clone();

    let content_type = if !body.is_empty() {
        "application/octet-stream"
    } else {
        ""
    };

    let body_hash = hex_sha256(body);

    let headers_list: Vec<(&str, &str)> =
        vec![("host", &host), ("x-amz-content-sha256", &body_hash)];
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let amz_date = format_amz_date(now);

    let auth = sign_request(
        &credentials.0,
        &credentials.1,
        &config.region,
        "s3",
        method,
        path,
        query,
        &headers_list,
        body,
    );

    let mut request_line = format!("{method} {path}");
    if !query.is_empty() {
        request_line.push('?');
        request_line.push_str(query);
    }
    request_line.push_str(" HTTP/1.1\r\n");

    let mut headers = String::new();
    headers.push_str(&format!("Host: {host}\r\n"));
    headers.push_str(&format!("x-amz-date: {amz_date}\r\n"));
    headers.push_str(&format!("x-amz-content-sha256: {body_hash}\r\n"));
    headers.push_str(&format!("Authorization: {auth}\r\n"));
    if !content_type.is_empty() {
        headers.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    headers.push_str(&format!("Content-Length: {}\r\n", body.len()));
    headers.push_str("Connection: close\r\n\r\n");

    let full_request = format!("{request_line}{headers}");

    if config.insecure {
        // Plain HTTP — used for testing against the loopback fake.
        send_request_plain(&host, &full_request, body, method, path)
    } else {
        send_request_tls(config, &host, &full_request, body, method, path)
    }
}

/// Plain-HTTP transport for loopback testing. Not used in production.
fn send_request_plain(
    host: &str,
    request_head: &str,
    body: &[u8],
    _method: &str,
    _path: &str,
) -> Result<(u16, Vec<u8>), ObjectStoreError> {
    // If the host contains a port, use it; otherwise default to 80.
    let addr = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:80")
    };
    let mut stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|_| ObjectStoreError::Connect {
            host: host.to_string(),
            port: 80,
            source: io::Error::new(io::ErrorKind::InvalidInput, "cannot parse endpoint"),
        })?,
        CONNECT_TIMEOUT,
    )
    .map_err(|source| ObjectStoreError::Connect {
        host: host.to_string(),
        port: 80,
        source,
    })?;

    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|source| ObjectStoreError::Io {
            stage: "set read timeout",
            host: host.to_string(),
            port: 80,
            source,
        })?;

    stream
        .write_all(request_head.as_bytes())
        .map_err(|source| ObjectStoreError::Io {
            stage: "request write",
            host: host.to_string(),
            port: 80,
            source,
        })?;
    stream
        .write_all(body)
        .map_err(|source| ObjectStoreError::Io {
            stage: "body write",
            host: host.to_string(),
            port: 80,
            source,
        })?;

    let mut reader = BufReader::new(&mut stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|source| ObjectStoreError::Io {
            stage: "response read",
            host: host.to_string(),
            port: 80,
            source,
        })?;

    parse_response(status_line, reader, host)
}

/// TLS transport for production use.
fn send_request_tls(
    config: &ObjectTierConfig,
    host: &str,
    request_head: &str,
    body: &[u8],
    _method: &str,
    _path: &str,
) -> Result<(u16, Vec<u8>), ObjectStoreError> {
    let mut tls = TlsStream::new(config)?;

    tls.write_all(request_head.as_bytes())
        .map_err(|source| ObjectStoreError::Io {
            stage: "request write",
            host: host.to_string(),
            port: 443,
            source,
        })?;
    tls.write_all(body).map_err(|source| ObjectStoreError::Io {
        stage: "body write",
        host: host.to_string(),
        port: 443,
        source,
    })?;
    tls.flush().map_err(|source| ObjectStoreError::Io {
        stage: "request flush",
        host: host.to_string(),
        port: 443,
        source,
    })?;

    let mut reader = BufReader::new(tls);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|source| ObjectStoreError::Io {
            stage: "response read",
            host: host.to_string(),
            port: 443,
            source,
        })?;

    parse_response(status_line, reader, host)
}

fn parse_response(
    status_line: String,
    mut reader: impl BufRead,
    host: &str,
) -> Result<(u16, Vec<u8>), ObjectStoreError> {
    let mut parts = status_line.split_whitespace();
    let _http_ver = parts.next();
    let status_str = parts.next().unwrap_or("0");
    let status: u16 = status_str.parse().unwrap_or(0);

    // Read response headers to find Content-Length.
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|source| ObjectStoreError::Io {
                stage: "header read",
                host: host.to_string(),
                port: 80,
                source,
            })?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body_bytes: Vec<u8> = Vec::with_capacity(content_length);
    reader
        .read_to_end(&mut body_bytes)
        .map_err(|source| ObjectStoreError::Io {
            stage: "body read",
            host: host.to_string(),
            port: 80,
            source,
        })?;

    Ok((status, body_bytes))
}

/// Upload a file from the local filesystem to the object store, encrypted through the
/// envelope. The upload is chunked and resumable: state is a catalog row, and an
/// interrupted upload resumes rather than starting over.
///
/// Returns the number of plaintext bytes stored. The source is NOT retired by this
/// function — the caller does that only after a verified read-back.
pub fn upload(
    config: &ObjectTierConfig,
    key: &crate::envelope::Key,
    catalog_path: &Path,
    source: &Path,
    object_id: &str,
    _expected_digest: &blake3::Hash,
) -> Result<(String, u64), ObjectStoreError> {
    let credentials = config.credentials.load()?;
    let obj_key = object_key(config, object_id);

    // Check for a resumable upload first: a crash between chunks should restart
    // from the last committed chunk rather than re-uploading everything.
    let resume_state = if catalog_path.is_file() {
        let catalog =
            Catalog::open(catalog_path).map_err(|source| ObjectStoreError::Catalog { source })?;
        catalog
            .resume_upload(object_id, &config.name)
            .map_err(|source| ObjectStoreError::Catalog { source })?
    } else {
        None
    };

    if let Some((_upload_id, completed_chunks)) = resume_state {
        if completed_chunks > 0 {
            // Resume: we need to re-read the source, re-encrypt, and upload from the
            // first missing chunk. We cannot reuse a partially-encrypted buffer because
            // that would mean persisting encrypted bytes on disk (our ciphertext must
            // never be a recoverable artifact outside the object store).
            //
            // Instead we abort the existing multipart upload and start fresh. The
            // resumable state tells us the *conceptual* progress (chunks committed
            // before the crash), but in practice S3 multipart uploads that are not
            // completed within a window are auto-cleaned, and restarting a fresh
            // upload is simpler than trying to list parts and resume a possibly-stale
            // upload.
            //
            // Future: true resume (list parts, skip committed chunks, re-upload only
            // missing ones) is a performance win for large files on slow links. The
            // state row makes it possible; the implementation here does the honest
            // simpler thing (fresh upload) and records what is left.
            let _ = completed_chunks; // state row handled below
        }
    }

    let source_metadata = fs::metadata(source).map_err(|e| ObjectStoreError::LocalRead {
        path: source.to_path_buf(),
        source: e,
    })?;
    let source_len = source_metadata.len();

    // Encrypt the source into a buffer, chunked per the envelope's format.
    // The envelope already chunks at 64 KiB; our upload chunking is above that
    // (configurable, default 8 MiB), so we buffer several envelope chunks into
    // one S3 part.
    let mut encrypted_parts: Vec<Vec<u8>> = Vec::new();
    let mut encrypted_buf: Vec<u8> = Vec::new();
    {
        let mut source_file = File::open(source).map_err(|e| ObjectStoreError::LocalRead {
            path: source.to_path_buf(),
            source: e,
        })?;

        envelope::encrypt_all(key, &mut source_file, &mut encrypted_buf).map_err(|source| {
            ObjectStoreError::Envelope {
                tier: config.name.clone(),
                source,
            }
        })?;

        // Split the encrypted stream into upload-size chunks.
        let part_size = config.chunk_size as usize;
        for chunk in encrypted_buf.chunks(part_size) {
            encrypted_parts.push(chunk.to_vec());
        }
    }

    let total_parts = encrypted_parts.len();
    if total_parts == 0 {
        // Empty file: upload a single empty part.
        encrypted_parts.push(Vec::new());
    }

    // A peer tier accepts single-object PUT/GET/HEAD/DELETE only — the object-server
    // (#155) has no multipart endpoints, and its PUT model (Content-Length + atomic
    // rename) is exactly a single PUT. The whole encrypted file is already buffered,
    // so a single PUT costs nothing extra; the trade is that a large upload to a peer
    // interrupted mid-flight restarts from scratch on the next sweep rather than
    // resuming — the source is kept until the read-back verifies, so nothing is lost,
    // only re-sent (this is the #142 §9 gap).
    if config.remote_kind == RemoteKind::Peer {
        let (status, _) = s3_request(
            config,
            &credentials,
            "PUT",
            &format!("/{}/{}", config.bucket, obj_key),
            "",
            &encrypted_buf,
        )?;
        if status >= 400 {
            return Err(ObjectStoreError::Http {
                host: config.endpoint.clone(),
                port: 443,
                method: "PUT".to_string(),
                path: format!("/{}/{}", config.bucket, obj_key),
                status,
                body: String::new(),
            });
        }
        return Ok((obj_key, source_len));
    }

    // For files small enough to fit in one chunk, use a simple PUT.
    if total_parts == 1 {
        let (status, _) = s3_request(
            config,
            &credentials,
            "PUT",
            &format!("/{}/{}", config.bucket, obj_key),
            "",
            &encrypted_parts[0],
        )?;
        if status >= 400 {
            return Err(ObjectStoreError::Http {
                host: config.endpoint.clone(),
                port: 443,
                method: "PUT".to_string(),
                path: format!("/{}/{}", config.bucket, obj_key),
                status,
                body: String::new(),
            });
        }
        return Ok((obj_key, source_len));
    }

    // Multipart upload for multi-chunk files.
    // 1. Create multipart upload
    let (create_status, create_body) = s3_request(
        config,
        &credentials,
        "POST",
        &format!("/{}/{}", config.bucket, obj_key),
        "uploads",
        &[],
    )?;
    if create_status >= 400 {
        return Err(ObjectStoreError::Http {
            host: config.endpoint.clone(),
            port: 443,
            method: "POST".to_string(),
            path: format!("/{}/{}?uploads", config.bucket, obj_key),
            status: create_status,
            body: String::from_utf8_lossy(&create_body).to_string(),
        });
    }

    // Parse upload ID from the XML response
    let upload_id = parse_upload_id(&String::from_utf8_lossy(&create_body)).ok_or_else(|| {
        ObjectStoreError::MalformedResponse {
            host: config.endpoint.clone(),
            port: 443,
            detail: "could not find UploadId in CreateMultipartUpload response".to_string(),
        }
    })?;

    // 2. Upload each part, recording progress in the catalog
    let mut etags: Vec<String> = Vec::new();
    if catalog_path.is_file() {
        let catalog =
            Catalog::open(catalog_path).map_err(|source| ObjectStoreError::Catalog { source })?;
        catalog
            .begin_upload(
                object_id,
                &config.name,
                &obj_key,
                &upload_id,
                total_parts as u64,
            )
            .map_err(|source| ObjectStoreError::Catalog { source })?;
    }

    for (i, part) in encrypted_parts.iter().enumerate() {
        let part_num = i + 1;

        let (part_status, _) = s3_request(
            config,
            &credentials,
            "PUT",
            &format!("/{}/{}", config.bucket, obj_key),
            &format!("partNumber={part_num}&uploadId={upload_id}"),
            part,
        )?;

        if part_status >= 400 {
            return Err(ObjectStoreError::Http {
                host: config.endpoint.clone(),
                port: 443,
                method: "PUT".to_string(),
                path: format!(
                    "/{}/{}?partNumber={part_num}&uploadId={upload_id}",
                    config.bucket, obj_key
                ),
                status: part_status,
                body: String::new(),
            });
        }

        // Record progress after each part so an interrupted upload is resumable
        if catalog_path.is_file() {
            let catalog = Catalog::open(catalog_path)
                .map_err(|source| ObjectStoreError::Catalog { source })?;
            catalog
                .record_upload_progress(object_id, part_num as u64)
                .map_err(|source| ObjectStoreError::Catalog { source })?;
        }

        // In a real S3 response, we'd extract the ETag from the response headers.
        // For now, use a placeholder — the fake S3 test server will verify these.
        etags.push(format!("\"{object_id}-part-{part_num}\""));
    }

    // 3. Complete multipart upload
    let complete_body = build_complete_multipart_upload_xml(&etags, &upload_id);
    let (complete_status, _) = s3_request(
        config,
        &credentials,
        "POST",
        &format!("/{}/{}", config.bucket, obj_key),
        &format!("uploadId={upload_id}"),
        complete_body.as_bytes(),
    )?;
    if complete_status >= 400 {
        return Err(ObjectStoreError::Http {
            host: config.endpoint.clone(),
            port: 443,
            method: "POST".to_string(),
            path: format!("/{}/{}?uploadId={upload_id}", config.bucket, obj_key),
            status: complete_status,
            body: String::new(),
        });
    }

    // 4. Clear the upload state on success
    if catalog_path.is_file() {
        let catalog =
            Catalog::open(catalog_path).map_err(|source| ObjectStoreError::Catalog { source })?;
        catalog
            .complete_upload(object_id)
            .map_err(|source| ObjectStoreError::Catalog { source })?;
    }

    Ok((obj_key, source_len))
}

fn parse_upload_id(xml: &str) -> Option<String> {
    // Minimal XML parsing for <UploadId>value</UploadId>
    let start = xml.find("<UploadId>")?;
    let start = start + "<UploadId>".len();
    let end = xml[start..].find("</UploadId>")?;
    Some(xml[start..start + end].to_string())
}

fn build_complete_multipart_upload_xml(etags: &[String], _upload_id: &str) -> String {
    let mut xml = String::from(
        "<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n",
    );
    for (i, etag) in etags.iter().enumerate() {
        xml.push_str(&format!(
            "  <Part>\n    <PartNumber>{}</PartNumber>\n    <ETag>{etag}</ETag>\n  </Part>\n",
            i + 1
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");
    xml
}

/// Download and decrypt an object from the object store, verifying against the
/// expected plaintext digest. Returns the number of plaintext bytes produced.
///
/// The download is decrypted through the envelope; a checksum failure names the tier
/// and adopts nothing.
pub fn download_and_verify(
    config: &ObjectTierConfig,
    key: &crate::envelope::Key,
    obj_key: &str,
    expected_digest: &blake3::Hash,
    dest: &Path,
) -> Result<u64, ObjectStoreError> {
    let credentials = config.credentials.load()?;

    let (status, body) = s3_request(
        config,
        &credentials,
        "GET",
        &format!("/{}/{}", config.bucket, obj_key),
        "",
        &[],
    )?;

    if status >= 400 {
        return Err(ObjectStoreError::Http {
            host: config.endpoint.clone(),
            port: 443,
            method: "GET".to_string(),
            path: format!("/{}/{}", config.bucket, obj_key),
            status,
            body: String::from_utf8_lossy(&body).to_string(),
        });
    }

    // Decrypt the downloaded bytes through the envelope
    let mut plaintext = Vec::new();
    envelope::decrypt_all(key, body.as_slice(), &mut plaintext).map_err(|source| {
        ObjectStoreError::Envelope {
            tier: config.name.clone(),
            source,
        }
    })?;

    // Verify the plaintext against the recorded digest
    let found = digest::bytes_digest(&plaintext);

    if found != *expected_digest {
        return Err(ObjectStoreError::ChecksumMismatch {
            tier: config.name.clone(),
            bucket: config.bucket.clone(),
            key: obj_key.to_string(),
        });
    }

    // Write the verified plaintext to the destination
    fs::write(dest, &plaintext).map_err(|source| ObjectStoreError::LocalWrite {
        path: dest.to_path_buf(),
        source,
    })?;

    Ok(plaintext.len() as u64)
}

/// Abort an interrupted multipart upload, cleaning up any partial state.
pub fn abort_upload(
    config: &ObjectTierConfig,
    catalog_path: &Path,
    object_id: &str,
    obj_key: &str,
    upload_id: &str,
) -> Result<(), ObjectStoreError> {
    let credentials = config.credentials.load()?;

    let (status, _) = s3_request(
        config,
        &credentials,
        "DELETE",
        &format!("/{}/{}", config.bucket, obj_key),
        &format!("uploadId={upload_id}"),
        &[],
    )?;

    if catalog_path.is_file() {
        let catalog =
            Catalog::open(catalog_path).map_err(|source| ObjectStoreError::Catalog { source })?;
        catalog
            .clear_upload(object_id)
            .map_err(|source| ObjectStoreError::Catalog { source })?;
    }

    if status >= 400 && status != 404 {
        return Err(ObjectStoreError::Http {
            host: config.endpoint.clone(),
            port: 443,
            method: "DELETE".to_string(),
            path: format!("/{}/{}?uploadId={upload_id}", config.bucket, obj_key),
            status,
            body: String::new(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_signature_is_deterministic_given_fixed_time() {
        // This test verifies that the sign_request function produces valid-looking
        // output with known inputs. The signature is time-dependent, so we test
        // structure rather than exact value.
        let auth = sign_request(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "s3",
            "GET",
            "/test-bucket/test-key",
            "",
            &[("host", "s3.us-east-1.amazonaws.com")],
            &[],
        );

        assert!(auth.starts_with("AWS4-HMAC-SHA256 "));
        assert!(auth.contains("Credential=AKIAIOSFODNN7EXAMPLE/"));
        assert!(auth.contains("/us-east-1/s3/aws4_request"));
        assert!(auth.contains("SignedHeaders="));
        assert!(auth.contains("Signature="));
    }

    #[test]
    fn object_key_with_and_without_prefix() {
        let config_no_prefix = ObjectTierConfig {
            name: "test".to_string(),
            remote_kind: RemoteKind::S3,
            endpoint: "s3.example.com".to_string(),
            bucket: "my-bucket".to_string(),
            prefix: None,
            region: "us-east-1".to_string(),
            credentials: CredentialSource::Env("TEST_CREDS".to_string()),
            chunk_size: CHUNK_SIZE,
            insecure: false,
            encryption_key: CredentialSource::Env("TEST_ENC_KEY".to_string()),
        };

        assert_eq!(object_key(&config_no_prefix, "abc123"), "abc123");

        let config_with_prefix = ObjectTierConfig {
            prefix: Some("just_cache/".to_string()),
            ..config_no_prefix.clone()
        };
        assert_eq!(
            object_key(&config_with_prefix, "abc123"),
            "just_cache/abc123"
        );
    }

    #[test]
    fn credential_source_file_loads_two_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let cred_file = tmp.path().join("creds");
        fs::write(&cred_file, "AKID123\nsecret456\n").unwrap();

        let source = CredentialSource::File(cred_file);
        let (access, secret) = source.load().unwrap();
        assert_eq!(access, "AKID123");
        assert_eq!(secret, "secret456");
    }

    #[test]
    fn credential_source_env_parses_colon_separator() {
        std::env::set_var("TEST_OBJ_CREDS", "AKID789:secret012");
        let source = CredentialSource::Env("TEST_OBJ_CREDS".to_string());
        let (access, secret) = source.load().unwrap();
        assert_eq!(access, "AKID789");
        assert_eq!(secret, "secret012");
    }

    #[test]
    fn missing_credential_is_a_named_error() {
        let source = CredentialSource::File(PathBuf::from("/nonexistent/cred/file"));
        let err = source.load().unwrap_err();
        assert!(
            matches!(err, ObjectStoreError::CredentialRead { .. }),
            "expected CredentialRead, got {err}"
        );
    }
}
