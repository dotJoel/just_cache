//! The MCP adapter (issue #137): a client-driven, read-only surface over the catalog-backed
//! commands, spoken as newline-delimited JSON-RPC 2.0 on stdio.
//!
//! ## Why this is an adapter and not a second implementation
//!
//! The CLI is the contract (§8, #137). Every tool here runs *this same binary* as a
//! subcommand and relays what it printed, so an answer given to an agent is byte-identical
//! to the answer given to a shell — there is no second code path to drift, and a fix to a
//! command fixes the agent's view of it. The alternative (calling the library functions
//! directly) would mean re-deriving each command's exit-code and output rules in a module
//! that has no business knowing them.
//!
//! ## What is deliberately absent
//!
//! The tool list *is* the policy. There is no `sweep`, no `catalog delete`, no `gc --apply`,
//! no `reconcile`, no `pin`: the default surface is the four read-only questions — where is
//! this, why is it there, is this consistent, are the bytes still good — and `scrub` is
//! pinned to `--dry-run` so it cannot repair. `restore` moves bytes and writes, so it is
//! exposed only when the operator starts the server with `--allow-restore`; that is the
//! opt-in §10 describes, and it is a decision the server makes once, not one an agent makes
//! per call.
//!
//! ## Exit codes are answers, not failures
//!
//! These commands use a nonzero exit to say "here is your answer, and you will want to look"
//! — `audit` exits 1 on any finding, `explain` exits nonzero when the answer is "not this
//! sweep". Reporting that as a tool *failure* would tell an agent to ignore the answer it
//! just asked for. So `isError` is set only when the command produced **no output at all**
//! and failed; the exit code is always stated in the text so a caller can still see it.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

/// The MCP revision this server speaks. One version, stated, rather than negotiation across
/// a range: the tool surface is four frozen read-only commands, so there is nothing in a
/// newer revision to want yet.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Server configuration: the operator's roots. These are the only paths an agent can cause
/// to be read, and they are chosen once, when the server starts — a tool call names a
/// query, never a path. An agent cannot widen its own scope, which is the same reason
/// `verify` and the mover take their roots from the command line and not from a config file
/// inside the watched tree.
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// The catalog file. `locate`, `scrub` and (optionally) `audit` need it.
    pub catalog: Option<PathBuf>,
    /// The watched tree. `explain`, `audit` and `restore` need it.
    pub watch: Option<PathBuf>,
    /// Cold-storage roots, fastest first. `explain`, `audit` and `restore` need them.
    pub dest: Vec<PathBuf>,
    /// Tier configuration, passed through to the commands that accept it.
    pub tiers: Option<PathBuf>,
    /// Lifecycle policy, passed through to `explain`.
    pub policy: Option<PathBuf>,
    /// Expose `restore`. Off by default: it writes bytes back into the watched tree.
    pub allow_restore: bool,
    /// The binary to re-execute. `main` passes its own path.
    pub program: PathBuf,
}

/// The read-only tool surface. One variant per CLI command the adapter is allowed to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Locate,
    Explain,
    Audit,
    Scrub,
    Restore,
}

const TOOL_ORDER: [Tool; 5] = [
    Tool::Locate,
    Tool::Explain,
    Tool::Audit,
    Tool::Scrub,
    Tool::Restore,
];

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Locate => "locate",
            Tool::Explain => "explain",
            Tool::Audit => "audit",
            Tool::Scrub => "scrub",
            Tool::Restore => "restore",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Tool::Locate => {
                "Where an object lives: every recorded copy, its tier, and which copy is the \
                 tier of record. Answers by namespace path or by a full or prefixed BLAKE3 \
                 digest. Works when the tier holding the bytes is not mounted, because the \
                 answer comes from the catalog — it does not read the bytes."
            }
            Tool::Explain => {
                "Why one path is where it is, and what a sweep would do with it next: the \
                 rule that fires or the exclusion that stops it, whether it is open or \
                 hardlinked, and what the mover would refuse."
            }
            Tool::Audit => {
                "Structural consistency between the watched tree and the cold tiers: healthy, \
                 duplicate, dangling, orphaned, missing-copy, replica-lost, plus the scrub \
                 state. Exits nonzero when there are findings; that is the answer, not a \
                 failure. Nothing is repaired."
            }
            Tool::Scrub => {
                "Reads every stored copy back and checks it against the recorded checksum, \
                 reporting rot and what a repair would do. Always a dry run: this surface \
                 never repairs, never writes, and records no last-verified state."
            }
            Tool::Restore => {
                "Brings an offloaded file's bytes back to its hot path, verified, and \
                 collapses the symlink into a real file. This tool writes; it is exposed \
                 only when the server was started with `--allow-restore`."
            }
        }
    }

    /// The JSON Schema for this tool's arguments. A tool takes only what its command does
    /// not already know from the server's configuration.
    fn schema(self) -> Value {
        let (props, required, extra) = match self {
            Tool::Locate => (
                json!({
                    "query": {
                        "type": "string",
                        "description": "A namespace path (relative to the watched tree, the \
                                        same string `catalog sync` ingested), or a full or \
                                        prefixed BLAKE3 hex id. Eight or more hex characters \
                                        is read as a digest prefix; anything else is a path."
                    }
                }),
                json!(["query"]),
                "locate",
            ),
            Tool::Explain => (
                json!({
                    "path": {
                        "type": "string",
                        "description": "The path to explain, absolute or relative to the \
                                        watched tree."
                    }
                }),
                json!(["path"]),
                "explain",
            ),
            Tool::Audit => (json!({}), json!([]), "audit"),
            Tool::Scrub => (json!({}), json!([]), "scrub"),
            Tool::Restore => (
                json!({
                    "path": {
                        "type": "string",
                        "description": "Path to bring back to the hot tier. It must be inside \
                                        the watched tree. It may currently be a working \
                                        symlink, a broken one, or missing."
                    }
                }),
                json!(["path"]),
                "restore",
            ),
        };
        json!({
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
            "description": format!("Runs `just_cache {extra}`."),
        })
    }

    /// Whether the operator's configuration can answer this tool at all. A tool whose
    /// roots were not supplied is not offered, rather than being offered and then failing:
    /// an agent should not have to guess which of its tools the server can actually serve,
    /// and "the server was not started with that root" is not something a model can fix.
    fn availability(self, config: &McpConfig) -> Result<(), String> {
        match self {
            Tool::Locate | Tool::Scrub => {
                if config.catalog.is_none() {
                    return Err("--catalog was not given".to_string());
                }
            }
            Tool::Audit => {
                if config.watch.is_none() || config.dest.is_empty() {
                    return Err("--watch and --dest were not given".to_string());
                }
            }
            Tool::Explain | Tool::Restore => {
                if config.watch.is_none() || config.dest.is_empty() {
                    return Err("--watch and --dest were not given".to_string());
                }
            }
        }
        if self == Tool::Restore && !config.allow_restore {
            return Err("the server was started without --allow-restore".to_string());
        }
        Ok(())
    }

    /// The argv handed to a re-execution of this binary, after the tool's own arguments
    /// have been checked. `json` is included only where the command grew a JSON writer;
    /// `scrub` and `restore` have none, so their text is relayed as the answer rather than
    /// a second format being invented for them (the rule #137 sets).
    fn argv(self, config: &McpConfig, args: &Value) -> Result<Vec<String>, String> {
        // `additionalProperties: false` in the schema is a hint to the agent, not
        // enforcement of it: the request is written by the client, so it is checked here
        // too. An argument this tool does not understand is refused by name rather than
        // dropped, so a client cannot believe it asked for something it did not.
        let allowed: &[&str] = match self {
            Tool::Audit | Tool::Scrub => &[],
            Tool::Explain | Tool::Restore => &["path"],
            Tool::Locate => &["query"],
        };
        let object = args
            .as_object()
            .ok_or_else(|| "arguments must be a JSON object".to_string())?;
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!(
                    "unknown argument `{key}`; {} takes {}",
                    self.name(),
                    if allowed.is_empty() {
                        "no arguments".to_string()
                    } else {
                        allowed.join(", ")
                    }
                ));
            }
        }
        let required = |key: &str| -> Result<String, String> {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("missing required argument `{key}`"))
        };

        let mut argv = vec![self.name().to_string()];
        match self {
            Tool::Locate => {
                argv.push(required("query")?);
                argv.push("--catalog".to_string());
                argv.push(path(config.catalog.as_ref().unwrap()));
                if let Some(tiers) = &config.tiers {
                    argv.push("--tiers".to_string());
                    argv.push(path(tiers));
                }
                argv.push("--json".to_string());
            }
            Tool::Explain => {
                argv.push(required("path")?);
                push_roots(&mut argv, config);
                if let Some(tiers) = &config.tiers {
                    argv.push("--tiers".to_string());
                    argv.push(path(tiers));
                }
                if let Some(policy) = &config.policy {
                    argv.push("--policy".to_string());
                    argv.push(path(policy));
                }
                argv.push("--json".to_string());
            }
            Tool::Audit => {
                push_roots(&mut argv, config);
                if let Some(catalog) = &config.catalog {
                    argv.push("--catalog".to_string());
                    argv.push(path(catalog));
                }
                if let Some(tiers) = &config.tiers {
                    argv.push("--tiers".to_string());
                    argv.push(path(tiers));
                }
                argv.push("--json".to_string());
            }
            Tool::Scrub => {
                argv.push("--catalog".to_string());
                argv.push(path(config.catalog.as_ref().unwrap()));
                // Always a dry run: this surface reports, it never repairs.
                argv.push("--dry-run".to_string());
            }
            Tool::Restore => {
                argv.push(required("path")?);
                push_roots(&mut argv, config);
                if let Some(tiers) = &config.tiers {
                    argv.push("--tiers".to_string());
                    argv.push(path(tiers));
                }
            }
        }
        Ok(argv)
    }
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn push_roots(argv: &mut Vec<String>, config: &McpConfig) {
    argv.push("--watch".to_string());
    argv.push(path(
        config.watch.as_ref().expect("checked by availability"),
    ));
    argv.push("--dest".to_string());
    for dest in &config.dest {
        argv.push(path(dest));
    }
}

/// Run the server over `input`/`output`, one JSON-RPC message per line.
///
/// The loop never exits on a bad message: a client that sends garbage, or a method that
/// does not exist, gets a JSON-RPC error and the server stays up. Dying on the first
/// malformed frame would turn one bad prompt into a dead session for every later question.
pub fn serve<R: BufRead, W: Write>(
    config: &McpConfig,
    input: R,
    mut output: W,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(config, &line) {
            output.write_all(response.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// One request in, at most one response out. `None` means the message was a notification
/// (no `id`) and JSON-RPC forbids a reply to it.
fn handle_line(config: &McpConfig, line: &str) -> Option<String> {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            // A frame that is not JSON has no id to answer to. `null` is what JSON-RPC
            // prescribes, and it is the honest value: we cannot know who asked.
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {error}"),
            ));
        }
    };
    let id = request.get("id").cloned();
    let Some(id) = id else {
        // No id: a notification (`notifications/initialized`, typically). Nothing to say.
        return None;
    };
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let response = match method {
        "initialize" => result_response(&id, initialize_result()),
        "ping" => result_response(&id, json!({})),
        "tools/list" => result_response(&id, tools_list(config)),
        "tools/call" => match call_tool(config, request.get("params").unwrap_or(&Value::Null)) {
            Ok(result) => result_response(&id, result),
            Err(message) => error_response(id, -32602, &message),
        },
        other => error_response(id, -32601, &format!("method not found: `{other}`")),
    };
    Some(response)
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "just_cache", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Read-only questions about a just_cache catalog: where a file lives, \
                         why it is there, whether the tree and its tiers agree, and whether \
                         the stored bytes still match their recorded checksums. This server \
                         never moves, repairs, or deletes anything."
    })
}

fn tools_list(config: &McpConfig) -> Value {
    let tools: Vec<Value> = TOOL_ORDER
        .iter()
        .copied()
        .filter(|tool| tool.availability(config).is_ok())
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "inputSchema": tool.schema(),
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// Execute one tool call. A refusal here is a JSON-RPC error (the call could not be made as
/// written); a command that ran and exited nonzero is a *result*, because for these commands
/// a nonzero exit is part of the answer. See the module comment.
fn call_tool(config: &McpConfig, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call needs a `name`".to_string())?;
    let tool = TOOL_ORDER
        .iter()
        .copied()
        .find(|tool| tool.name() == name)
        .ok_or_else(|| format!("unknown tool `{name}`"))?;
    // Re-check availability: the configured roots are why a tool exists, and a call must
    // not succeed just because the name was right.
    tool.availability(config)
        .map_err(|why| format!("tool `{name}` is not available: {why}"))?;
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    let argv = tool.argv(config, args)?;

    let output = Command::new(&config.program)
        .args(&argv)
        .output()
        .map_err(|error| {
            format!(
                "running `{} {}`: {error}",
                config.program.display(),
                argv.join(" ")
            )
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string();
    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .to_string();
    let code = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "killed by a signal".to_string());

    // The answer is stdout. Stderr is included when the command failed, because that is
    // where these commands explain a refusal ("catalog ... does not exist"), and an agent
    // that cannot see the reason cannot correct itself.
    let mut text = String::new();
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    if text.is_empty() {
        text = format!(
            "`just_cache {}` exited {code} with no output",
            argv.join(" ")
        );
    } else if !output.status.success() {
        // A normal answer is not cluttered with its exit code; a refusal is, because the
        // code is how the CLI distinguishes "no" from "not found from the catalog's view".
        text.push_str(&format!("\n(exit code {code})"));
    }

    // `isError` means "the tool did not answer". A nonzero exit from `audit` (findings) or
    // `explain` ("not this sweep") *is* the answer, so it only counts as an error when the
    // command produced nothing but its failure.
    let is_error = !output.status.success() && stdout.is_empty();
    Ok(json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    }))
}

fn result_response(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}
