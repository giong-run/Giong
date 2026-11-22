//! `giong mcp`: the commands as MCP tools over stdio. Agents talk to it
//! without a person watching, so every line on stdout must be a JSON-RPC
//! message, a bad line must not end the session, and a tool's answer must be
//! the same report the command prints with `--json`.

use serde_json::{json, Value};

use crate::harness::*;

/// Commands that are not tools: the server itself, the long-running `dev`
/// (its state is the `dev_status` tool), and the two about giong itself.
const NOT_TOOLS: &[&str] = &["mcp", "dev", "help", "version"];

/// Sends `lines` to a fresh `giong mcp` in `dir`, closes its input, and
/// returns every line it printed, each parsed as JSON.
fn session(dir: &std::path::Path, lines: &[String]) -> (Out, Vec<Value>) {
    let mut input = lines.join("\n");
    input.push('\n');
    let out = giong(["mcp"])
        .dir(dir)
        .stdin(input.into_bytes())
        .timeout(std::time::Duration::from_secs(120))
        .run();
    let msgs = out
        .stdout
        .lines()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!("a stdout line is not JSON ({e}): {l:?}\n{}", out.dump())
            })
        })
        .collect();
    (out, msgs)
}

fn req(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

fn initialize() -> String {
    req(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "giong-e2e", "version": "1"}}),
    )
}

fn by_id(msgs: &[Value], id: u64) -> &Value {
    msgs.iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("no answer to request {id}: {msgs:#?}"))
}

#[test]
fn it_initializes_and_lists_a_tool_for_every_command() {
    if !has_command("mcp") {
        return;
    }
    let dir = TempDir::new("mcp-list");
    let (out, msgs) = session(
        dir.path(),
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
            req(2, "tools/list", json!({})),
        ],
    );
    out.expect_code(0);
    // A notification has no answer: two requests, two messages.
    assert_eq!(msgs.len(), 2, "{msgs:#?}");
    let init = &by_id(&msgs, 1)["result"];
    assert_eq!(init["serverInfo"]["version"], VERSION, "{init:#}");
    assert!(init["protocolVersion"].is_string());

    let tools = by_id(&msgs, 2)["result"]["tools"]
        .as_array()
        .expect("tools/list gives result.tools")
        .clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for c in commands() {
        if !NOT_TOOLS.contains(&c.as_str()) {
            assert!(
                names.contains(&c.as_str()),
                "`giong {c}` is not an MCP tool (tools: {names:?})"
            );
        }
    }
    for t in &tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t:#}");
        assert!(
            !t["description"].as_str().unwrap_or("").trim().is_empty(),
            "{t:#}"
        );
    }
}

/// The tool answers with the command's own report, which follows the same
/// contract as `giong <command> --json`.
#[test]
fn a_tool_answers_with_the_commands_report() {
    if !has_command("mcp") {
        return;
    }
    let dir = TempDir::new("mcp-call");
    new_package(&dir, "mcpcall", "");
    let (_, msgs) = session(
        dir.path(),
        &[
            initialize(),
            req(
                2,
                "tools/call",
                json!({"name": "explain", "arguments": {"words": ["E5000"]}}),
            ),
            req(3, "tools/call", json!({"name": "doctor", "arguments": {}})),
        ],
    );
    for (id, command) in [(2, "explain"), (3, "doctor")] {
        let r = &by_id(&msgs, id)["result"];
        let text = r["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{command}: no text content: {r:#}"));
        let v: Value = serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("{command}: the tool text is not JSON ({e}): {text}"));
        crate::schema::check_report(&v, command)
            .unwrap_or_else(|e| panic!("{command}: {e}\n{text}"));
        // The MCP error flag agrees with the report.
        let is_error = r["isError"].as_bool().unwrap_or(false);
        assert_eq!(is_error, v.get("error").is_some(), "{command}: {r:#}");
    }
}

#[test]
fn bad_input_gets_a_json_rpc_error_and_the_session_goes_on() {
    if !has_command("mcp") {
        return;
    }
    let dir = TempDir::new("mcp-bad");
    let huge = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"{}\"}}",
        "x".repeat(1 << 20)
    );
    let (out, msgs) = session(
        dir.path(),
        &[
            initialize(),
            "this is not json".to_string(),
            "{\"jsonrpc\":\"2.0\",\"id\":3,".to_string(),
            req(4, "no/such/method", json!({})),
            req(
                5,
                "tools/call",
                json!({"name": "no-such-tool", "arguments": {}}),
            ),
            req(
                6,
                "tools/call",
                json!({"name": "explain", "arguments": {"no_such_option": true}}),
            ),
            huge,
            req(7, "tools/list", json!({})),
        ],
    );
    out.expect_code(0);
    assert!(!out.stderr.contains("panicked"), "{}", out.dump());
    let parse_errors = msgs.iter().filter(|m| m["error"]["code"] == -32700).count();
    assert!(
        parse_errors >= 2,
        "two broken lines, {parse_errors} parse errors: {msgs:#?}"
    );
    assert_eq!(by_id(&msgs, 4)["error"]["code"], -32601);
    for id in [5, 6] {
        let m = by_id(&msgs, id);
        assert!(
            m.get("error").is_some() || m["result"]["isError"] == true,
            "request {id} was not refused: {m:#}"
        );
    }
    assert!(by_id(&msgs, 9).get("error").is_some());
    // Still answering after all that.
    assert!(by_id(&msgs, 7)["result"]["tools"].is_array());
}

/// `giong mcp` speaks JSON-RPC on stdout, so `--json` cannot apply: it is a
/// usage error, not a silent exit 0 with nothing printed.
#[test]
#[ignore = "known bug: giong mcp --json exits 0 and prints nothing (want a GIONG-E5000 document)"]
fn json_is_refused_with_a_usage_error() {
    if !has_command("mcp") {
        return;
    }
    let dir = TempDir::new("mcp-json");
    let out = giong(["mcp", "--json"])
        .dir(dir.path())
        .stdin(Vec::new())
        .run();
    let v = out.report("mcp");
    assert_eq!(v["error"]["code"], "GIONG-E5000", "{}", out.dump());
}
