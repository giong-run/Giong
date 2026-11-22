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
//! The protocol: JSON-RPC 2.0, one message per line (MCP's stdio transport),
//! version 2025-06-18 (older clients get the version they ask for).

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};

use serde_json::{json, Map, Value};

use super::{Command, COMMANDS};
use crate::args::{Args, Kind};
use crate::code::Code;
use crate::report::{Failure, Report};

pub const COMMAND: Command = Command {
    name: "mcp",
    summary: "serve every giong command as a tool to coding agents (MCP over stdio)",
    usage: "giong mcp\n\n\
            Speaks the Model Context Protocol on standard input and output: one tool per giong \
            command (doctor, check, explain, clean, ...), each answering with the command's JSON \
            document, and dev_status for a running giong dev. Register it with your agent: \
            claude mcp add giong -- giong mcp (giong ai init writes .mcp.json for you).",
    flags: &[],
    max_words: 0,
    run,
};

const PROTOCOL: &str = "2025-06-18";
/// Commands an agent cannot run as a tool: they never return, or are this.
const NOT_TOOLS: &[&str] = &["dev", "dashboard", "mcp", "help", "version"];

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
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(handle).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(msg) => handle(&msg),
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

/// The answer to one message; `None` for a notification.
pub fn handle(msg: &Value) -> Option<Value> {
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
                    "instructions": "giong's commands as tools. Each answers with the command's JSON \
                        document: findings carry a GIONG-E code, a fix and, when giong fix can apply \
                        it, edits. Pass `dir` to run in a project other than the current directory.",
                }),
            )
        }
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({"tools": tools()})),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            let args = &msg["params"]["arguments"];
            match call(name, args) {
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

fn tool_commands() -> impl Iterator<Item = &'static &'static Command> {
    COMMANDS.iter().filter(|c| !NOT_TOOLS.contains(&c.name))
}

/// One tool per command, its input schema from the command's flags.
pub fn tools() -> Vec<Value> {
    let mut out: Vec<Value> = tool_commands()
        .map(|c| {
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
            json!({
                "name": c.name,
                "title": format!("giong {}", c.name),
                "description": format!("{}.\n\n{}", c.summary, c.usage),
                "inputSchema": {"type": "object", "properties": props, "additionalProperties": false},
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
    }));
    out
}

/// Runs a tool: `giong <name> --json ...` in a child process (a command
/// that fails cannot take the server down), answering with its document.
pub fn call(name: &str, args: &Value) -> Result<Value, String> {
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
    let Some(command) = tool_commands().find(|c| c.name == name) else {
        return Err(format!("giong mcp has no tool {name:?}"));
    };
    let mut argv = vec![command.name.to_string(), "--json".to_string()];
    let mut passthrough = Vec::new();
    for (key, value) in args {
        match key.as_str() {
            "dir" => {}
            "words" => {
                let words = value
                    .as_array()
                    .ok_or("words must be an array of strings")?;
                for w in words {
                    let w = w.as_str().ok_or("words must be an array of strings")?;
                    if w.starts_with('-') {
                        return Err(format!("{w:?} is a flag, not a word"));
                    }
                    argv.push(w.to_string());
                }
            }
            "passthrough" => {
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
                    (Kind::Switch, Value::Bool(false) | Value::Null) => {}
                    (Kind::Value, Value::String(v)) => argv.push(format!("--{flag}={v}")),
                    (Kind::Value, Value::Number(n)) => argv.push(format!("--{flag}={n}")),
                    (Kind::Value, Value::Null) => {}
                    _ => return Err(format!("{flag} has the wrong type")),
                }
            }
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
               "error": {"message": String::from_utf8_lossy(&out.stderr).trim()}})
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
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t"}}}))
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(
            handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none()
        );
        let list = handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
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
        let bad = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap();
        assert_eq!(bad["error"]["code"], -32601);
    }

    #[test]
    fn bad_arguments_are_refused_before_running() {
        assert!(call("clean", &json!({"bogus": true})).is_err());
        assert!(call("clean", &json!({"all": "yes"})).is_err());
        assert!(call("explain", &json!({"words": ["--all"]})).is_err());
        assert!(call("nope", &json!({})).is_err());
    }
}
