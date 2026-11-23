//! `giong mcp`: the commands as MCP tools over stdio. Agents talk to it
//! without a person watching, so every line on stdout must be a JSON-RPC
//! message, a bad line must not end the session, and a tool's answer must be
//! the same report the command prints with `--json`.

use serde_json::{json, Value};

use crate::harness::*;

/// Commands that are not tools: the server itself, the ones that serve until
/// stopped (`dev`, whose state is the `dev_status` tool, and `dashboard`),
/// and the two about giong itself.
const NOT_TOOLS: &[&str] = &[
    "mcp",
    "dev",
    "dashboard",
    "help",
    "version",
    "new",
    "ai",
    "test",
];

/// Commands that only change files, containers or machines: tools only with
/// `giong mcp --allow-writes` (security audit point 3). Others, like
/// `clean`, are listed read-only and refuse their writes.
const WRITE_TOOLS: &[&str] = &["fix", "image", "bench"];

/// Sends `lines` to a fresh `giong mcp` in `dir`, closes its input, and
/// returns every line it printed, each parsed as JSON.
fn session(dir: &std::path::Path, lines: &[String]) -> (Out, Vec<Value>) {
    session_with(dir, &[], lines)
}

fn session_with(dir: &std::path::Path, flags: &[&str], lines: &[String]) -> (Out, Vec<Value>) {
    let mut input = lines.join("\n");
    input.push('\n');
    let mut run = giong(["mcp"]);
    for f in flags {
        run = run.arg(f);
    }
    let out = run
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
        if !NOT_TOOLS.contains(&c.as_str()) && !WRITE_TOOLS.contains(&c.as_str()) {
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

/// Security point 3: an agent with `giong mcp` and no person watching gets
/// read-only tools by default. Commands that write, build images or load a
/// server need `--allow-writes`, and every tool says what it does with MCP's
/// annotations.
#[test]
fn the_default_tools_are_read_only_and_say_so() {
    if !has_command("mcp") {
        return;
    }
    let dir = TempDir::new("mcp-readonly");
    let (out, msgs) = session(dir.path(), &[initialize(), req(2, "tools/list", json!({}))]);
    out.expect_code(0);
    let tools = by_id(&msgs, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .clone();
    for t in &tools {
        let name = t["name"].as_str().unwrap();
        assert!(
            !WRITE_TOOLS.contains(&name),
            "`{name}` is a tool without --allow-writes"
        );
        assert_eq!(
            t["annotations"]["readOnlyHint"], true,
            "`{name}` is not marked read-only: {t:#}"
        );
        for hint in ["destructiveHint", "idempotentHint"] {
            assert!(
                t["annotations"][hint].is_boolean(),
                "`{name}` has no {hint}: {t:#}"
            );
        }
    }
}

/// Security point 3: a write on a read-only server is refused with the way
/// to allow it, and what destroys data is refused even with
/// `--allow-writes`. Nothing is deleted either way.
#[test]
fn writes_need_allow_writes_and_destruction_is_never_a_tool() {
    if !has_command("mcp") || !has_command("clean") {
        return;
    }
    let dir = TempDir::new("mcp-writes");
    new_package(&dir, "mcpw", "");
    let kept = dir.write("target/debug/keep-me", "build output");
    let refused = |m: &Value| m.get("error").is_some() || m["result"]["isError"] == true;

    let (_, msgs) = session(
        dir.path(),
        &[
            initialize(),
            req(2, "tools/call", json!({"name": "clean", "arguments": {}})),
        ],
    );
    let m = by_id(&msgs, 2);
    assert!(refused(m), "a read-only server cleaned: {m:#}");
    assert!(m.to_string().contains("--allow-writes"), "{m:#}");
    assert!(kept.exists(), "a read-only server deleted build output");

    let (_, msgs) = session_with(
        dir.path(),
        &["--allow-writes"],
        &[
            initialize(),
            req(2, "tools/list", json!({})),
            req(
                3,
                "tools/call",
                json!({"name": "clean", "arguments": {"all": true}}),
            ),
        ],
    );
    let tools = by_id(&msgs, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .clone();
    let clean = tools
        .iter()
        .find(|t| t["name"] == "clean")
        .expect("clean is a tool");
    assert_eq!(clean["annotations"]["readOnlyHint"], false, "{clean:#}");
    let m = by_id(&msgs, 3);
    assert!(refused(m), "clean --all ran over MCP: {m:#}");
    assert!(kept.exists(), "clean --all deleted build output over MCP");
}

/// Security point 3: what reaches the model through `giong mcp` is masked.
/// `env get` and `--reveal` print secrets for a person at a terminal; over
/// MCP they are refused or masked.
#[test]
fn the_env_tool_never_hands_a_secret_to_the_model() {
    if !has_command("mcp") || !has_command("env") {
        return;
    }
    let dir = TempDir::new("mcp-env");
    new_package(&dir, "mcpenv", "");
    dir.write(".env.example", "E2E_MCP_TOKEN=\n");
    dir.write(".env", "E2E_MCP_TOKEN=mcp-secret-77\n");
    let (out, msgs) = session(
        dir.path(),
        &[
            initialize(),
            req(
                2,
                "tools/call",
                json!({"name": "env", "arguments": {"reveal": true}}),
            ),
            req(
                3,
                "tools/call",
                json!({"name": "env", "arguments": {"words": ["get", "E2E_MCP_TOKEN"]}}),
            ),
            req(4, "tools/call", json!({"name": "env", "arguments": {}})),
        ],
    );
    out.expect_code(0);
    for id in [2, 3, 4] {
        let m = by_id(&msgs, id).to_string();
        assert!(
            !m.contains("mcp-secret-77"),
            "request {id} leaked the secret: {m}"
        );
    }
}
