//! giong-mcp end to end: a runtime with two workers, the server on a
//! loopback port, and a plain HTTP client on the test thread.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use giong::error::Code;
use giong::observe::{self, Source};
use giong_mcp::Config;
use serde_json::{json, Value};

/// One HTTP/1.1 request; returns the status and the body.
fn http(addr: SocketAddr, method: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "{method} /mcp HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    let mut has_host = false;
    for (k, v) in headers {
        has_host |= k.eq_ignore_ascii_case("host");
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_host {
        req.push_str(&format!("Host: {addr}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (status, body)
}

const JSON: (&str, &str) = ("Content-Type", "application/json");

fn rpc(addr: SocketAddr, method: &str, params: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    let (status, body) = http(addr, "POST", &[JSON], &body);
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Calls a tool and returns its structured result, after checking it
/// against the output schema the tool declares.
fn tool(addr: SocketAddr, name: &str, args: Value) -> Value {
    let reply = rpc(
        addr,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    );
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{reply}");
    // The same JSON as text, for clients that read only `content`, and
    // compact: models pay for every byte.
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains('\n'), "{text}");
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed, result["structuredContent"]);
    let tools = rpc(addr, "tools/list", json!({}));
    let schema = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .map(|t| t["outputSchema"].clone())
        .unwrap();
    if let Err(e) = conforms(&result["structuredContent"], &schema, name) {
        panic!("the {name} answer does not match its outputSchema: {e}\n{reply}");
    }
    result["structuredContent"].clone()
}

/// The part of JSON Schema the output schemas use: `type` (a name or a
/// list), `properties`, `required`, `additionalProperties`, `items`, `enum`
/// and `minimum`.
fn conforms(value: &Value, schema: &Value, at: &str) -> Result<(), String> {
    let types: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().map(|t| t.as_str().unwrap()).collect(),
        Value::Null => vec![],
        other => return Err(format!("{at}: bad type {other}")),
    };
    let is = |t: &str| match t {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_u64() || value.is_i64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    };
    if !types.is_empty() && !types.iter().any(|t| is(t)) {
        return Err(format!("{at}: {value} is not {types:?}"));
    }
    if let Some(options) = schema["enum"].as_array() {
        if !options.contains(value) {
            return Err(format!("{at}: {value} is not one of {options:?}"));
        }
    }
    if let (Some(min), Some(n)) = (schema["minimum"].as_f64(), value.as_f64()) {
        if n < min {
            return Err(format!("{at}: {n} < {min}"));
        }
    }
    if let Some(object) = value.as_object() {
        for key in schema["required"].as_array().into_iter().flatten() {
            if !object.contains_key(key.as_str().unwrap()) {
                return Err(format!("{at}: {key} is missing"));
            }
        }
        for (key, v) in object {
            let sub = &schema["properties"][key];
            if !sub.is_null() {
                conforms(v, sub, &format!("{at}.{key}"))?;
            } else if schema["additionalProperties"].is_object() {
                conforms(v, &schema["additionalProperties"], &format!("{at}.{key}"))?;
            } else if schema["additionalProperties"] == false {
                return Err(format!("{at}: {key} is not allowed"));
            }
        }
    }
    if let (Some(items), Some(list)) = (schema.get("items"), value.as_array()) {
        for (i, v) in list.iter().enumerate() {
            conforms(v, items, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

struct TestPool;

impl Source for TestPool {
    fn kind(&self) -> &'static str {
        "test pool"
    }
    fn name(&self) -> String {
        "db:5432/app".into()
    }
    fn stats(&self) -> Vec<(&'static str, u64)> {
        vec![("connections", 4), ("waiting", 1)]
    }
}

/// A server on a free loopback port that needs no token.
fn open() -> Config {
    Config::tcp("127.0.0.1:0").allow_unauthenticated()
}

/// A two-worker runtime with an MCP server, until the closure returns.
fn with_service(cfg: Config, f: impl FnOnce(SocketAddr, giong::Handle) + Send + 'static) {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    let server = cfg.start(&handle).unwrap();
    let addr = server.local_addr().unwrap();
    assert_eq!(server.url(), format!("http://{addr}/mcp"));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let h = handle.clone();
    let test = thread::spawn(move || {
        // A failed assertion still stops the service, so that the test
        // fails instead of hanging.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(addr, h.clone())));
        h.shutdown();
        let _ = done_tx.send(());
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    });
    rt.run(|_| async {
        // A pool-like source on every worker.
        let pool = Rc::new(TestPool);
        observe::register_rc(&pool);
        giong::shutdown_signal().await;
        drop(pool);
    })
    .unwrap();
    done_rx.recv().unwrap();
    test.join().unwrap();
    server.stop();
}

#[test]
fn initialize_and_list_the_tools() {
    with_service(open(), |addr, _| {
        let reply = rpc(
            addr,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        );
        let result = &reply["result"];
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["serverInfo"]["name"], "giong-mcp");
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["instructions"]
            .as_str()
            .unwrap()
            .contains("overview"));

        // The client's notification gets 202 and no body.
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
        let (status, body) = http(
            addr,
            "POST",
            &[JSON, ("MCP-Protocol-Version", "2025-06-18")],
            &note,
        );
        assert_eq!((status, body.as_str()), (202, ""));

        let tools = rpc(addr, "tools/list", json!({}));
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["overview", "workers", "tasks", "sources", "profile"]
        );
        for t in tools["result"]["tools"].as_array().unwrap() {
            // `profile` changes what the workers do while it measures.
            assert_eq!(t["annotations"]["readOnlyHint"], t["name"] != "profile");
            assert_eq!(t["inputSchema"]["type"], "object");
            assert_eq!(t["outputSchema"]["type"], "object");
        }
        assert_eq!(rpc(addr, "ping", json!({}))["result"], json!({}));
    });
}

#[test]
fn overview_workers_tasks_and_sources() {
    with_service(open(), |addr, handle| {
        let line = line!() + 2;
        let parked: Vec<_> = (0..7)
            .map(|i| handle.spawn_on(i % 2, || giong::time::sleep(Duration::from_secs(60))))
            .collect();
        // Let the watcher take a few samples.
        thread::sleep(Duration::from_millis(350));

        let o = tool(addr, "overview", json!({}));
        assert_eq!(o["runtime"]["workers"], 2);
        assert_eq!(o["process"]["pid"], std::process::id());
        assert!(o["process"]["rss_bytes"].as_u64().unwrap() > 0);
        assert_eq!(o["workers"].as_array().unwrap().len(), 2);
        let choice = giong::Builder::new()
            .block_on(async { giong::driver_choice() })
            .unwrap();
        assert_eq!(o["workers"][0]["driver"], choice.kind.name());
        let c = &o["workers"][0]["driver_choice"];
        assert!(c["chosen_by"].is_string(), "{o}");
        assert_eq!(
            c["io_uring_error"].as_str(),
            choice.io_uring_error.as_deref()
        );
        // Where io_uring is blocked (CI's Docker job), giong says why it runs
        // on epoll; anywhere else, nothing is wrong.
        let warnings = o["warnings"].as_array().unwrap();
        match &choice.io_uring_error {
            Some(e) => {
                assert_eq!(warnings.len(), 1, "{o}");
                let w = warnings[0].as_str().unwrap();
                assert!(
                    w.starts_with("2 of 2 workers run on the epoll driver"),
                    "{w}"
                );
                assert!(w.ends_with(e.as_str()), "{w}");
            }
            None => assert!(warnings.is_empty(), "{o}"),
        }

        let w = tool(addr, "workers", json!({}));
        let w0 = &w["workers"][0];
        assert_eq!(w0["tasks"], 4, "{w0}");
        assert_eq!(w0["timers"], 4, "{w0}");
        assert!(w0["recv_buffers"]["total"].as_u64().unwrap() > 0);
        assert!(w0["last_second"]["busy"].as_f64().is_some(), "{w0}");
        assert_eq!(w0["blocked"], Value::Null);

        let t = tool(addr, "tasks", json!({}));
        assert_eq!(t["total_tasks"], 7, "{t}");
        let row = &t["by_location"][0];
        let loc = row["location"].as_str().unwrap();
        assert!(
            loc.contains("tests/mcp.rs") && loc.contains(&format!(":{line}:")),
            "{loc}"
        );
        assert_eq!(row["tasks"], 7);
        assert_eq!(row["per_worker"], json!([4, 3]));
        let t1 = tool(addr, "tasks", json!({ "worker": 1, "limit": 1 }));
        assert_eq!(t1["total_tasks"], 3);
        assert_eq!(t1["by_location"].as_array().unwrap().len(), 1);
        // JSON Schema counts 1.0 as an integer, and some clients send it.
        let t1 = tool(addr, "tasks", json!({ "worker": 1.0, "limit": 1.0 }));
        assert_eq!(t1["total_tasks"], 3);

        let s = tool(addr, "sources", json!({}));
        let src = &s["sources"][0];
        assert_eq!(src["kind"], "test pool");
        assert_eq!(src["name"], "db:5432/app");
        assert_eq!(src["total"], json!({ "connections": 8, "waiting": 2 }));
        assert_eq!(src["per_worker"].as_array().unwrap().len(), 2);

        for p in parked {
            p.abort();
        }
    });
}

#[test]
fn a_blocked_worker_is_reported_with_its_task() {
    with_service(
        open().snapshot_timeout(Duration::from_millis(200)),
        |addr, handle| {
            let line = line!() + 1;
            let blocker = handle.spawn_on(1, || async {
                std::thread::sleep(Duration::from_millis(1500));
            });
            thread::sleep(Duration::from_millis(500));

            let o = tool(addr, "overview", json!({}));
            let warnings = not_about_the_driver(&o);
            assert_eq!(warnings.len(), 1, "{o}");
            let warning = warnings[0];
            assert!(
                warning.starts_with("worker 1 has been inside one poll"),
                "{warning}"
            );
            assert!(
                warning.contains(&format!("tests/mcp.rs:{line}:")),
                "{warning}"
            );
            let b = &o["workers"][1]["blocked"];
            assert!(b["for_ms"].as_u64().unwrap() >= 200, "{b}");
            assert_eq!(o["workers"][0]["blocked"], Value::Null);

            // Worker 1 cannot describe itself, but the rest still answers.
            let w = tool(addr, "workers", json!({}));
            assert!(
                w["workers"][1]["snapshot_error"]
                    .as_str()
                    .unwrap()
                    .contains("did not answer"),
                "{w}"
            );
            assert!(w["workers"][1]["blocked"]["in_task"]
                .as_str()
                .unwrap()
                .contains("tests/mcp.rs"));
            assert!(w["workers"][0]["snapshot_error"].is_null(), "{w}");

            giong::block_on(async { blocker.await.unwrap() });
            thread::sleep(Duration::from_millis(300));
            let o = tool(addr, "overview", json!({}));
            assert!(not_about_the_driver(&o).is_empty(), "{o}");
        },
    );
}

/// The overview's warnings, less the one that says why giong runs on epoll
/// (CI runs these tests in a container that blocks io_uring, too).
fn not_about_the_driver(overview: &Value) -> Vec<&str> {
    overview["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .filter(|w| !w.contains("because io_uring cannot be used here"))
        .collect()
}

#[test]
fn profile_finds_the_busy_task() {
    with_service(open(), |addr, handle| {
        let line = line!() + 1;
        let busy = handle.spawn_on(0, || async {
            loop {
                let start = std::time::Instant::now();
                while start.elapsed() < Duration::from_micros(300) {}
                giong::yield_now().await;
            }
        });
        let p = tool(addr, "profile", json!({ "duration_ms": 300 }));
        let top = &p["total"][0];
        assert!(
            top["location"]
                .as_str()
                .unwrap()
                .contains(&format!("tests/mcp.rs:{line}:")),
            "{p}"
        );
        assert!(top["share_of_all_workers"].as_f64().unwrap() > 0.2, "{p}");
        assert!(p["workers"][0]["busy"].as_f64().unwrap() > 0.5, "{p}");
        assert!(p["duration_ms"].as_f64().unwrap() >= 300.0);
        assert!(!handle.is_profiling(), "profiling is turned off again");
        busy.abort();

        // Bad arguments are the tool's error, for the agent to fix.
        let reply = rpc(
            addr,
            "tools/call",
            json!({ "name": "profile", "arguments": { "duration_ms": 5 } }),
        );
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("duration_ms"));
        let reply = rpc(
            addr,
            "tools/call",
            json!({ "name": "tasks", "arguments": { "wrker": 1 } }),
        );
        assert_eq!(reply["result"]["isError"], true, "{reply}");
    });
}

#[test]
fn protocol_errors() {
    with_service(open(), |addr, _| {
        let reply = rpc(addr, "resources/list", json!({}));
        assert_eq!(reply["error"]["code"], -32601);
        let reply = rpc(addr, "tools/call", json!({ "name": "no_such_tool" }));
        assert_eq!(reply["error"]["code"], -32602);

        let (status, body) = http(addr, "POST", &[JSON], "{not json");
        assert_eq!(status, 400);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            -32700
        );

        let (status, body) = http(
            addr,
            "POST",
            &[JSON],
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
        );
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["code"],
            -32600
        );

        // A batch gets a batch back.
        let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":"b","method":"ping"}]"#;
        let (status, body) = http(addr, "POST", &[JSON], batch);
        assert_eq!(status, 200);
        let replies: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(replies.as_array().unwrap().len(), 2);
        assert_eq!(replies[1]["id"], "b");
    });
}

#[test]
fn only_local_callers_and_proper_requests_are_answered() {
    let token = "s3cret-0123456789abcdef";
    with_service(Config::tcp("127.0.0.1:0").token(token), move |addr, _| {
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let bearer = format!("Bearer {token}");
        let auth = ("Authorization", bearer.as_str());

        assert_eq!(http(addr, "POST", &[JSON], ping).0, 401);
        for wrong in [
            "Bearer nope",
            "Bearer ",
            "Bearer",
            "Basic czNjcmV0LTAxMjM0NTY3ODlhYmNkZWY=",
        ] {
            let (status, _) = http(addr, "POST", &[JSON, ("Authorization", wrong)], ping);
            assert_eq!(status, 401, "{wrong}");
        }
        assert_eq!(http(addr, "POST", &[JSON, auth], ping).0, 200);
        // The scheme's name is not case-sensitive (RFC 9110).
        let lower = format!("bearer {token}");
        assert_eq!(
            http(
                addr,
                "POST",
                &[JSON, ("Authorization", lower.as_str())],
                ping
            )
            .0,
            200
        );
        // DNS rebinding: a page on evil.example resolving its name to 127.0.0.1.
        assert_eq!(
            http(
                addr,
                "POST",
                &[JSON, auth, ("Host", "evil.example:80")],
                ping
            )
            .0,
            403
        );
        assert_eq!(
            http(
                addr,
                "POST",
                &[JSON, auth, ("Origin", "https://evil.example")],
                ping
            )
            .0,
            403
        );
        assert_eq!(
            http(
                addr,
                "POST",
                &[JSON, auth, ("Origin", "http://localhost:6274")],
                ping
            )
            .0,
            200
        );
        assert_eq!(http(addr, "GET", &[auth], "").0, 405);
        assert_eq!(
            http(addr, "POST", &[auth, ("Content-Type", "text/plain")], ping).0,
            415
        );
        assert_eq!(
            http(
                addr,
                "POST",
                &[JSON, auth, ("MCP-Protocol-Version", "1999-01-01")],
                ping
            )
            .0,
            400
        );
    });
}

#[test]
fn it_listens_on_loopback_or_a_unix_socket_only() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let err = Config::tcp("0.0.0.0:0").start(&handle).unwrap_err();
    assert_eq!(giong::error::code(&err), Some(Code::McpNotLocal));
    let err = Config::tcp("example.com:7777").start(&handle).unwrap_err();
    assert_eq!(giong::error::code(&err), Some(Code::McpBadAddress));

    let dir = std::env::temp_dir().join(format!("giong-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mcp.sock");
    // A stale socket file, as a killed process leaves behind.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let server = Config::unix(&path).start(&handle).unwrap();
    assert_eq!(server.local_addr(), None);
    assert_eq!(server.unix_path(), Some(path.as_path()));
    assert!(server.url().starts_with("unix:"));
    // Only the service's user can connect, whatever the umask.
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");
    // No temporary name is left behind.
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let body = r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#;
    write!(
        s,
        "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains(r#""id":7"#), "{out}");
    server.stop();
    assert!(!path.exists(), "the socket file is removed on stop");
    let _ = std::fs::remove_dir(&dir);
}

/// A batch of `n` `workers` calls.
fn workers_batch(n: usize) -> String {
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"workers"}}"#;
    format!("[{}]", vec![call; n].join(","))
}

#[test]
fn what_one_request_can_cost_is_bounded() {
    with_service(open(), |addr, _| {
        // The review's attack: a 254 KiB body of 3,275 calls whose answers
        // are big. It was answered with 14 MB, built in memory.
        assert_eq!(http(addr, "POST", &[JSON], &workers_batch(3275)).0, 413);
        // What still fits in a body is refused as a whole.
        let (status, body) = http(addr, "POST", &[JSON], &workers_batch(800));
        assert_eq!(status, 200);
        assert!(body.len() < 512, "{} bytes", body.len());
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["error"]["code"], -32600, "{reply}");
        assert!(reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("at most 16"));

        // Up to 16 messages are answered, for clients of MCP 2025-03-26.
        let (status, body) = http(addr, "POST", &[JSON], &workers_batch(16));
        assert_eq!(status, 200);
        let replies: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(replies.as_array().unwrap().len(), 16);
        assert_eq!(replies[15]["result"]["isError"], false);
        let (_, body) = http(
            addr,
            "POST",
            &[JSON, ("MCP-Protocol-Version", "2025-03-26")],
            &workers_batch(2),
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Later versions have no batches.
        for version in ["2025-06-18", "2025-11-25"] {
            let (status, body) = http(
                addr,
                "POST",
                &[JSON, ("MCP-Protocol-Version", version)],
                &workers_batch(1),
            );
            assert_eq!(status, 200);
            let reply: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(reply["error"]["code"], -32600, "{version}: {reply}");
        }

        // Bodies are small; a larger one is refused before it is read.
        let big = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
            "x".repeat(70 * 1024)
        );
        assert_eq!(http(addr, "POST", &[JSON], &big).0, 413);
    });
}

#[test]
fn many_clients_at_once_do_not_grow_the_process() {
    with_service(open(), |addr, _| {
        let rss = || {
            std::fs::read_to_string("/proc/self/status")
                .unwrap()
                .lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                .unwrap()
                * 1024
        };
        let before = rss();
        let clients: Vec<_> = (0..24)
            .map(|_| thread::spawn(move || http(addr, "POST", &[JSON], &workers_batch(800))))
            .collect();
        for c in clients {
            assert_eq!(c.join().unwrap().0, 200);
        }
        // 24 such requests took a 4-worker service from 4 MB to 2 GB.
        // ThreadSanitizer's shadow memory grows with every allocation, so
        // its builds only check that every client is answered.
        let grown = rss().saturating_sub(before);
        if !cfg!(giong_tsan) {
            assert!(grown < 64 << 20, "the process grew by {} MB", grown >> 20);
        }
    });
}

#[test]
fn malformed_messages_are_invalid_requests() {
    with_service(open(), |addr, _| {
        for message in [
            r#"{"jsonrpc":"2.0","id":1,"method":5}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"params":{}}"#,
        ] {
            let (status, body) = http(addr, "POST", &[JSON], message);
            assert_eq!(status, 200, "{message}");
            let reply: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(reply["error"]["code"], -32600, "{message}: {reply}");
        }
        // A client's answer to a request of the server's gets no answer.
        for message in [
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":1,"message":"no"}}"#,
        ] {
            assert_eq!(http(addr, "POST", &[JSON], message), (202, String::new()));
        }
    });
}

#[test]
fn tcp_needs_a_proper_token() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let err = Config::tcp("127.0.0.1:0").start(&handle).unwrap_err();
    assert_eq!(giong::error::code(&err), Some(Code::McpNoToken), "{err}");
    assert!(err.to_string().contains("GIONG_MCP_TOKEN"), "{err}");
    for token in [
        "",
        "   ",
        "s3cret",
        "has a space in it 0123",
        "line\nbreak-0123456789",
    ] {
        let err = Config::tcp("127.0.0.1:0")
            .token(token)
            .start(&handle)
            .unwrap_err();
        assert_eq!(
            giong::error::code(&err),
            Some(Code::McpBadToken),
            "{token:?}"
        );
    }
    // Explicitly open, or a proper token: both start.
    let open = open().start(&handle).unwrap();
    let closed = Config::tcp("127.0.0.1:0")
        .token("0123456789abcdef")
        .start(&handle)
        .unwrap();
    // A Unix socket needs none: its file mode keeps other users out.
    let dir = std::env::temp_dir().join(format!("giong-mcp-token-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let unix = Config::unix(dir.join("s")).start(&handle).unwrap();
    // But an empty one is still a mistake there.
    let err = Config::unix(dir.join("t"))
        .token("")
        .start(&handle)
        .unwrap_err();
    assert_eq!(giong::error::code(&err), Some(Code::McpBadToken));
    drop((open, closed, unix));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_busy_address_is_a_coded_error() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = holder.local_addr().unwrap();
    let err = Config::tcp(addr.to_string())
        .allow_unauthenticated()
        .start(&handle)
        .unwrap_err();
    assert_eq!(giong::error::code(&err), Some(Code::McpBind), "{err}");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    let text = err.to_string();
    assert!(
        text.contains("giong-mcp") && text.contains(&addr.to_string()),
        "{text}"
    );
    assert!(text.contains("GIONG_MCP"), "{text}");

    // A live Unix socket is not replaced.
    let dir = std::env::temp_dir().join(format!("giong-mcp-busy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mcp.sock");
    let first = Config::unix(&path).start(&handle).unwrap();
    let err = Config::unix(&path).start(&handle).unwrap_err();
    assert_eq!(giong::error::code(&err), Some(Code::McpBind), "{err}");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    // Nor is a file that is not a socket.
    let file = dir.join("file");
    std::fs::write(&file, b"keep").unwrap();
    let err = Config::unix(&file).start(&handle).unwrap_err();
    assert_eq!(giong::error::code(&err), Some(Code::McpBind), "{err}");
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");

    // A process that replaced the socket after this one started keeps it
    // when this one stops.
    std::fs::remove_file(&path).unwrap();
    let successor = std::os::unix::net::UnixListener::bind(&path).unwrap();
    first.stop();
    assert!(path.exists(), "the successor's socket was removed");
    drop(successor);
    let _ = std::fs::remove_dir_all(&dir);
}

struct PanickingPool;

impl Source for PanickingPool {
    fn kind(&self) -> &'static str {
        "broken pool"
    }
    fn name(&self) -> String {
        "db:5432/broken".into()
    }
    fn stats(&self) -> Vec<(&'static str, u64)> {
        panic!("stats() went wrong");
    }
}

#[test]
fn a_panicking_source_is_not_a_stopped_worker() {
    // The panic hook prints a backtrace first when RUST_BACKTRACE is set,
    // which takes seconds under ThreadSanitizer or in an emulated kernel.
    let cfg = open().snapshot_timeout(Duration::from_secs(10));
    with_service(cfg, |addr, handle| {
        let (registered, done) = mpsc::channel();
        let task = handle.spawn_on(1, move || async move {
            let pool = Rc::new(PanickingPool);
            observe::register_rc(&pool);
            let _ = registered.send(());
            giong::time::sleep(Duration::from_secs(60)).await;
            drop(pool);
        });
        done.recv().unwrap();
        let w = tool(addr, "workers", json!({}));
        let error = w["workers"][1]["snapshot_error"].as_str().unwrap();
        assert!(error.contains("panicked"), "{error}");
        assert!(error.contains("stats() went wrong"), "{error}");
        assert!(w["workers"][0]["snapshot_error"].is_null(), "{w}");
        task.abort();
    });
}
