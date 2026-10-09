//! Optional webhook notifications (issue #186): a short summary of a scheduled
//! maintenance run, and the findings of an `audit`, posted to a Discord-compatible
//! webhook when — and only when — an endpoint is configured.
//!
//! # Off by default, and quiet when off
//!
//! The endpoint lives in its own small file, `notify.toml`, beside the catalog. It is
//! **opened only when present and never created** — the same posture `tiers.toml`,
//! `policy.toml` and `schedule.toml` have (invariant 9). A run with no `notify.toml`
//! opens no socket at all: there is no endpoint to dial, so none is dialled. This is a
//! stronger guarantee than \"the send is skipped\": the homes of the notify call are
//! reached only through a loaded [`NotifyConfig`], and loading returns `None` when the
//! file is absent.
//!
//! A file that *is* there but malformed is a refusal naming its line, exactly like a
//! broken `schedule.toml` — a config the operator asked for and the tool cannot honour
//! is an error, not a silent fallback to \"notifications off\". This is the one failure
//! that is fatal: it is an operator mistake at config time (like a mistyped cadence),
//! whereas a *delivery* failure is a runtime network condition and is only ever a
//! warning (see below).
//!
//! # The URL is a credential
//!
//! Anyone holding the webhook URL can post to the channel as the operator, so the URL
//! is never printed, never logged, and never placed in an error message or a report —
//! not even by a config error, which names the line instead. Deliberately nothing here
//! stores the raw URL after parsing: [`NotifyConfig`] keeps only the parsed target, so
//! there is no field to leak by accident. At most a delivery warning names the **host**.
//!
//! # Delivery never blocks the pass
//!
//! Every failure — an unreachable endpoint, a TLS failure, a non-2xx status, or a
//! malformed response — is a warning the caller prints on stderr. None is a nonzero
//! exit, none fails the maintenance pass, and there is no retry loop that could stall a
//! run: one POST, one bounded wait, done. The connect, write and read are each bounded
//! by [`NOTIFY_TIMEOUT`].
//!
//! # Transport: no new dependency
//!
//! The POST is hand-rolled over `std::net::TcpStream` + `rustls`, mirroring
//! `src/object_store.rs` — the one existing HTTPS client. The transport there is a
//! private `TlsStream` bound to the S3 tier config (its endpoint, its `insecure` flag,
//! its signing) and is not reachable without dragging S3 signing along, so this module
//! mirrors the pattern rather than reusing it. The JSON body is hand-built in the house
//! style (every `--json` writer is a `String` built by hand), reusing
//! [`crate::events::json_string`] so anything interpolated is escaped the same way as
//! every other document this tool emits.
//!
//! Plain `http://` is accepted **only for a loopback host**, where it exists so tests can
//! drive the failure paths against a `std::net::TcpListener`; a non-loopback plain-http
//! URL is refused at parse time, because posting a credential in the clear is the one
//! thing the credential-is-a-secret rule must not allow.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use serde::Deserialize;
use thiserror::Error;

/// The default config file, beside the catalog — the same placement every other concern
/// uses. Absent means notifications are off.
pub const NOTIFY_FILE_NAME: &str = "notify.toml";

/// The bound on every network step of one notification: the connect, each write, and the
/// read of the response.
///
/// One webhook POST carries a handful of lines, so 5 seconds is generous for a real
/// delivery and small enough that a hung endpoint cannot stall a maintenance pass for
/// long. It is deliberately below `object_store`'s 10 s connect / 30 s I/O budgets,
/// which cover multi-megabyte transfers a notification never has.
pub const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a `notify.toml` could not be read or trusted. Mirrors
/// [`crate::schedule::ScheduleError`].
#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("cannot read the notify config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("malformed notify config {path}: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// A syntactically valid file that says something impossible, naming the line.
    #[error("invalid notify config {path}: {detail}")]
    Invalid { path: PathBuf, detail: String },
}

/// Why one notification could not be delivered. Every variant is a warning to the caller,
/// never an error that stops a pass, and none carries the URL — only the host, or the
/// status the endpoint answered.
#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("cannot reach the notification endpoint {host}: {source}")]
    Connect {
        host: String,
        #[source]
        source: io::Error,
    },
    #[error("the notification endpoint address {host} cannot be resolved")]
    Resolve { host: String },
    #[error("TLS handshake with the notification endpoint {host} failed: {detail}")]
    Tls { host: String, detail: String },
    #[error("could not send the notification to {host}: {source}")]
    Write {
        host: String,
        #[source]
        source: io::Error,
    },
    #[error("could not read the notification response from {host}: {source}")]
    Read {
        host: String,
        #[source]
        source: io::Error,
    },
    #[error("the notification endpoint {host} answered HTTP {status}")]
    Status { host: String, status: u16 },
    #[error("the notification endpoint {host} returned a malformed response")]
    Malformed { host: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Https,
    Http,
}

/// A parsed webhook endpoint: enough to make one request, and nothing that echoes the
/// credential back. The URL's path carries the webhook secret, so it is kept here only
/// to be sent and is never rendered by `Debug` in a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
}

impl Target {
    /// The authority as it belongs in a `Host:` header (a bare IPv6 literal is bracketed).
    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }
}

/// A loaded `notify.toml`: the endpoint to post to. Nothing else — the message content is
/// built by the pure functions below from counts the caller already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    target: Target,
}

impl NotifyConfig {
    /// The default file beside a directory.
    pub fn default_path(dir: &Path) -> PathBuf {
        dir.join(NOTIFY_FILE_NAME)
    }

    /// Read the default config beside `dir`, or `None` when it is not there.
    ///
    /// Open-if-present: it never creates the file, so "no `notify.toml`" is the quiet,
    /// socket-free default. A file that *is* there but malformed is still an error — a
    /// broken config is not the same as no config.
    pub fn load_beside(dir: &Path) -> Result<Option<NotifyConfig>, NotifyError> {
        let path = dir.join(NOTIFY_FILE_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        NotifyConfig::load(&path).map(Some)
    }

    /// Read and parse an explicitly named config.
    pub fn load(path: &Path) -> Result<NotifyConfig, NotifyError> {
        let text = TextOnDisk::read(path)?;
        NotifyConfig::parse(&text, path)
    }

    /// Parse the TOML text. Split out from [`load`] so the decision is unit-testable
    /// without a filesystem.
    pub fn parse(text: &str, path: &Path) -> Result<NotifyConfig, NotifyError> {
        let raw: RawNotify = toml::from_str(text).map_err(|source| NotifyError::Malformed {
            path: path.to_path_buf(),
            source,
        })?;
        let line = line_at(text, raw.url.span().start);
        let invalid = |detail: String| NotifyError::Invalid {
            path: path.to_path_buf(),
            detail,
        };
        let target = parse_target(raw.url.get_ref())
            .map_err(|reason| invalid(format!("line {line}: {reason}")))?;
        Ok(NotifyConfig { target })
    }

    /// The host the endpoint names, for a caller that wants to report it. Never the URL.
    pub fn host(&self) -> &str {
        &self.target.host
    }

    /// POST `message` as a Discord `{"content": ...}` body.
    ///
    /// One request, bounded by [`NOTIFY_TIMEOUT`], no retry. A 2xx is success; everything
    /// else — an unreachable host, a TLS failure, a non-2xx status, a malformed response —
    /// is a [`DeliveryError`] the caller turns into a warning.
    pub fn send(&self, message: &str) -> Result<(), DeliveryError> {
        let body = json_body(message);
        let target = &self.target;
        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nUser-Agent: just_cache\r\nConnection: close\r\n\r\n",
            target.path,
            target.authority(),
            body.len()
        );
        match target.scheme {
            Scheme::Https => send_https(target, &head, body.as_bytes()),
            Scheme::Http => send_plain(target, &head, body.as_bytes()),
        }
    }
}

/// The file's shape. `deny_unknown_fields` turns a typo into a malformed-config error
/// that names its line rather than a silently ignored key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNotify {
    url: toml::Spanned<String>,
}

/// The 1-based line number holding a byte offset, exactly as `policy.rs`, `tiers.rs` and
/// `schedule.rs` do it.
fn line_at(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

/// Parse `https://host[:port]/path` (and the loopback-only `http://` form) by hand, with
/// no URL dependency. The reason is a [`String`] that is safe to show: it never repeats
/// the URL, only what is wrong with it.
fn parse_target(raw: &str) -> Result<Target, String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or("the URL must begin with `https://`")?;
    let scheme = match scheme {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => return Err(format!("unsupported URL scheme `{other}`; use `https://`")),
    };
    if rest.contains('@') {
        return Err("the URL must not carry credentials in the authority".to_string());
    }

    // Split the authority from the path at the first `/`; no path at all becomes `/`.
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let (host, port) = split_authority(authority)?;
    if host.is_empty() {
        return Err("the URL names no host".to_string());
    }
    let default_port = match scheme {
        Scheme::Https => 443,
        Scheme::Http => 80,
    };
    let port = match port {
        Some(port) => port,
        None => default_port,
    };
    if scheme == Scheme::Http && !is_loopback_host(host) {
        return Err(
            "plain http is accepted only for a loopback host; a webhook URL is a credential, \
             so use `https://`"
                .to_string(),
        );
    }
    Ok(Target {
        scheme,
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// Split `host` or `host:port`, handling a bracketed IPv6 literal (`[::1]:8080`).
fn split_authority(authority: &str) -> Result<(&str, Option<u16>), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or("an IPv6 address in the URL is missing its closing `]`")?;
        return match tail.strip_prefix(':') {
            Some(port) => Ok((host, Some(parse_port(port)?))),
            None if tail.is_empty() => Ok((host, None)),
            None => Err(format!("unexpected `{tail}` after the IPv6 address")),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() => Ok((host, Some(parse_port(port)?))),
        _ => Ok((authority, None)),
    }
}

fn parse_port(port: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .map_err(|_| format!("`{port}` is not a port number"))
}

/// True for a host that stays on the machine: `localhost`, `127.0.0.0/8`, or `::1`.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host.starts_with("127.")
        || host == "0.0.0.0"
}

/// The JSON body for one Discord message. Hand-built like every other writer here, with
/// the shared escaper, so a message with a quote or a newline cannot break the document.
pub fn json_body(message: &str) -> String {
    format!("{{\"content\":{}}}", crate::events::json_string(message))
}

/// One maintenance pass in a run summary: its name and its ordered counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassCounts {
    pub pass: &'static str,
    pub counts: Vec<(&'static str, usize)>,
}

impl PassCounts {
    pub fn new(pass: &'static str, counts: Vec<(&'static str, usize)>) -> Self {
        PassCounts { pass, counts }
    }

    /// `scrub: 12 verified, 1 repaired, 0 damaged` — counts only, never a path.
    fn line(&self) -> String {
        let body: Vec<String> = self
            .counts
            .iter()
            .map(|(label, count)| format!("{count} {label}"))
            .collect();
        format!("{}: {}", self.pass, body.join(", "))
    }
}

/// The short summary of a scheduled maintenance run: which passes ran, how many were
/// held back by a free-space floor, and each pass's own counts.
///
/// `None` when nothing happened — no pass ran and none was held back — because a check
/// that found nothing due must stay quiet, exactly as `schedule` itself is on stdout.
/// The catalog is named by its last path component only, never its full path.
pub fn run_summary(catalog: &Path, ran: &[PassCounts], held_back: &[&str]) -> Option<String> {
    if ran.is_empty() && held_back.is_empty() {
        return None;
    }
    let name = catalog
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "catalog".to_string());
    let mut out = format!(
        "just_cache scheduled run on {name}: {} pass(es) ran, {} held back",
        ran.len(),
        held_back.len()
    );
    for pass in ran {
        out.push_str("\n  ");
        out.push_str(&pass.line());
    }
    if !held_back.is_empty() {
        out.push_str(&format!(
            "\n  held back by the free-space floor: {}",
            held_back.join(", ")
        ));
    }
    Some(out)
}

/// The message for an `audit` that has findings: the total, the non-zero counts by kind,
/// and the repair outcome when a repair was attempted. Counts and kind names only — a
/// finding's path is never included. `repairs` is `(resolved, unresolved)`: an unrepaired
/// finding is one the run could not fix, whatever the reason.
pub fn audit_message(
    catalog: &Path,
    total: usize,
    counts: &[(&str, usize)],
    repairs: Option<(usize, usize)>,
) -> String {
    let name = catalog
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "catalog".to_string());
    let mut out = format!("just_cache audit on {name}: {total} finding(s)");
    let kinds: Vec<String> = counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(kind, count)| format!("{kind}: {count}"))
        .collect();
    if !kinds.is_empty() {
        out.push_str(&format!("\n  {}", kinds.join(", ")));
    }
    if let Some((resolved, unresolved)) = repairs {
        out.push_str(&format!(
            "\n  repairs: {resolved} resolved, {unresolved} unresolved"
        ));
    }
    out
}

/// The prompt warning for a scheduled pass that failed. The pass name only: the pass's
/// own error text can name paths or hosts, and this message leaves the machine.
pub fn pass_failed(pass: &str) -> String {
    format!("just_cache: the scheduled {pass} pass failed; see the host's own logs")
}

/// Read a small file, mapping a missing file to the same error as any other read failure
/// (a caller only reaches [`NotifyConfig::load`] for a file it knows is there).
struct TextOnDisk;

impl TextOnDisk {
    fn read(path: &Path) -> Result<String, NotifyError> {
        std::fs::read_to_string(path).map_err(|source| NotifyError::Read {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Resolve the target's host to one address. Resolution itself is not bounded by
/// [`NOTIFY_TIMEOUT`] — std exposes no timeout for the system resolver — which is a named
/// gap, not an oversight; the connect, write and read are all bounded.
fn resolve(target: &Target) -> Result<std::net::SocketAddr, DeliveryError> {
    (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|_| DeliveryError::Resolve {
            host: target.host.clone(),
        })?
        .next()
        .ok_or_else(|| DeliveryError::Resolve {
            host: target.host.clone(),
        })
}

/// Plain-HTTP delivery, used only for a loopback endpoint (enforced at parse time).
fn send_plain(target: &Target, head: &str, body: &[u8]) -> Result<(), DeliveryError> {
    let host = &target.host;
    let mut stream =
        TcpStream::connect_timeout(&resolve(target)?, NOTIFY_TIMEOUT).map_err(|source| {
            DeliveryError::Connect {
                host: host.clone(),
                source,
            }
        })?;
    stream
        .set_read_timeout(Some(NOTIFY_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(NOTIFY_TIMEOUT)))
        .map_err(|source| DeliveryError::Write {
            host: host.clone(),
            source,
        })?;
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|source| DeliveryError::Write {
            host: host.clone(),
            source,
        })?;
    let mut reader = BufReader::new(stream);
    read_status(&mut reader, host)
}

/// HTTPS delivery: a `rustls` client over the same hand-rolled request, mirroring
/// `object_store`'s transport.
fn send_https(target: &Target, head: &str, body: &[u8]) -> Result<(), DeliveryError> {
    let host = &target.host;
    let mut tls = TlsStream::new(target)?;
    tls.write_all(head.as_bytes())
        .and_then(|()| tls.write_all(body))
        .and_then(|()| tls.flush())
        .map_err(|source| DeliveryError::Write {
            host: host.clone(),
            source,
        })?;
    let mut reader = BufReader::new(tls);
    read_status(&mut reader, host)
}

/// Read the response status line and require a 2xx. Anything else — a non-2xx status, or
/// a line that is not an HTTP status — is a [`DeliveryError`].
fn read_status(reader: &mut impl BufRead, host: &str) -> Result<(), DeliveryError> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|source| DeliveryError::Read {
            host: host.to_string(),
            source,
        })?;
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let status = parts.next().and_then(|code| code.parse::<u16>().ok());
    let (Some(status), true) = (status, version.starts_with("HTTP/")) else {
        return Err(DeliveryError::Malformed {
            host: host.to_string(),
        });
    };
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(DeliveryError::Status {
            host: host.to_string(),
            status,
        })
    }
}

/// A minimal TLS stream owning both the TCP stream and the TLS session, mirroring
/// `object_store::TlsStream`. Unlike that one, a read timeout is surfaced as an error
/// rather than retried forever: a hung endpoint must end the wait, not spin on it.
struct TlsStream {
    stream: TcpStream,
    conn: rustls::ClientConnection,
}

impl TlsStream {
    fn new(target: &Target) -> Result<Self, DeliveryError> {
        let host = &target.host;
        let stream =
            TcpStream::connect_timeout(&resolve(target)?, NOTIFY_TIMEOUT).map_err(|source| {
                DeliveryError::Connect {
                    host: host.clone(),
                    source,
                }
            })?;
        stream
            .set_read_timeout(Some(NOTIFY_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(NOTIFY_TIMEOUT)))
            .map_err(|source| DeliveryError::Write {
                host: host.clone(),
                source,
            })?;

        let server_name = ServerName::try_from(host.clone())
            .map_err(|_| DeliveryError::Resolve { host: host.clone() })?;
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let conn = rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|err| {
            DeliveryError::Tls {
                host: host.clone(),
                detail: err.to_string(),
            }
        })?;

        let mut tls = TlsStream { stream, conn };
        tls.conn
            .complete_io(&mut tls.stream)
            .map_err(|err| DeliveryError::Tls {
                host: host.clone(),
                detail: err.to_string(),
            })?;
        Ok(tls)
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
            match self.conn.complete_io(&mut self.stream) {
                Ok((0, _)) => return Ok(0), // EOF
                Ok(_) => continue,
                // A blocking socket with a read timeout reports a timeout as WouldBlock;
                // treat it as the timeout it is rather than looping.
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the endpoint timed out",
                    ));
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("/etc/just_cache/notify.toml")
    }

    #[test]
    fn a_webhook_url_parses_into_host_port_and_path() {
        let config = NotifyConfig::parse(
            "url = \"https://discord.com/api/webhooks/123/abc\"\n",
            &path(),
        )
        .unwrap();
        assert_eq!(config.host(), "discord.com");
        assert_eq!(config.target.port, 443);
        assert_eq!(config.target.path, "/api/webhooks/123/abc");
        assert_eq!(config.target.scheme, Scheme::Https);
    }

    #[test]
    fn a_port_and_a_loopback_http_url_are_accepted() {
        // The loopback http form exists so tests can point at a TcpListener.
        let config =
            NotifyConfig::parse("url = \"http://127.0.0.1:8080/hook\"\n", &path()).unwrap();
        assert_eq!(config.target.scheme, Scheme::Http);
        assert_eq!(config.target.host, "127.0.0.1");
        assert_eq!(config.target.port, 8080);
        assert_eq!(config.target.path, "/hook");
    }

    #[test]
    fn a_missing_path_becomes_the_root() {
        let config = NotifyConfig::parse("url = \"https://example.com\"\n", &path()).unwrap();
        assert_eq!(config.target.path, "/");
        assert_eq!(config.target.port, 443);
    }

    #[test]
    fn a_non_loopback_plain_http_url_is_refused_naming_its_line() {
        let err = NotifyConfig::parse("url = \"http://example.com/hook\"\n", &path()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("line 1"), "{text}");
        assert!(text.contains("loopback"), "{text}");
        // The refusal never echoes the credential-bearing URL.
        assert!(!text.contains("example.com/hook"), "{text}");
    }

    #[test]
    fn an_unsupported_scheme_is_refused_naming_its_line() {
        let err = NotifyConfig::parse("url = \"ftp://example.com/x\"\n", &path()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("line 1"), "{text}");
        assert!(text.contains("scheme"), "{text}");
    }

    #[test]
    fn a_malformed_file_is_an_error_that_names_its_line() {
        // `deny_unknown_fields` turns a typo into a parse error whose message carries the
        // line, exactly as a broken schedule.toml does.
        let err = NotifyConfig::parse("url = 7\n", &path()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("malformed notify config"), "{text}");
        assert!(text.contains("line 1"), "{text}");
    }

    #[test]
    fn an_absent_file_is_off_and_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(NotifyConfig::load_beside(tmp.path()).unwrap().is_none());
        assert!(
            !NotifyConfig::default_path(tmp.path()).exists(),
            "loading a config must never create it"
        );
    }

    #[test]
    fn the_body_is_a_discord_content_object_with_the_message_escaped() {
        assert_eq!(json_body("hello"), "{\"content\":\"hello\"}");
        // A quote or a newline inside the message cannot break the document.
        assert_eq!(json_body("a\"b\nc"), "{\"content\":\"a\\\"b\\nc\"}");
    }

    #[test]
    fn the_run_summary_carries_counts_and_no_full_path() {
        let catalog = Path::new("/mnt/cache/media/.just_cache-catalog.sqlite");
        let ran = vec![
            PassCounts::new(
                "scrub",
                vec![("verified", 12), ("repaired", 1), ("damaged", 0)],
            ),
            PassCounts::new("reconcile", vec![("rebuilt", 2), ("failed", 0)]),
        ];
        let message = run_summary(catalog, &ran, &[]).unwrap();
        assert!(message.contains("2 pass(es) ran"), "{message}");
        assert!(
            message.contains("scrub: 12 verified, 1 repaired, 0 damaged"),
            "{message}"
        );
        assert!(
            message.contains("reconcile: 2 rebuilt, 0 failed"),
            "{message}"
        );
        // Only the catalog's last component is named, never the path.
        assert!(message.contains(".just_cache-catalog.sqlite"), "{message}");
        assert!(!message.contains("/mnt"), "{message}");
    }

    #[test]
    fn a_quiet_run_produces_no_summary() {
        assert!(run_summary(Path::new("/c/x.sqlite"), &[], &[]).is_none());
    }

    #[test]
    fn a_held_back_pass_appears_without_a_path() {
        let message = run_summary(Path::new("/c/x.sqlite"), &[], &["scrub"]).unwrap();
        assert!(message.contains("0 pass(es) ran, 1 held back"), "{message}");
        assert!(message.contains("scrub"), "{message}");
        assert!(!message.contains("/c/"), "{message}");
    }

    #[test]
    fn the_audit_message_lists_non_zero_kinds_and_repair_outcomes() {
        let catalog = Path::new("/mnt/cache/media/.just_cache-catalog.sqlite");
        let counts = [("duplicate", 1), ("dangling-symlink", 2), ("healthy", 0)];
        let message = audit_message(catalog, 3, &counts, Some((1, 0)));
        assert!(message.contains("3 finding(s)"), "{message}");
        assert!(
            message.contains("duplicate: 1, dangling-symlink: 2"),
            "{message}"
        );
        assert!(!message.contains("healthy"), "{message}");
        assert!(message.contains("1 resolved, 0 unresolved"), "{message}");
        assert!(!message.contains("/mnt"), "{message}");
    }

    #[test]
    fn the_failure_message_names_only_the_pass() {
        let message = pass_failed("scrub");
        assert!(message.contains("scrub"), "{message}");
        assert!(message.contains("failed"), "{message}");
    }
}
