//! `giong routes`, `giong openapi` and `giong top` against a real service: a
//! two-worker giong runtime serving giong-web routes, with giong-mcp on a
//! Unix socket or a loopback port. The `giong` binary is run as a user runs
//! it, and its JSON report is checked.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use giong_web::{Error, Req, Response, Router};
use serde_json::Value;

async fn ok(_req: Req) -> Result<Response, Error> {
    Ok(Response::text("ok"))
}

/// A running service until the guard drops.
struct Service {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    pub mcp: String,
    _dir: PathBuf,
}

impl Drop for Service {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "giong-cli-e2e-{tag}-{}-{:?}",
        std::process::id(),
        thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A directory for a Unix socket, under `/tmp` with a short name: a socket
/// path holds at most 104 bytes on macOS, where `$TMPDIR` alone takes ~50.
fn socket_dir() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/gc-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts the service; with `block`, worker 1 blocks its thread in one task
/// for 3 s, a moment after starting.
fn service(tcp_token: Option<&str>) -> Service {
    service_with(tcp_token, false)
}

fn service_with(tcp_token: Option<&str>, block: bool) -> Service {
    let dir = socket_dir();
    let sock = dir.join("mcp.sock");
    let (ready_tx, ready_rx) = mpsc::channel::<String>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let token = tcp_token.map(str::to_string);
    let thread = thread::spawn(move || {
        let rt = giong::Builder::new().workers(2).build().unwrap();
        let handle = rt.handle();
        let cfg = match &token {
            Some(t) => giong_mcp::Config::tcp("127.0.0.1:0").token(t.clone()),
            None => giong_mcp::Config::unix(&sock),
        };
        let server = cfg.start(&handle).unwrap();
        let mcp = match server.local_addr() {
            Some(a) => a.to_string(),
            None => format!("unix:{}", sock.display()),
        };
        let h = handle.clone();
        let stopper = thread::spawn(move || {
            let _ = stop_rx.recv();
            h.shutdown();
        });
        let ready = ready_tx.clone();
        let (count_tx, count_rx) = mpsc::channel::<()>();
        let announce = thread::spawn(move || {
            count_rx.recv().unwrap();
            count_rx.recv().unwrap();
            ready.send(mcp).unwrap();
        });
        rt.run(move |w| {
            let count = count_tx.clone();
            async move {
                if block && w == 1 {
                    drop(giong::spawn(async {
                        giong::time::sleep(Duration::from_millis(200)).await;
                        // Deliberately blocking: what giong top must catch.
                        std::thread::sleep(Duration::from_secs(3));
                    }));
                }
                let users = Router::new().get("/{id}", ok).delete("/{id}", ok);
                let app = Router::new()
                    .get("/health", ok)
                    .post("/users", ok)
                    .nest("/users", users)
                    .put("/files/{*path}", ok)
                    .stream_bodies(1 << 20)
                    .build(());
                count.send(()).unwrap();
                giong::shutdown_signal().await;
                drop(app);
            }
        })
        .unwrap();
        announce.join().unwrap();
        stopper.join().unwrap();
        server.stop();
    });
    let mcp = ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    Service {
        stop: Some(stop_tx),
        thread: Some(thread),
        mcp,
        _dir: dir,
    }
}

/// Runs `giong <args> --json` in `cwd`; returns the exit status and report.
fn giong(cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_giong"));
    cmd.args(args).arg("--json").current_dir(cwd);
    cmd.env_remove("GIONG_MCP").env_remove("GIONG_MCP_TOKEN");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let report: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): {text}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap_or(-1), report)
}

#[test]
fn routes_lists_every_route_with_curl_lines() {
    let svc = service(None);
    let dir = temp_dir("routes-cwd");
    let (status, r) = giong(&dir, &["routes", "--mcp", &svc.mcp], &[]);
    assert_eq!(status, 0, "{r}");
    assert_eq!(r["command"], "routes");
    let apps = r["data"]["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 1, "{r}");
    assert_eq!(apps[0]["workers"], serde_json::json!([0, 1]));
    let rows: Vec<(String, String)> = apps[0]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["method"].as_str().unwrap().to_string(),
                x["pattern"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let want = [
        ("GET", "/health"),
        ("POST", "/users"),
        ("GET", "/users/{id}"),
        ("DELETE", "/users/{id}"),
        ("PUT", "/files/{*path}"),
    ];
    assert_eq!(
        rows,
        want.iter()
            .map(|(m, p)| (m.to_string(), p.to_string()))
            .collect::<Vec<_>>()
    );
    let put = &apps[0]["routes"][4];
    assert_eq!(put["stream_limit"], 1 << 20);
    assert_eq!(
        put["curl"],
        "curl -i -X PUT --data-binary @file 'http://127.0.0.1:8080/files/path/example'"
    );
    assert!(put["added_at"]
        .as_str()
        .unwrap()
        .contains("tests/service.rs:"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn openapi_writes_a_document_for_the_routes() {
    let svc = service(None);
    let dir = temp_dir("openapi-cwd");
    let out = dir.join("openapi.json");
    let (status, r) = giong(
        &dir,
        &[
            "openapi",
            "--mcp",
            &svc.mcp,
            "--out",
            out.to_str().unwrap(),
            "--title",
            "users api",
            "--base-url",
            "http://localhost:9000",
        ],
        &[],
    );
    assert_eq!(status, 0, "{r}");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(doc, r["data"]["document"]);
    assert_eq!(doc["info"]["title"], "users api");
    assert_eq!(doc["servers"][0]["url"], "http://localhost:9000");
    let paths = doc["paths"].as_object().unwrap();
    assert_eq!(paths.len(), 4, "{doc}");
    assert!(doc["paths"]["/users/{id}"]["delete"]["operationId"] == "delete_users_by_id");
    assert!(doc["paths"]["/files/{path}"]["put"]["x-giong-stream-limit"] == 1 << 20);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn openapi_out_follows_no_committed_link() {
    let svc = service(None);
    let dir = temp_dir("openapi-links");
    let elsewhere = temp_dir("openapi-elsewhere");
    let secret = elsewhere.join("openapi.json");
    std::fs::write(&secret, "do not touch").unwrap();
    // A committed folder link, and a committed file link.
    std::os::unix::fs::symlink(&elsewhere, dir.join("docs")).unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("openapi.json")).unwrap();
    let absolute = dir.join("docs").join("openapi.json");
    for out in [
        "docs/openapi.json",
        "openapi.json",
        absolute.to_str().unwrap(),
    ] {
        let (status, r) = giong(&dir, &["openapi", "--mcp", &svc.mcp, "--out", out], &[]);
        assert_eq!(status, 2, "{out}: {r}");
        assert_eq!(r["error"]["code"], "GIONG-E5000", "{out}: {r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("symbolic link"),
            "{out}: {r}"
        );
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "do not touch");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 1, "{out}");
    }
    // A folder with no link in it is written as before.
    std::fs::create_dir(dir.join("api")).unwrap();
    let (status, r) = giong(
        &dir,
        &["openapi", "--mcp", &svc.mcp, "--out", "api/openapi.json"],
        &[],
    );
    assert_eq!(status, 0, "{r}");
    assert!(dir.join("api").join("openapi.json").is_file());
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&elsewhere).unwrap();
}

#[test]
fn top_once_takes_a_snapshot_over_tcp_with_a_token() {
    let token = "0123456789abcdef0123";
    let svc = service(Some(token));
    let dir = temp_dir("top-cwd");
    let (status, r) = giong(
        &dir,
        &["top", "--once", "--mcp", &svc.mcp, "--profile", "200"],
        &[("GIONG_MCP_TOKEN", token)],
    );
    assert_eq!(status, 0, "{r}");
    assert_eq!(r["data"]["overview"]["runtime"]["workers"], 2);
    assert_eq!(r["data"]["workers"]["workers"].as_array().unwrap().len(), 2);
    assert!(r["data"]["tasks"]["total_tasks"].as_u64().is_some());
    assert!(r["data"]["profile"]["duration_ms"].as_f64().unwrap() >= 150.0);

    // A wrong token is a coded failure, exit status 2.
    let (status, r) = giong(
        &dir,
        &["top", "--once", "--mcp", &svc.mcp],
        &[("GIONG_MCP_TOKEN", "wrong-token-wrong-token")],
    );
    assert_eq!(status, 2, "{r}");
    assert_eq!(r["error"]["code"], "GIONG-E5019", "{r}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_service_is_found_through_giong_mcp_and_dev_json() {
    let svc = service(None);
    let dir = temp_dir("locate-cwd");
    // GIONG_MCP, as the service itself reads it.
    let (status, r) = giong(&dir, &["routes"], &[("GIONG_MCP", &svc.mcp)]);
    assert_eq!(status, 0, "{r}");
    // .giong/dev.json in a project, as giong dev writes it.
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".giong")).unwrap();
    let sock = svc.mcp.strip_prefix("unix:").unwrap();
    std::fs::write(
        dir.join(".giong/dev.json"),
        serde_json::json!({ "pid": 1, "mcp_socket": sock, "listen": ["0.0.0.0:3000"] }).to_string(),
    )
    .unwrap();
    let (status, r) = giong(&dir, &["routes"], &[]);
    assert_eq!(status, 0, "{r}");
    // The curl lines use what giong dev listens on.
    assert_eq!(r["data"]["base_url"], "http://127.0.0.1:3000");
    let (status, r) = giong(&dir, &["openapi"], &[]);
    assert_eq!(status, 0, "{r}");
    assert_eq!(r["data"]["document"]["info"]["title"], "demo");
    assert_eq!(r["data"]["document"]["info"]["version"], "0.1.0");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn no_service_and_a_dead_socket_are_coded_failures() {
    let dir = temp_dir("none");
    let (status, r) = giong(&dir, &["routes"], &[]);
    assert_eq!(status, 2, "{r}");
    assert_eq!(r["error"]["code"], "GIONG-E5057", "{r}");
    let sockets = socket_dir();
    let gone = sockets.join("gone.sock");
    let (status, r) = giong(
        &dir,
        &[
            "top",
            "--once",
            "--mcp",
            &format!("unix:{}", gone.display()),
        ],
        &[],
    );
    assert_eq!(status, 2, "{r}");
    assert_eq!(r["error"]["code"], "GIONG-E5019", "{r}");
    let (status, r) = giong(&dir, &["top", "--interval", "5"], &[]);
    assert_eq!(status, 2, "{r}");
    assert_eq!(r["error"]["code"], "GIONG-E5000", "{r}");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&sockets).unwrap();
}

#[test]
fn top_names_a_stuck_worker_and_fails() {
    let svc = service_with(None, true);
    let dir = temp_dir("blocked-cwd");
    thread::sleep(Duration::from_millis(700));
    let (status, r) = giong(&dir, &["top", "--once", "--mcp", &svc.mcp], &[]);
    assert_eq!(status, 1, "{r}");
    let f = r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "GIONG-E5055")
        .unwrap_or_else(|| panic!("no stuck-worker finding: {r}"));
    assert_eq!(f["severity"], "error");
    assert!(
        f["detail"].as_str().unwrap().contains("tests/service.rs:"),
        "{f}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
