//! The MCP adapter, end to end through the real binary (issue #137).
//!
//! What these tests are for: an agent-facing surface is a public API (§8), so the things
//! worth pinning are the ones a prompt gets written against — which tools exist, which do
//! not, and what a call returns. Three claims carry the security posture:
//!
//! * the tool list is the policy: no `sweep`, no `delete`, no `gc --apply`, no `reconcile`,
//!   no `pin`, and `restore` only when the operator opts in;
//! * a tool is offered only when the server was given the roots it needs, so `tools/list`
//!   never advertises an answer this invocation cannot produce;
//! * the answer an agent gets is the answer the CLI gives: the adapter relays the command's
//!   own output, so the two cannot drift.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

/// A watched tree and a cold root, both real directories.
struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    fn build() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("hot")).unwrap();
        std::fs::create_dir_all(dir.path().join("cold")).unwrap();
        Tree { dir }
    }

    fn hot(&self) -> PathBuf {
        self.dir.path().join("hot")
    }

    fn cold(&self) -> PathBuf {
        self.dir.path().join("cold")
    }

    fn catalog(&self) -> PathBuf {
        self.hot().join(".just_cache-catalog.sqlite")
    }

    /// A palettefile under the watched tree, plus a synced catalog that knows about it.
    fn with_synced_file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.hot().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let out = bin()
            .args(["catalog", "sync", "--watch"])
            .arg(self.hot())
            .arg("--dest")
            .arg(self.cold())
            .output()
            .unwrap();
        assert!(out.status.success(), "sync failed: {:?}", text(&out));
        path
    }
}

fn text(out: &std::process::Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Drive one server session: feed every line, close stdin, read every response.
///
/// Closing stdin is how a session ends — the adapter is client-driven (`serve` reads until
/// EOF) — so this also exercises the normal exit path rather than killing the process.
fn session(args: &[String], lines: &[String]) -> (Vec<Value>, String) {
    let mut child = bin()
        .arg("mcp")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for line in lines {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let responses = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("response must be JSON"))
        .collect();
    (responses, String::from_utf8_lossy(&out.stderr).into_owned())
}

fn request(id: i64, method: &str, params: Value) -> String {
    let mut object = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method });
    if !params.is_null() {
        object["params"] = params;
    }
    object.to_string()
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .expect("tools/list returns an array")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

/// One argument, as the server would receive it.
fn arg(path: &Path) -> String {
    path.display().to_string()
}

/// The flags that give the server every root it can take.
fn all_roots(tree: &Tree) -> Vec<String> {
    vec![
        "--catalog".to_string(),
        arg(&tree.catalog()),
        "--watch".to_string(),
        arg(&tree.hot()),
        "--dest".to_string(),
        arg(&tree.cold()),
    ]
}

/// The tree-and-tiers roots only: the tools that compare the watched tree to its cold root.
fn tree_roots(tree: &Tree) -> Vec<String> {
    vec![
        "--watch".to_string(),
        arg(&tree.hot()),
        "--dest".to_string(),
        arg(&tree.cold()),
    ]
}

/// The catalog-only root.
fn catalog_only(tree: &Tree) -> Vec<String> {
    vec!["--catalog".to_string(), arg(&tree.catalog())]
}

#[test]
fn initialize_names_the_server_without_requiring_a_catalog() {
    let tree = Tree::build();
    let lines = vec![request(1, "initialize", Value::Null)];
    let (responses, stderr) = session(&catalog_only(&tree), &lines);

    assert_eq!(responses.len(), 1, "one request, one response: {stderr}");
    let result = &responses[0]["result"];
    assert_eq!(result["serverInfo"]["name"], "just_cache");
    assert!(result["protocolVersion"].is_string());
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
}

#[test]
fn the_tool_list_is_the_policy() {
    let tree = Tree::build();
    let lines = vec![request(1, "tools/list", Value::Null)];
    // Every root the server can take, and the opt-in, so the *complete* surface is visible.
    let mut args = all_roots(&tree);
    args.push("--allow-restore".to_string());
    let (responses, _) = session(&args, &lines);

    let names = tool_names(&responses[0]);
    assert_eq!(
        names,
        vec!["locate", "explain", "audit", "scrub", "restore"]
    );

    // The point of the assertion above is what is *missing*. Name the refusals explicitly so
    // a future change that adds one of these has to delete a line that says it should not.
    for forbidden in [
        "sweep",
        "delete",
        "catalog-delete",
        "gc",
        "reconcile",
        "pin",
        "unpin",
        "resolve",
        "mount",
        "volume",
    ] {
        assert!(
            !names.iter().any(|name| name == forbidden),
            "`{forbidden}` must never be on the agent-facing surface: {names:?}"
        );
    }
}

#[test]
fn restore_is_opt_in_and_absent_by_default() {
    let tree = Tree::build();
    let lines = vec![request(1, "tools/list", Value::Null)];
    let args = all_roots(&tree);
    let (responses, _) = session(&args, &lines);
    let names = tool_names(&responses[0]);

    assert!(names.contains(&"explain".to_string()), "{names:?}");
    assert!(
        !names.contains(&"restore".to_string()),
        "restore writes bytes; it is opt-in: {names:?}"
    );

    // And it cannot be called by name either — the list is not the only gate.
    let lines = vec![request(
        1,
        "tools/call",
        serde_json::json!({ "name": "restore", "arguments": { "path": "hot/a.bin" } }),
    )];
    let (responses, _) = session(&args, &lines);
    assert_eq!(responses[0]["error"]["code"], -32602);
    assert!(
        responses[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("allow-restore"),
        "the refusal names the reason: {}",
        responses[0]
    );
}

#[test]
fn a_tool_is_offered_only_when_its_roots_were_given() {
    let tree = Tree::build();

    // Catalog only: the questions the catalog alone can answer.
    let lines = vec![request(1, "tools/list", Value::Null)];
    let (responses, _) = session(&catalog_only(&tree), &lines);
    assert_eq!(tool_names(&responses[0]), vec!["locate", "scrub"]);

    // Watch and dest only: the questions that compare the tree to its tiers.
    let (responses, _) = session(&tree_roots(&tree), &lines);
    assert_eq!(tool_names(&responses[0]), vec!["explain", "audit"]);
}

#[test]
fn the_answer_is_the_cli_answer() {
    let tree = Tree::build();
    tree.with_synced_file("shows/movie.mkv", b"not really a movie");

    // What the CLI says, directly.
    let direct = bin()
        .args(["locate", "shows/movie.mkv", "--catalog"])
        .arg(tree.catalog())
        .arg("--json")
        .output()
        .unwrap();
    assert!(direct.status.success(), "{}", text(&direct));
    let direct_text = String::from_utf8_lossy(&direct.stdout)
        .trim_end()
        .to_string();

    // What an agent gets from the same question.
    let lines = vec![request(
        1,
        "tools/call",
        serde_json::json!({ "name": "locate", "arguments": { "query": "shows/movie.mkv" } }),
    )];
    let (responses, stderr) = session(&catalog_only(&tree), &lines);
    assert!(stderr.is_empty(), "the server logged to stderr: {stderr}");

    let result = &responses[0]["result"];
    assert_eq!(result["isError"], false);
    let relayed = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        relayed, direct_text,
        "the adapter must relay the CLI's own output, byte for byte"
    );
}

#[cfg(unix)]
#[test]
fn a_nonzero_exit_that_carries_an_answer_is_not_a_tool_error() {
    let tree = Tree::build();
    tree.with_synced_file("kept.bin", b"kept bytes");
    // A symlink into the cold root pointing at nothing: `audit` reports it as dangling and
    // exits 1, which is the case this test is about — an answer delivered with a nonzero code.
    std::os::unix::fs::symlink(
        tree.cold().join("gone.bin"),
        tree.hot().join("dangling.bin"),
    )
    .unwrap();

    let direct = bin()
        .args(["audit", "--watch"])
        .arg(tree.hot())
        .arg("--dest")
        .arg(tree.cold())
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(
        direct.status.code(),
        Some(1),
        "the fixture must actually make audit exit 1: {}",
        text(&direct)
    );

    let lines = vec![request(
        1,
        "tools/call",
        serde_json::json!({ "name": "audit", "arguments": {} }),
    )];
    let (responses, _) = session(&tree_roots(&tree), &lines);

    let result = &responses[0]["result"];
    let body = result["content"][0]["text"].as_str().unwrap();
    assert!(
        body.contains("dangling"),
        "the finding is in the answer: {body}"
    );
    assert_eq!(
        result["isError"], false,
        "audit exits 1 to say `look at this`, not `I failed`"
    );
}

#[test]
fn a_failure_with_no_answer_is_a_tool_error() {
    let tree = Tree::build();
    // A catalog that does not exist: `locate` refuses and prints nothing to stdout.
    let lines = vec![request(
        1,
        "tools/call",
        serde_json::json!({ "name": "locate", "arguments": { "query": "anything" } }),
    )];
    let (responses, _) = session(&catalog_only(&tree), &lines);

    let result = &responses[0]["result"];
    assert_eq!(result["isError"], true);
    let body = result["content"][0]["text"].as_str().unwrap();
    assert!(
        body.contains("catalog"),
        "the refusal explains itself: {body}"
    );
}

#[test]
fn a_bad_call_is_refused_by_name_and_the_session_survives() {
    let tree = Tree::build();
    tree.with_synced_file("a.bin", b"a");
    let lines = vec![
        request(
            1,
            "tools/call",
            serde_json::json!({ "name": "no-such-tool", "arguments": {} }),
        ),
        // An argument the tool does not take is refused, not dropped: a client must not be
        // able to believe it asked for something it did not.
        request(
            2,
            "tools/call",
            serde_json::json!({ "name": "locate", "arguments": { "query": "a.bin", "watch": "/nope" } }),
        ),
        request(
            3,
            "tools/call",
            serde_json::json!({ "name": "scrub", "arguments": {} }),
        ),
        request(4, "not/a/method", Value::Null),
        // Still answering after all of that.
        request(5, "tools/list", Value::Null),
    ];
    let (responses, _) = session(&catalog_only(&tree), &lines);

    assert_eq!(responses.len(), 5, "every request gets its own response");
    assert_eq!(responses[0]["error"]["code"], -32602);
    assert!(responses[0]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown tool"));
    assert_eq!(responses[1]["error"]["code"], -32602);
    assert!(responses[1]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown argument `watch`"));
    // `scrub` is always a dry run; there is no argument that could make it repair.
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[2]["result"]["isError"], false, "{}", responses[2]);
    assert_eq!(responses[3]["error"]["code"], -32601);
    assert_eq!(responses[4]["id"], 5);
    assert!(!tool_names(&responses[4]).is_empty());
}

#[test]
fn a_malformed_frame_is_an_error_and_not_the_end_of_the_session() {
    let tree = Tree::build();
    let lines = vec![
        "{ this is not json".to_string(),
        // A notification: no id, so no reply is owed.
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
        request(2, "ping", Value::Null),
    ];
    let (responses, _) = session(&catalog_only(&tree), &lines);

    assert_eq!(
        responses.len(),
        2,
        "the parse error and the ping — not the notification"
    );
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[0]["id"], Value::Null);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[1]["result"], serde_json::json!({}));
}

#[test]
fn a_server_with_nowhere_to_answer_from_is_a_usage_error() {
    let out = bin().arg("mcp").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--catalog"),
        "{}",
        text(&out)
    );

    // `--watch` without `--dest` is not a source of answers either: audit/explain compare
    // the tree *to* something.
    let tree = Tree::build();
    let out = bin()
        .arg("mcp")
        .arg("--watch")
        .arg(tree.hot())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}
