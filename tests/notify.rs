//! End-to-end behaviour of the optional webhook notifications (issue #186), driven
//! through the binary for the contract the operator sees.
//!
//! There is no network in these tests: the only endpoint a run can reach is a
//! `std::net::TcpListener` this file binds on loopback, so "did it send?" is answered by
//! counting the connections to that listener, never by a real webhook. The cases the
//! issue calls out:
//!
//! * with no `notify.toml`, nothing is sent and nothing is dialled;
//! * a configured run posts a short summary whose body carries counts and no full path;
//! * a non-2xx answer is a warning that leaves the pass's exit code unchanged;
//! * an unreachable endpoint is a warning that leaves the pass's exit code unchanged;
//! * a malformed `notify.toml` is refused naming its line, before any socket is opened.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit; stdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

/// A non-zero payload long enough that a flipped byte changes the digest.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251 + 1) as u8).collect()
}

/// A hot file and its cold mirror, synced, so a scrub has one object with two clean
/// locations and a scheduled run reports zero findings.
fn clean_tree(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let hot = tmp.join("hot");
    let cold = tmp.join("cold");
    let catalog = tmp.join("catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), payload(8 * 1024)).unwrap();
    fs::write(cold.join("a.bin"), payload(8 * 1024)).unwrap();
    let sync = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .expect("just_cache runs");
    assert_exit(&sync, 0);
    (hot, cold, catalog)
}

fn write_schedule(catalog: &Path) {
    fs::write(
        catalog.parent().unwrap().join("schedule.toml"),
        "[scrub]\nevery = \"1h\"\n",
    )
    .unwrap();
}

fn write_notify_beside(catalog: &Path, url: &str) {
    write_notify_in(catalog.parent().unwrap(), url);
}

/// Write `notify.toml` into a directory (the placement is always beside the catalog).
fn write_notify_in(dir: &Path, url: &str) {
    fs::write(dir.join("notify.toml"), format!("url = \"{url}\"\n")).unwrap();
}

fn schedule(catalog: &Path) -> Output {
    bin()
        .args(["schedule", "--run", "--catalog"])
        .arg(catalog)
        .output()
        .expect("just_cache runs")
}

fn state_path(catalog: &Path) -> PathBuf {
    catalog.parent().unwrap().join(".just_cache-schedule.state")
}

/// A one-connection-at-a-time loopback HTTP server used as the webhook endpoint.
///
/// It records every request it is handed and answers each with the same canned status
/// line, so a test can assert both that a request arrived and what it carried. A
/// non-blocking accept loop means the test can also assert that *no* connection came,
/// without blocking forever waiting for one.
struct Recorder {
    addr: SocketAddr,
    conns: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Recorder {
    fn start(status_line: &'static str) -> Recorder {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let conns = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let conns = Arc::clone(&conns);
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            conns.fetch_add(1, Ordering::SeqCst);
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let request = read_request(&mut stream);
                            requests.lock().unwrap().push(request);
                            let _ = stream.write_all(status_line.as_bytes());
                            let _ = stream.flush();
                        }
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Recorder {
            addr,
            conns,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn connections(&self) -> usize {
        self.conns.load(Ordering::SeqCst)
    }

    /// Wait for at least `n` connections, so the assert does not race the server thread.
    fn wait_for(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.connections() < n && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The body of the first request, after the header block.
    fn first_body(&self) -> String {
        let requests = self.requests.lock().unwrap();
        let request = requests.first().expect("a request was recorded");
        let end = headers_end(request).expect("headers end");
        String::from_utf8_lossy(&request[end..]).into_owned()
    }

    /// The request line (`POST /hook HTTP/1.1`) of the first request.
    fn first_request_line(&self) -> String {
        let requests = self.requests.lock().unwrap();
        let request = requests.first().expect("a request was recorded");
        String::from_utf8_lossy(request)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Read one request: the header block, then exactly `Content-Length` bytes of body.
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(end) = headers_end(&buf) {
                    if buf.len() >= end + content_length(&buf[..end]) {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    buf
}

fn headers_end(request: &[u8]) -> Option<usize> {
    request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Content-Length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

const OK: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
const SERVER_ERROR: &str = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";

#[test]
fn no_config_sends_nothing_and_dials_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = clean_tree(tmp.path());
    write_schedule(&catalog);
    let recorder = Recorder::start(OK);

    let run = schedule(&catalog);
    assert_exit(&run, 0);
    // The pass ran (its state was recorded) and, with no notify.toml, the only endpoint
    // in the test saw no connection at all.
    assert!(state_path(&catalog).exists(), "the pass must have run");
    assert_eq!(
        recorder.connections(),
        0,
        "an unconfigured run must open no socket"
    );
    assert!(
        !stderr(&run).contains("notification"),
        "nothing to warn about: {}",
        stderr(&run)
    );
}

#[test]
fn a_configured_run_posts_a_summary_with_counts_and_no_full_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = clean_tree(tmp.path());
    write_schedule(&catalog);
    let recorder = Recorder::start(OK);
    write_notify_beside(&catalog, &recorder.url("/hook"));

    let run = schedule(&catalog);
    assert_exit(&run, 0);
    recorder.wait_for(1);
    assert_eq!(recorder.connections(), 1, "exactly one summary is sent");
    assert_eq!(recorder.first_request_line(), "POST /hook HTTP/1.1");

    let body = recorder.first_body();
    assert!(body.starts_with("{\"content\":\""), "{body}");
    assert!(
        body.contains("just_cache scheduled run on catalog.sqlite"),
        "{body}"
    );
    assert!(body.contains("1 pass(es) ran"), "{body}");
    assert!(body.contains("scrub: "), "{body}");
    assert!(body.contains("locations"), "{body}");
    assert!(body.contains("0 damaged"), "{body}");
    // No full path leaks into the message that leaves the machine.
    assert!(
        !body.contains(&tmp.path().display().to_string()),
        "the summary must not carry the full path: {body}"
    );
}

#[test]
fn a_non_2xx_answer_warns_and_leaves_the_exit_code_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = clean_tree(tmp.path());
    write_schedule(&catalog);
    let recorder = Recorder::start(SERVER_ERROR);
    write_notify_beside(&catalog, &recorder.url("/hook"));

    let run = schedule(&catalog);
    // The scrub was clean, so the pass exits zero whether or not the webhook answered.
    assert_exit(&run, 0);
    recorder.wait_for(1);
    assert_eq!(recorder.connections(), 1);
    let warning = stderr(&run);
    assert!(
        warning.contains("warning: could not send a notification"),
        "{warning}"
    );
    assert!(warning.contains("500"), "{warning}");
    // The pass was not blocked: its last-run time was recorded.
    assert!(state_path(&catalog).exists(), "the pass must have run");
}

#[test]
fn an_unreachable_endpoint_warns_and_leaves_the_exit_code_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = clean_tree(tmp.path());
    write_schedule(&catalog);
    // A bound-then-dropped listener gives a loopback port with nothing behind it.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    write_notify_beside(&catalog, &format!("http://{closed}/hook"));

    let run = schedule(&catalog);
    assert_exit(&run, 0);
    let warning = stderr(&run);
    assert!(
        warning.contains("warning: could not send a notification"),
        "{warning}"
    );
    assert!(warning.contains("cannot reach"), "{warning}");
    assert!(state_path(&catalog).exists(), "the pass must have run");
}

#[test]
fn a_malformed_notify_config_is_refused_before_any_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let (_hot, _cold, catalog) = clean_tree(tmp.path());
    write_schedule(&catalog);
    let recorder = Recorder::start(OK);
    // `url` must be a string; a number is a config the operator asked for and the tool
    // cannot honour, so it is refused like a broken schedule.toml.
    fs::write(catalog.parent().unwrap().join("notify.toml"), "url = 7\n").unwrap();

    let run = schedule(&catalog);
    assert_exit(&run, 2);
    let error = stderr(&run);
    assert!(error.contains("malformed notify config"), "{error}");
    assert!(error.contains("line 1"), "{error}");
    assert_eq!(recorder.connections(), 0, "no socket before a refusal");
    assert!(
        !state_path(&catalog).exists(),
        "a refused config must not run the pass"
    );
}

#[test]
fn audit_posts_when_it_has_findings_and_not_when_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();

    // Clean first: nothing to say.
    fs::write(watch.join("live.bin"), b"hot").unwrap();
    let clean = Recorder::start(OK);
    write_notify_in(&watch, &clean.url("/hook"));
    let run = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .output()
        .unwrap();
    assert_exit(&run, 0);
    assert_eq!(clean.connections(), 0, "a clean audit must stay quiet");

    // A duplicate (the same bytes at the hot path and the cold tier) is a finding.
    fs::write(watch.join("dup.bin"), b"same").unwrap();
    fs::write(cold.join("dup.bin"), b"same").unwrap();
    let recorder = Recorder::start(OK);
    write_notify_in(&watch, &recorder.url("/hook"));
    let run = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .output()
        .unwrap();
    assert_exit(&run, 1);
    recorder.wait_for(1);
    assert_eq!(recorder.connections(), 1);
    let body = recorder.first_body();
    // The catalog is named by its last component only, never the full path.
    assert!(
        body.contains("just_cache audit on .just_cache-catalog.sqlite"),
        "{body}"
    );
    assert!(body.contains("1 finding(s)"), "{body}");
    assert!(body.contains("duplicate: 1"), "{body}");
    assert!(
        !body.contains(&tmp.path().display().to_string()),
        "the audit message must not carry a full path: {body}"
    );
}
