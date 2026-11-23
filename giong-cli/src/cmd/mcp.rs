//! `giong mcp`: every `giong` command as a tool for coding agents, over the
//! Model Context Protocol on standard input and output.
//!
//! An agent registers it once (`giong ai init` writes the entry:
//! `{"command": "giong", "args": ["mcp"]}`) and gets one tool per command,
//! built from the command's flags, which runs `giong <command> --json` and
//! answers with its JSON document. `dev` is left out: it runs until stopped;
//! `dev_status` reads what a running `giong dev` keeps in `.giong/dev.json`
//! instead.
//!
//! An agent acts without asking at each step, so what a tool may do is
//! decided here, not by the agent ([`POLICIES`]). By default every tool only
//! reads: `doctor`, `check`, `explain`, `env show` with secrets masked, `db
//! status`, and so on. `giong mcp --allow-writes` adds what changes the
//! project but can be undone or redone (`clean` without `--all`, `fix`, `env
//! init`, `db up`, ...). What destroys data or hands out secrets (`db reset`,
//! `db down --volumes`, `clean --all`, `env get`, `--reveal`, `image --push`)
//! is never a tool: a person types it. A command with no policy is not a
//! tool at all. "Reads" is about the project's files and data: `check` runs
//! cargo, which runs the project's build scripts and proc macros, the same
//! trust as opening the project in an editor with rust-analyzer; `test`, which
//! runs the project's own code, is not a tool. Every tool carries MCP's `readOnlyHint`, `destructiveHint`
//! and `idempotentHint`, so a client can ask before a write.
//!
//! The protocol: JSON-RPC 2.0, one message per line (MCP's stdio transport),
//! version 2025-06-18 (older clients get the version they ask for).

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};

use serde_json::{json, Map, Value};

use super::{Command, COMMANDS};
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::report::{Failure, Report};

pub const COMMAND: Command = Command {
    name: "mcp",
    summary: "serve every giong command as a tool to coding agents (MCP over stdio)",
    usage: "giong mcp [--allow-writes]\n\n\
            Speaks the Model Context Protocol on standard input and output: one tool per giong \
            command (doctor, check, explain, clean, ...), each answering with the command's JSON \
            document, and dev_status for a running giong dev. Register it with your agent: \
            claude mcp add giong -- giong mcp (giong ai init writes .mcp.json for you).\n\n\
            The tools only read unless --allow-writes is given; then they may also change the \
            project (clean, fix, env init, db up/migrate/new, record save, bench, image). \
            db reset, db down --volumes, clean --all, env get and --reveal are never tools.",
    flags: &[Flag {
        name: "allow-writes",
        kind: Kind::Switch,
        help: "let the tools change the project too (clean, fix, env init, db up/migrate/new, \
               record save, bench, image); never db reset, db down --volumes, clean --all or secrets",
    }],
    max_words: 0,
    run,
};

const PROTOCOL: &str = "2025-06-18";

/// What one use of a tool does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Only reads: always allowed.
    Read,
    /// Changes the project in a way that can be undone or redone: allowed
    /// with `--allow-writes`.
    Write,
    /// Destroys data, hands out secrets or reaches outside: never a tool.
    Never,
}

/// What a command may do as a tool.
pub struct Policy {
    pub name: &'static str,
    /// What it reads (empty: it has no read-only use).
    pub reads: &'static str,
    /// What `--allow-writes` adds (empty: nothing).
    pub writes: &'static str,
    /// What it never does as a tool (empty: nothing is refused).
    pub never: &'static str,
    /// Whether an allowed write can delete or overwrite (MCP's destructiveHint).
    pub destructive: bool,
    /// Whether running it twice does what running it once does.
    pub idempotent: bool,
    /// The access of one use, from its words and the flags it sets.
    pub access: fn(words: &[&str], flags: &[&str]) -> Access,
}

/// The commands that are tools. `dev` and `dashboard` never return, `help`
/// and `version` say nothing an agent needs, `new` and `ai` write a whole
/// project, and `test` runs the project's code: none is a tool. A thread
/// that adds a command adds its policy here, or it is not served.
pub const POLICIES: &[Policy] = &[
    read_only("doctor", "checks this machine and project"),
    read_only("check", "lints the project"),
    read_only("explain", "explains a GIONG code"),
    read_only("routes", "lists the service's routes"),
    read_only("top", "shows a running service's load"),
    Policy {
        name: "openapi",
        reads: "prints the OpenAPI document",
        writes: "--out writes it to a file",
        never: "",
        destructive: true,
        idempotent: true,
        access: |_, flags| {
            if flags.contains(&"out") {
                Access::Write
            } else {
                Access::Read
            }
        },
    },
    Policy {
        name: "clean",
        reads: "--dry-run lists what would go",
        writes: "removes old builds",
        never: "--all",
        destructive: true,
        idempotent: true,
        access: |_, flags| {
            if flags.contains(&"all") {
                Access::Never
            } else if flags.contains(&"dry-run") {
                Access::Read
            } else {
                Access::Write
            }
        },
    },
    Policy {
        name: "fix",
        reads: "",
        writes: "applies the edits giong check offers",
        never: "",
        destructive: true,
        idempotent: true,
        access: |_, _| Access::Write,
    },
    Policy {
        name: "env",
        reads: "show (secrets masked) and check",
        writes: "init writes .env from .env.example",
        never: "get and --reveal",
        destructive: false,
        idempotent: true,
        access: |words, flags| {
            if flags.contains(&"reveal") {
                return Access::Never;
            }
            match words.first().copied() {
                None | Some("show" | "check") => Access::Read,
                Some("init") => Access::Write,
                Some(_) => Access::Never,
            }
        },
    },
    Policy {
        name: "db",
        reads: "status",
        writes: "up, migrate and new",
        never: "reset, down and anything else",
        destructive: false,
        idempotent: false,
        access: |words, _| match words.first().copied() {
            Some("status") => Access::Read,
            Some("up" | "migrate" | "new") => Access::Write,
            _ => Access::Never,
        },
    },
    Policy {
        name: "record",
        reads: "list and show",
        writes: "save writes a test from a recorded request",
        never: "anything else",
        destructive: true,
        idempotent: false,
        access: |words, _| match words.first().copied() {
            Some("list" | "show") => Access::Read,
            Some("save") => Access::Write,
            _ => Access::Never,
        },
    },
    Policy {
        name: "bench",
        reads: "",
        writes: "loads the project's service, or a URL on this machine, and writes results",
        never: "a URL that is not on this machine",
        destructive: false,
        idempotent: false,
        access: |words, _| match words.first() {
            Some(url) if !loopback_url(url) => Access::Never,
            _ => Access::Write,
        },
    },
    Policy {
        name: "image",
        reads: "",
        writes: "builds a container image (--load into the local engine)",
        never: "--push",
        destructive: true,
        idempotent: true,
        access: |_, flags| {
            if flags.contains(&"push") {
                Access::Never
            } else {
                Access::Write
            }
        },
    },
];

/// Whether a URL's host is this machine: 127.0.0.0/8, ::1 or localhost
/// (with or without a scheme, a port, a path or user information).
fn loopback_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

const fn read_only(name: &'static str, reads: &'static str) -> Policy {
    Policy {
        name,
        reads,
        writes: "",
        never: "",
        destructive: false,
        idempotent: true,
        access: |_, _| Access::Read,
    }
}

fn policy(name: &str) -> Option<&'static Policy> {
    POLICIES.iter().find(|p| p.name == name)
}

fn run(args: &Args) -> Result<Report, Failure> {
    // Standard output carries JSON-RPC: a report document there would break
    // the client, and printing nothing would look like success.
    if args.json() {
        return Err(Failure::new(
            Code::USAGE,
            "giong mcp answers in JSON-RPC on standard output, so --json does not apply to it",
            "run giong mcp without --json (claude mcp add giong -- giong mcp)",
        ));
    }
    let writes = args.has("allow-writes");
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| handle(m, writes)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(msg) => handle(&msg, writes),
            Err(e) => Some(error(Value::Null, -32700, &format!("not JSON: {e}"))),
        };
        if let Some(reply) = reply {
            if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
    // The client closed the connection: nothing to print.
    Ok(Report::new("mcp").quiet())
}

/// The answer to one message; `None` for a notification. `writes`: the
/// server was started with `--allow-writes`.
pub fn handle(msg: &Value, writes: bool) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg["method"].as_str().unwrap_or("");
    let Some(id) = id else {
        // Notifications (initialized, cancelled): nothing to answer.
        return None;
    };
    Some(match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"]
                .as_str()
                .unwrap_or(PROTOCOL);
            let version = if asked < PROTOCOL { asked } else { PROTOCOL };
            ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "giong", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": if writes {
                        "giong's commands as tools. Each answers with the command's JSON \
                        document: findings carry a GIONG-E code, a fix and, when giong fix can apply \
                        it, edits. Pass `dir` to run in a project other than the current directory. \
                        Writes are allowed (clean, fix, env init, db up/migrate/new, record save, \
                        bench, image); db reset, db down, clean --all and secrets are not tools: \
                        ask the person to run them."
                    } else {
                        "giong's commands as tools. Each answers with the command's JSON \
                        document: findings carry a GIONG-E code, a fix and, when giong fix can apply \
                        it, edits. Pass `dir` to run in a project other than the current directory. \
                        The tools only read: for a change (giong fix, clean, env init, db up, ...) \
                        tell the person the command, or ask them to restart giong mcp with \
                        --allow-writes."
                    },
                }),
            )
        }
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({"tools": tools(writes)})),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            let args = &msg["params"]["arguments"];
            match call(name, args, writes) {
                Ok(result) => ok(id, result),
                Err(why) => error(id, -32602, &why),
            }
        }
        _ => error(id, -32601, &format!("giong mcp has no method {method:?}")),
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The commands served as tools, with their policies: those with a policy,
/// and with `writes` false only those with a read-only use.
fn tool_commands(writes: bool) -> impl Iterator<Item = (&'static Command, &'static Policy)> {
    COMMANDS.iter().filter_map(move |c| {
        let p = policy(c.name)?;
        (writes || !p.reads.is_empty()).then_some((*c, p))
    })
}

/// One tool per command, its input schema from the command's flags.
pub fn tools(writes: bool) -> Vec<Value> {
    let mut out: Vec<Value> = tool_commands(writes)
        .map(|(c, p)| {
            let mut props = Map::new();
            for f in c.flags {
                let ty = match f.kind {
                    Kind::Switch => "boolean",
                    Kind::Value => "string",
                };
                props.insert(f.name.to_string(), json!({"type": ty, "description": f.help}));
            }
            if c.max_words > 0 {
                props.insert(
                    "words".into(),
                    json!({"type": "array", "items": {"type": "string"}, "maxItems": c.max_words,
                           "description": "the words after the command (see the usage)"}),
                );
            }
            if c.usage.contains("-- ") {
                props.insert(
                    "passthrough".into(),
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "arguments after --, passed on untouched"}),
                );
            }
            props.insert(
                "dir".into(),
                json!({"type": "string", "description": "the project directory (default: where giong mcp runs)"}),
            );
            let writes_here = writes && !p.writes.is_empty();
            let mut limits = String::new();
            if !p.reads.is_empty() {
                limits += &format!("Reads: {}.", p.reads);
            }
            if !p.writes.is_empty() {
                limits += &if writes {
                    format!(" Writes: {}.", p.writes)
                } else {
                    format!(" Not as a tool here (giong mcp --allow-writes): {}.", p.writes)
                };
            }
            if !p.never.is_empty() {
                limits += &format!(" Never as a tool: {}.", p.never);
            }
            json!({
                "name": c.name,
                "title": format!("giong {}", c.name),
                "description": format!("{}.\n\n{}\n\n{}", c.summary, c.usage, limits.trim()),
                "inputSchema": {"type": "object", "properties": props, "additionalProperties": false},
                "annotations": {
                    "readOnlyHint": !writes_here,
                    "destructiveHint": writes_here && p.destructive,
                    "idempotentHint": !writes_here || p.idempotent,
                    "openWorldHint": false,
                },
            })
        })
        .collect();
    out.push(json!({
        "name": "dev_status",
        "title": "giong dev status",
        "description": "What a running giong dev serves: its pid, the service's pid and addresses, \
            the last build (state, errors, first error) and the log file. Start giong dev in a \
            terminal of its own; this reads .giong/dev.json.",
        "inputSchema": {"type": "object", "properties": {
            "dir": {"type": "string", "description": "the project directory (default: where giong mcp runs)"}
        }, "additionalProperties": false},
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true,
                        "openWorldHint": false},
    }));
    out
}

/// Runs a tool: `giong <name> --json ...` in a child process (a command
/// that fails cannot take the server down), answering with its document.
pub fn call(name: &str, args: &Value, writes: bool) -> Result<Value, String> {
    let empty = Map::new();
    let args = match args {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err("arguments must be an object".into()),
    };
    let dir = match args.get("dir") {
        Some(Value::String(d)) => Some(PathBuf::from(d)),
        None | Some(Value::Null) => None,
        Some(_) => return Err("dir must be a string".into()),
    };
    if name == "dev_status" {
        return Ok(dev_status(dir));
    }
    // Every command with a policy is looked up, so that a write asked of a
    // read-only server says how to allow it.
    let Some((command, policy)) = tool_commands(true).find(|(c, _)| c.name == name) else {
        return Err(format!("giong mcp has no tool {name:?}"));
    };
    let mut argv = vec![command.name.to_string(), "--json".to_string()];
    let mut words: Vec<&str> = Vec::new();
    let mut set: Vec<&str> = Vec::new();
    let mut passthrough = Vec::new();
    for (key, value) in args {
        match key.as_str() {
            "dir" => {}
            "words" => {
                let given = value
                    .as_array()
                    .ok_or("words must be an array of strings")?;
                for w in given {
                    let w = w.as_str().ok_or("words must be an array of strings")?;
                    if w.starts_with('-') {
                        return Err(format!("{w:?} is a flag, not a word"));
                    }
                    words.push(w);
                    argv.push(w.to_string());
                }
            }
            "passthrough" if command.usage.contains("-- ") => {
                let items = value
                    .as_array()
                    .ok_or("passthrough must be an array of strings")?;
                for p in items {
                    passthrough.push(
                        p.as_str()
                            .ok_or("passthrough must be an array of strings")?
                            .to_string(),
                    );
                }
            }
            flag => {
                let Some(f) = command.flags.iter().find(|f| f.name == flag) else {
                    return Err(format!("giong {} has no option {flag:?}", command.name));
                };
                match (f.kind, value) {
                    (Kind::Switch, Value::Bool(true)) => argv.push(format!("--{flag}")),
                    (Kind::Switch, Value::Bool(false) | Value::Null) => continue,
                    (Kind::Value, Value::String(v)) => argv.push(format!("--{flag}={v}")),
                    (Kind::Value, Value::Number(n)) => argv.push(format!("--{flag}={n}")),
                    (Kind::Value, Value::Null) => continue,
                    _ => return Err(format!("{flag} has the wrong type")),
                }
                set.push(f.name);
            }
        }
    }
    let shown = std::iter::once(command.name)
        .chain(words.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    match (policy.access)(&words, &set) {
        Access::Read => {}
        Access::Write if writes => {}
        Access::Write => {
            return Err(format!(
                "giong mcp was started read-only, and giong {shown} changes the project: tell \
                 the person to run it, or to restart giong mcp with --allow-writes"
            ))
        }
        Access::Never => {
            return Err(format!(
                "giong {shown} with these options is never run as a tool (it can destroy data \
                 or reveal secrets: {}); tell the person to run it themselves",
                policy.never
            ))
        }
    }
    if !passthrough.is_empty() {
        argv.push("--".into());
        argv.extend(passthrough);
    }
    let exe = std::env::current_exe().map_err(|e| format!("giong cannot find itself: {e}"))?;
    let mut cmd = Process::new(exe);
    cmd.args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = &dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("giong {} could not run: {e}", command.name))?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let doc: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
        json!({"command": command.name, "ok": false,
               "error": {"message": crate::build::strip_ansi(String::from_utf8_lossy(&out.stderr).trim())}})
    });
    // Exit status 2: the command could not do its job (a tool error);
    // 1: it ran and found errors (a result like any other).
    let failed = out.status.code() == Some(2) || doc.get("error").is_some();
    Ok(json!({
        "content": [{"type": "text", "text": doc.to_string()}],
        "structuredContent": doc,
        "isError": failed,
    }))
}

fn dev_status(dir: Option<PathBuf>) -> Value {
    let dir = dir
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let root = crate::project::find_manifest_dir(&dir).unwrap_or(dir);
    let doc = match std::fs::read_to_string(root.join(".giong").join("dev.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        // A giong dev that was killed leaves its file behind.
        Some(v) if v["pid"].as_u64().is_some_and(super::dev::alive) => {
            json!({"running": true, "dev": v})
        }
        Some(_) => json!({"running": false,
                          "hint": "the last giong dev here was stopped: start it again in a terminal of its own"}),
        None => json!({"running": false,
                       "hint": "giong dev is not running in this project: start it in a terminal of its own"}),
    };
    json!({"content": [{"type": "text", "text": doc.to_string()}], "structuredContent": doc, "isError": false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_and_the_tool_list() {
        let init = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t"}}}), false)
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            false
        )
        .is_none());
        let list = handle(
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            true,
        )
        .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"doctor") && names.contains(&"check") && names.contains(&"dev_status")
        );
        assert!(!names.contains(&"dev") && !names.contains(&"mcp"));
        assert!(!names.contains(&"dashboard"), "it serves until stopped");
        assert!(names.contains(&"record") && names.contains(&"openapi"));
        let clean = &list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "clean")
            .unwrap()["inputSchema"];
        assert_eq!(clean["properties"]["all"]["type"], "boolean");
        assert_eq!(clean["properties"]["days"]["type"], "string");
        let bad = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}), false).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
    }

    #[test]
    fn bad_arguments_are_refused_before_running() {
        assert!(call("clean", &json!({"bogus": true}), true).is_err());
        assert!(call("clean", &json!({"all": "yes"}), true).is_err());
        assert!(call("explain", &json!({"words": ["--all"]}), true).is_err());
        assert!(call("nope", &json!({}), true).is_err());
        assert!(call("dev", &json!({}), true).is_err());
        // No passthrough where the command takes none.
        assert!(call("clean", &json!({"passthrough": ["--all"]}), true).is_err());
    }

    fn names(writes: bool) -> Vec<String> {
        tools(writes)
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn tool(writes: bool, name: &str) -> Value {
        tools(writes)
            .into_iter()
            .find(|t| t["name"] == name)
            .unwrap()
    }

    #[test]
    fn the_tools_only_read_unless_writes_are_allowed() {
        // fix has no read-only use: listed only with --allow-writes.
        let ro = names(false);
        assert!(ro.contains(&"doctor".into()) && ro.contains(&"clean".into()));
        for t in tools(false) {
            assert_eq!(t["annotations"]["readOnlyHint"], true, "{}", t["name"]);
            assert_eq!(t["annotations"]["destructiveHint"], false, "{}", t["name"]);
        }
        let clean = tool(true, "clean");
        assert_eq!(clean["annotations"]["readOnlyHint"], false);
        assert_eq!(clean["annotations"]["destructiveHint"], true);
        assert_eq!(tool(true, "doctor")["annotations"]["readOnlyHint"], true);
        assert!(tool(false, "clean")["description"]
            .as_str()
            .unwrap()
            .contains("--allow-writes"));
        // Read-only: a write is refused before anything runs, and says how
        // to allow it.
        let e = call("clean", &json!({}), false).unwrap_err();
        assert!(e.contains("--allow-writes"), "{e}");
        // Never, whatever the mode.
        let e = call("clean", &json!({"all": true}), true).unwrap_err();
        assert!(e.contains("never"), "{e}");
    }

    #[test]
    fn destructive_uses_and_secrets_are_never_tools() {
        let access = |name: &str, words: &[&str], flags: &[&str]| {
            (policy(name).unwrap().access)(words, flags)
        };
        assert_eq!(access("db", &["reset"], &["yes"]), Access::Never);
        assert_eq!(access("db", &["down"], &["volumes"]), Access::Never);
        assert_eq!(access("db", &["down"], &[]), Access::Never);
        assert_eq!(access("db", &[], &[]), Access::Never);
        assert_eq!(access("db", &["status"], &[]), Access::Read);
        assert_eq!(access("db", &["migrate"], &[]), Access::Write);
        assert_eq!(access("env", &["get"], &[]), Access::Never);
        assert_eq!(access("env", &[], &["reveal"]), Access::Never);
        assert_eq!(access("env", &["show"], &["reveal"]), Access::Never);
        assert_eq!(access("env", &[], &[]), Access::Read);
        assert_eq!(access("env", &["check"], &[]), Access::Read);
        assert_eq!(access("env", &["init"], &[]), Access::Write);
        assert_eq!(access("clean", &[], &["all"]), Access::Never);
        assert_eq!(access("clean", &[], &["dry-run"]), Access::Read);
        assert_eq!(access("clean", &[], &["days"]), Access::Write);
        assert_eq!(access("record", &["show"], &[]), Access::Read);
        assert_eq!(access("record", &["save"], &[]), Access::Write);
        assert_eq!(access("record", &["drop"], &[]), Access::Never);
        assert_eq!(access("image", &[], &["push"]), Access::Never);
        assert_eq!(access("openapi", &[], &["out"]), Access::Write);
        assert_eq!(access("bench", &[], &["rate"]), Access::Write);
        for here in [
            "http://127.0.0.1:8080/users",
            "127.0.0.2:9000",
            "http://[::1]:8080",
            "https://LOCALHOST/",
            "http://u:p@localhost:1",
        ] {
            assert_eq!(access("bench", &[here], &[]), Access::Write, "{here}");
        }
        for away in [
            "https://example.com",
            "http://10.0.0.5:8080",
            "http://localhost.example.com",
            "http://127.0.0.1@example.com/",
            "http://[2001:db8::1]/",
        ] {
            assert_eq!(access("bench", &[away], &[]), Access::Never, "{away}");
        }
        // Commands with no policy are not tools.
        for name in [
            "new",
            "ai",
            "test",
            "dev",
            "dashboard",
            "help",
            "version",
            "mcp",
        ] {
            assert!(policy(name).is_none(), "{name}");
        }
    }
}
