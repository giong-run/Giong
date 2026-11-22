//! `giong dashboard` end to end: a real service (giong-web with `Logger`
//! and `Record`, giong-mcp with the `web` feature on a Unix socket), a
//! project directory with the `.giong/dev.json` and `.giong/dev.log` that
//! `giong dev` writes, and the `giong dashboard` binary serving the page.
//! The page's API is called the way the page calls it.
#![cfg(unix)]

use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong_web::middleware::{Logger, Record, RequestId};
use giong_web::{Error, Req, Response, Router, Server, Sink};
use serde_json::{json, Value};

async fn hello(_req: Req<Rc<Cell<u64>>>) -> Result<Response, Error> {
    Ok(Response::json(r#"{"hello":"world"}"#))
}

async fn create(req: Req<Rc<Cell<u64>>>) -> Result<Response, Error> {
    let n = req.state().get() + 1;
    req.state().set(n);
    Ok(Response::new(201)
        .header("Content-Type", "application/json")
        .body(format!("{{\"id\":{n}}}")))
}

async fn fail(_req: Req<Rc<Cell<u64>>>) -> Result<Response, Error> {
    Err(Error::new(503, "DB_DOWN", "the database is down"))
}

fn file_sink(path: PathBuf) -> Sink {
    Sink::Custom(Rc::new(move |line: &str| {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let _ = f.write_all(line.as_bytes());
        if !line.ends_with('\n') {
            let _ = f.write_all(b"\n");
        }
    }))
}

/// A project whose service runs: writes `.giong/dev.json` and the log the
/// way `giong dev` does. Gives the project directory and the service's
/// HTTP address. The service lives until the process ends.
fn project(tag: &str) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("giong-cli-dash-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".giong")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo-api\"\nversion = \"0.4.2\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let log = dir.join(".giong").join("dev.log");
    std::fs::write(&log, "demo-api starting\n").unwrap();
    let sock = dir.join(".giong").join("mcp.sock");
    let (tx, rx) = mpsc::channel();
    let (log2, sock2) = (log.clone(), sock.clone());
    thread::spawn(move || {
        let rt = giong::Builder::new().workers(1).build().unwrap();
        let handle = rt.handle();
        let server = giong_mcp::Config::unix(&sock2).start(&handle).unwrap();
        rt.run(move |_| {
            let (log, tx) = (log2.clone(), tx.clone());
            async move {
                let app = Router::new()
                    .get("/hello", hello)
                    .post("/users", create)
                    .get("/fail", fail)
                    .layer(RequestId::new())
                    .layer(Logger::new(file_sink(log.clone())))
                    .layer(Record::new(file_sink(log)))
                    .build(Rc::new(Cell::new(0)));
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                tx.send(listener.local_addr().unwrap().to_string()).unwrap();
                let _ = Server::new().serve(listener, app).await;
            }
        })
        .unwrap();
        server.stop();
    });
    let addr = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let dev = json!({
        "pid": std::process::id(),
        "service_pid": std::process::id(),
        "started_at": 0,
        "mcp_socket": sock.display().to_string(),
        "mcp_token": null,
        "listen": [addr],
        "log": log.display().to_string(),
        "restarts": 2,
        "build": {"state": "ok", "started_at": 0, "finished_at": 1, "took_ms": 812, "errors": 0, "warnings": 1, "error": null},
    });
    std::fs::write(dir.join(".giong").join("dev.json"), dev.to_string()).unwrap();
    (dir, addr)
}

struct Dash(Child, String);

impl Drop for Dash {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `giong dashboard --port 0 --json` in `dir`; gives its address.
fn dashboard(dir: &Path) -> Dash {
    let mut child = Command::new(env!("CARGO_BIN_EXE_giong"))
        .args(["dashboard", "--port", "0", "--json"])
        .current_dir(dir)
        .env_remove("GIONG_MCP")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let v: Value = serde_json::from_str(&first).unwrap_or_else(|e| panic!("{e}: {first}"));
    // The report every command prints with --json, before the server blocks.
    assert_eq!(v["command"], "dashboard", "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["findings"], json!([]), "{v}");
    assert!(v["version"].is_string(), "{v}");
    let url = v["data"]["url"].as_str().unwrap();
    let addr = url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    Dash(child, addr)
}

/// One raw request to the dashboard; gives the status and the body.
fn raw(addr: &str, req: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = out
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn get(addr: &str, path: &str) -> Value {
    let (status, body) = raw(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nX-Giong-Dashboard: 1\r\n\r\n"),
    );
    let v: Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{path}: {status} {e}: {body}"));
    assert!(
        status == 200 || v.get("error").is_some(),
        "{path}: {status} {body}"
    );
    v
}

fn post(addr: &str, path: &str, body: &Value) -> (u16, Value) {
    let b = body.to_string();
    let (status, text) = raw(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nX-Giong-Dashboard: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
            b.len()
        ),
    );
    (
        status,
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}: {text}")),
    )
}

fn http(addr: &str, method: &str, path: &str) -> u16 {
    raw(
        addr,
        &format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
    )
    .0
}

/// Waits until `f` holds, for up to 10 s.
fn eventually(mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_dashboard_shows_the_running_service() {
    let (dir, svc) = project("main");
    let d = dashboard(&dir);
    let a = d.1.as_str();

    // The page, with its policy.
    let (status, page) = raw(a, &format!("GET / HTTP/1.1\r\nHost: {a}\r\n\r\n"));
    assert_eq!(status, 200);
    assert!(page.contains("<title>Gióng dev</title>"));

    // State: dev.json, the service through giong-mcp.
    let s = get(a, "/api/state");
    assert_eq!(s["dev"]["build"]["state"], "ok", "{s}");
    assert_eq!(s["dev"]["restarts"], 2);
    assert_eq!(s["base_url"], format!("http://{svc}"));
    assert_eq!(s["overview"]["runtime"]["workers"], 1, "{s}");
    assert!(s["service_error"].is_null(), "{s}");

    // Routes with curl lines; OpenAPI named after the package.
    let r = get(a, "/api/routes");
    let routes = r["apps"][0]["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 3, "{r}");
    assert_eq!(routes[1]["method"], "POST");
    assert!(routes[1]["curl"]
        .as_str()
        .unwrap()
        .contains(&format!("http://{svc}/users")));
    let o = get(a, "/api/openapi");
    assert_eq!(o["info"]["title"], "demo-api");
    assert_eq!(o["info"]["version"], "0.4.2");
    assert!(o["paths"]["/users"]["post"].is_object(), "{o}");

    // Workers, tasks, pools.
    assert!(get(a, "/api/workers")["workers"].is_array());
    assert!(get(a, "/api/tasks")["by_location"].is_array());
    assert!(get(a, "/api/sources")["sources"].is_array());

    // Traffic shows up in the log, request and exchange lines.
    assert_eq!(http(&svc, "GET", "/hello"), 200);
    assert_eq!(http(&svc, "POST", "/users"), 201);
    assert_eq!(http(&svc, "GET", "/fail"), 503);
    let mut entries = Vec::new();
    eventually(|| {
        let l = get(a, "/api/log?after=0");
        entries = l["entries"].as_array().unwrap().clone();
        entries.iter().filter(|e| e["kind"] == "exchange").count() >= 3
    });
    assert_eq!(entries[0]["kind"], "text");
    assert!(entries
        .iter()
        .any(|e| e["kind"] == "request" && e["data"]["status"] == 503));
    let fail = entries
        .iter()
        .find(|e| e["kind"] == "request" && e["data"]["status"] == 503)
        .unwrap();
    assert_eq!(fail["data"]["error"]["code"], "DB_DOWN", "{fail}");

    // Send a request through the box.
    let (status, sent) = post(
        a,
        "/api/send",
        &json!({"method": "POST", "path": "/users", "headers": [["x-a", "1"]], "body": "{}"}),
    );
    assert_eq!(status, 200, "{sent}");
    assert_eq!(sent["status"], 201);
    assert_eq!(sent["body"], "{\"id\":2}");

    // Save a recorded exchange as a case, then replay it.
    let ex = entries
        .iter()
        .find(|e| e["kind"] == "exchange" && e["data"]["path"] == "/hello")
        .unwrap();
    let (status, saved) = post(
        a,
        "/api/record/save",
        &json!({"at": ex["at"], "name": "from-dashboard"}),
    );
    assert_eq!(status, 200, "{saved}");
    let file = dir.join("tests/recorded/from-dashboard.json");
    assert!(file.is_file(), "{saved}");
    let (status, again) = post(
        a,
        "/api/record/save",
        &json!({"at": ex["at"], "name": "from-dashboard"}),
    );
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["data"]["cases"], 2, "{again}");
    let out = Command::new(env!("CARGO_BIN_EXE_giong"))
        .args(["record", "replay", "--json"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["passed"], 2, "{v}");

    // Offline error pages.
    assert!(get(a, "/api/explain?code=E5066")["page"]
        .as_str()
        .unwrap()
        .contains("GIONG-E5066"));
    assert!(get(a, "/api/explain?code=nope")["error"].is_object());
}

#[test]
fn other_web_pages_cannot_use_the_dashboard() {
    let (dir, svc) = project("guard");
    let d = dashboard(&dir);
    let a = d.1.as_str();
    let port = a.rsplit(':').next().unwrap();
    // DNS rebinding: a page on evil.example resolved to 127.0.0.1.
    let (status, _) = raw(
        a,
        &format!("GET / HTTP/1.1\r\nHost: evil.example:{port}\r\n\r\n"),
    );
    assert_eq!(status, 403);
    // A form or image from another site: no custom header.
    let (status, _) = raw(a, &format!("GET /api/state HTTP/1.1\r\nHost: {a}\r\n\r\n"));
    assert_eq!(status, 403);
    // A fetch from another origin with the header (after a preflight the
    // dashboard refuses anyway).
    let (status, _) = raw(
        a,
        &format!("GET /api/state HTTP/1.1\r\nHost: {a}\r\nOrigin: http://evil.example\r\nX-Giong-Dashboard: 1\r\n\r\n"),
    );
    assert_eq!(status, 403);
    // A preflight is refused, without CORS headers.
    let (status, _) = raw(
        a,
        &format!("OPTIONS /api/send HTTP/1.1\r\nHost: {a}\r\nOrigin: http://evil.example\r\n\r\n"),
    );
    assert_eq!(status, 403);
    let (status, _) = raw(
        a,
        &format!("OPTIONS /api/send HTTP/1.1\r\nHost: {a}\r\n\r\n"),
    );
    assert_eq!(status, 405);
    // The send box is not a proxy to other machines.
    let (status, v) = post(
        a,
        "/api/send",
        &json!({"base_url": "http://192.0.2.1:80", "method": "GET", "path": "/"}),
    );
    assert_eq!(status, 400, "{v}");
    // Garbage does not kill it.
    let mut s = TcpStream::connect(a).unwrap();
    s.write_all(b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03\r\n\r\n")
        .unwrap();
    drop(s);
    let (status, _) = raw(a, &format!("GET / HTTP/1.1\r\nHost: {a}\r\n\r\n"));
    assert_eq!(status, 200);
    let _ = svc;
}

#[test]
fn a_stopped_service_is_reported_not_fatal() {
    let dir = std::env::temp_dir().join(format!("giong-cli-dash-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let d = dashboard(&dir);
    let s = get(&d.1, "/api/state");
    assert_eq!(s["service_error"]["code"], "GIONG-E5057", "{s}");
    assert!(get(&d.1, "/api/routes")["error"]["code"].is_string());
    let l = get(&d.1, "/api/log");
    assert!(l["log"].is_null(), "{l}");
    // Port in use: a coded failure.
    let port = d.1.rsplit(':').next().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_giong"))
        .args(["dashboard", "--port", port, "--json"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "GIONG-E5053", "{v}");
}

/// A package of no dependencies whose binary prints a line and stays up for
/// `secs` seconds: enough for `giong dev` to build and run.
fn tiny_project(tag: &str, secs: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("giong-cli-dash-dev-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        format!(
            "fn main() {{ println!(\"hello from tiny\"); \
             std::thread::sleep(std::time::Duration::from_secs({secs})); }}\n"
        ),
    )
    .unwrap();
    dir
}

/// Runs `giong dev --once --json` with `flags` in `dir`; gives the child and
/// its stdout lines as they come.
fn dev_once(dir: &Path, flags: &[&str]) -> (Dash, mpsc::Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_giong"))
        .args(["dev", "--once", "--json"])
        .args(flags)
        .current_dir(dir)
        .env_remove("GIONG_MCP")
        .env_remove("LISTEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    (Dash(child, String::new()), rx)
}

/// The next line that `want` accepts, within 120 s (a cold cargo build).
fn next(rx: &mpsc::Receiver<Value>, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let v = rx
            .recv_timeout(left)
            .unwrap_or_else(|e| panic!("no matching line from giong dev: {e}"));
        if want(&v) {
            return v;
        }
    }
}

#[test]
fn giong_dev_ui_serves_the_dashboard() {
    let dir = tiny_project("ui", 10);
    let (_dev, rx) = dev_once(&dir, &["--ui-port", "0"]);
    let event = next(&rx, |v| v["event"] == "dashboard");
    let addr = event["url"]
        .as_str()
        .unwrap()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    next(&rx, |v| v["event"] == "ready");
    eventually(|| get(&addr, "/api/state")["dev"]["build"]["state"] == "ok");
    eventually(|| {
        get(&addr, "/api/log")["entries"]
            .as_array()
            .is_some_and(|l| l.iter().any(|x| x.to_string().contains("hello from tiny")))
    });
    // The guards hold for the dashboard giong dev serves too.
    let (status, _) = raw(
        &addr,
        &format!("GET /api/state HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    );
    assert_eq!(status, 403);
}

#[test]
fn giong_dev_runs_without_the_dashboard_when_its_ports_are_taken() {
    // Ten free ports in a row, held while giong dev starts.
    let held = (0..50)
        .find_map(|_| {
            let first = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            let base = first.local_addr().ok()?.port();
            if base > u16::MAX - 10 {
                return None;
            }
            let mut all = vec![first];
            for p in base + 1..base + 10 {
                all.push(std::net::TcpListener::bind(("127.0.0.1", p)).ok()?);
            }
            Some((base, all))
        })
        .expect("ten free ports in a row");
    let dir = tiny_project("busy", 2);
    let port = held.0.to_string();
    let (mut dev, rx) = dev_once(&dir, &["--ui-port", &port]);
    let warning = next(&rx, |v| v["event"] == "warning");
    assert_eq!(warning["code"], "GIONG-E5053", "{warning}");
    // The service still builds and runs, and the report keeps the warning.
    let report = next(&rx, |v| v["command"] == "dev");
    assert_eq!(report["ok"], true, "{report}");
    let finding = &report["findings"][0];
    assert_eq!(finding["code"], "GIONG-E5053", "{report}");
    assert_eq!(finding["severity"], "warning", "{report}");
    assert!(dev.0.wait().unwrap().success());
    drop(held);
}

/// Keeps a demo running for a person or a browser test to look at:
/// `GIONG_DASH_DEMO_SECS=600 cargo test -p giong-cli --test dashboard demo -- --ignored --nocapture`.
#[test]
#[ignore]
fn demo() {
    let secs: u64 = std::env::var("GIONG_DASH_DEMO_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let (dir, svc) = project("demo");
    let d = dashboard(&dir);
    println!(
        "DASHBOARD http://{}/ SERVICE http://{svc} DIR {}",
        d.1,
        dir.display()
    );
    let start = Instant::now();
    let mut i = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        let _ = http(&svc, "GET", "/hello");
        if i % 3 == 0 {
            let _ = http(&svc, "POST", "/users");
        }
        if i % 7 == 0 {
            let _ = http(&svc, "GET", "/fail");
        }
        if i % 11 == 0 {
            let _ = http(&svc, "GET", "/missing");
        }
        i += 1;
        thread::sleep(Duration::from_millis(400));
    }
}
