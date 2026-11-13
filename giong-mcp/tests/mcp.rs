//! giong-mcp end to end: a runtime with two workers, the server on a
//! loopback port, and a plain HTTP client on the test thread.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
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

/// Calls a tool and returns its structured result.
fn tool(addr: SocketAddr, name: &str, args: Value) -> Value {
    let reply = rpc(
        addr,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    );
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{reply}");
    // The same JSON as text, for clients that read only `content`.
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    result["structuredContent"].clone()
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
        f(addr, h.clone());
        h.shutdown();
        let _ = done_tx.send(());
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
    with_service(Config::tcp("127.0.0.1:0"), |addr, _| {
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
            assert_eq!(t["annotations"]["readOnlyHint"], true);
            assert_eq!(t["inputSchema"]["type"], "object");
        }
        assert_eq!(rpc(addr, "ping", json!({}))["result"], json!({}));
    });
}

#[test]
fn overview_workers_tasks_and_sources() {
    with_service(Config::tcp("127.0.0.1:0"), |addr, handle| {
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
        Config::tcp("127.0.0.1:0").snapshot_timeout(Duration::from_millis(200)),
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
    with_service(Config::tcp("127.0.0.1:0"), |addr, handle| {
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
    with_service(Config::tcp("127.0.0.1:0"), |addr, _| {
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
    with_service(Config::tcp("127.0.0.1:0").token("s3cret"), |addr, _| {
        let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let auth = ("Authorization", "Bearer s3cret");

        assert_eq!(http(addr, "POST", &[JSON], ping).0, 401);
        assert_eq!(
            http(
                addr,
                "POST",
                &[JSON, ("Authorization", "Bearer nope")],
                ping
            )
            .0,
            401
        );
        assert_eq!(http(addr, "POST", &[JSON, auth], ping).0, 200);
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
    assert!(server.url().starts_with("unix:"));
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
