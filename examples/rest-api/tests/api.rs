//! The API end to end: a throwaway PostgreSQL cluster and Redis server, the
//! app served over HTTPS on a free port, and giong-http's client.
//!
//! PostgreSQL is found through `GIONG_PG_BIN`, then in
//! `/usr/lib/postgresql/<version>/bin`, then on `PATH`; Redis through
//! `GIONG_REDIS_BIN` (the directory that holds `redis-server`), then on
//! `PATH`. Without them the test is skipped,
//! unless `GIONG_PG_REQUIRED=1` or `GIONG_REDIS_REQUIRED=1` (CI sets both).

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong_http::client::Client;
use giong_tls::{TlsAcceptor, TlsConnector};
use giong_web::Server;
use rest_api::{app, AccessLog, State, SCHEMA};

/// A child process killed with the test, even if the test binary dies.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(cmd: &mut Command, uid: Option<(u32, u32)>) -> Daemon {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some((u, g)) = uid {
        cmd.uid(u).gid(g);
    }
    // SAFETY: prctl is async-signal-safe and touches only the child.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    Daemon(cmd.spawn().expect("start the server"))
}

fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| v == "1")
}

fn pg_bin() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GIONG_PG_BIN") {
        return Some(dir.into());
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir("/usr/lib/postgresql")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|bin| bin.join("initdb").exists())
        .collect();
    versions.sort();
    versions.pop().or_else(|| on_path("initdb"))
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find(|dir| dir.join(name).exists())
}

/// PostgreSQL refuses to run as root: then it runs as `postgres`.
fn pg_user() -> Option<(u32, u32)> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f[0] == "postgres").then(|| (f[2].parse().unwrap(), f[3].parse().unwrap()))
    })
}

fn chown(path: &Path, owner: Option<(u32, u32)>) {
    if let Some((u, g)) = owner {
        std::os::unix::fs::chown(path, Some(u), Some(g)).unwrap();
    }
}

fn wait_for(path: &Path, what: &str) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{what} did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A cluster listening on a Unix socket only; its URL.
fn postgres(dir: &Path, bin: &Path) -> (Daemon, String) {
    let owner = pg_user();
    let (data, socket) = (dir.join("pg"), dir.join("pg-socket"));
    std::fs::create_dir_all(&socket).unwrap();
    chown(dir, owner);
    chown(&socket, owner);
    let mut initdb = Command::new(bin.join("initdb"));
    initdb
        .arg("-D")
        .arg(&data)
        .args(["-U", "postgres", "-A", "trust", "--no-sync"]);
    if let Some((u, g)) = owner {
        initdb.uid(u).gid(g);
    }
    let out = initdb.output().expect("run initdb");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let daemon = spawn(
        Command::new(bin.join("postgres"))
            .arg("-D")
            .arg(&data)
            .args(["-c", "listen_addresses=", "-c", "fsync=off", "-c"])
            .arg(format!("unix_socket_directories={}", socket.display())),
        owner,
    );
    wait_for(&socket.join(".s.PGSQL.5432"), "PostgreSQL");
    // The socket appears a moment before connections are accepted.
    std::thread::sleep(Duration::from_millis(300));
    let url = format!(
        "postgres://postgres@/postgres?host={}&connect_timeout=10",
        socket.display()
    );
    (daemon, url)
}

/// A Redis server on a Unix socket; its URL.
fn redis(dir: &Path, bin: &Path) -> (Daemon, String) {
    let socket = dir.join("redis.sock");
    let daemon = spawn(
        Command::new(bin)
            .args(["--port", "0", "--save", "", "--appendonly", "no"])
            .arg("--unixsocket")
            .arg(&socket),
        None,
    );
    wait_for(&socket, "Redis");
    (daemon, format!("redis+unix://{}", socket.display()))
}

/// A certificate for `localhost`, and a client that trusts it.
fn tls() -> (TlsAcceptor, Client) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let acceptor =
        TlsAcceptor::from_pem(cert.pem().as_bytes(), key.serialize_pem().as_bytes()).unwrap();
    let connector = TlsConnector::with_root_pem(cert.pem().as_bytes()).unwrap();
    (acceptor, Client::builder().tls(connector).build())
}

fn state(db_url: &str, cache_url: &str) -> State {
    State {
        db: giong_postgres::Pool::new(giong_postgres::Config::from_url(db_url).unwrap(), 2),
        cache: giong_redis::Pool::new(giong_redis::Config::from_url(cache_url).unwrap(), 1),
        cache_ttl: Duration::from_secs(60),
    }
}

/// Serves an app over HTTPS on a free port of this worker; its base URL.
fn serve(state: State, acceptor: TlsAcceptor) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = app(state, AccessLog::Off);
    giong::spawn(async move { Server::new().serve_tls(listener, acceptor, app).await });
    format!("https://localhost:{port}")
}

#[test]
fn crud_through_the_cache_over_https() {
    // GIONG_REDIS_BIN is the directory that holds redis-server.
    let redis_dir = std::env::var_os("GIONG_REDIS_BIN").map(PathBuf::from);
    let redis_bin = redis_dir
        .or_else(|| on_path("redis-server"))
        .map(|dir| dir.join("redis-server"));
    let pg = pg_bin();
    let (Some(pg), Some(redis_bin)) = (pg, redis_bin) else {
        assert!(
            !required("GIONG_PG_REQUIRED") && !required("GIONG_REDIS_REQUIRED"),
            "PostgreSQL or Redis is required but was not found"
        );
        eprintln!("skipping: PostgreSQL or Redis not found");
        return;
    };
    let dir = std::env::temp_dir().join(format!("rest-api-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (_pg, db_url) = postgres(&dir, &pg);
    let (_redis, cache_url) = redis(&dir, &redis_bin);

    giong::block_on(async {
        let (acceptor, client) = tls();
        let setup = state(&db_url, &cache_url);
        setup.db.batch_execute(SCHEMA).await.unwrap();
        let base = serve(setup, acceptor.clone());
        let url = |path: &str| format!("{base}{path}");

        let resp = client.get(&url("/ready")).send().await.unwrap();
        assert_eq!((resp.status(), resp.text().unwrap()), (200, "ready"));

        // Create.
        let resp = client
            .post(&url("/users"))
            .header("Content-Type", "application/json")
            .body(r#"{"name":"  Ada Lovelace ","email":"ada@example.com"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201, "{}", resp.text().unwrap());
        let created: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
        let id = created["id"].as_i64().unwrap();
        assert_eq!(
            resp.header("location"),
            Some(format!("/users/{id}").as_str())
        );
        assert_eq!(created["name"], "Ada Lovelace");
        assert!(created["created_at"].as_str().unwrap().ends_with('Z'));

        let resp = client
            .post(&url("/users"))
            .header("Content-Type", "application/json")
            .body(r#"{"name":"Ada again","email":"ada@example.com"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        assert!(resp.text().unwrap().contains("\"code\":\"email_taken\""));

        let resp = client
            .post(&url("/users"))
            .header("Content-Type", "application/json")
            .body(r#"{"name":"","email":"nobody"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 422);
        assert!(resp.text().unwrap().contains("\"code\":\"invalid_name\""));

        // Read: from PostgreSQL first, from Redis next.
        let first = client
            .get(&url(&format!("/users/{id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 200);
        assert_eq!(first.header("x-cache"), Some("miss"));
        let second = client
            .get(&url(&format!("/users/{id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(second.header("x-cache"), Some("hit"));
        assert_eq!(first.body(), second.body());
        let resp = client.get(&url("/users/999999")).send().await.unwrap();
        assert_eq!(resp.status(), 404);
        let resp = client.get(&url("/users/abc")).send().await.unwrap();
        assert_eq!(resp.status(), 400);

        // Update: the cached copy goes.
        let resp = client
            .put(&url(&format!("/users/{id}")))
            .header("Content-Type", "application/json")
            .body(r#"{"name":"Ada King","email":"ada@example.com"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{}", resp.text().unwrap());
        let resp = client
            .get(&url(&format!("/users/{id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.header("x-cache"), Some("miss"));
        assert!(resp.text().unwrap().contains("\"name\":\"Ada King\""));

        // Pages, and the export.
        for n in 0..4 {
            let body = format!(r#"{{"name":"User {n}","email":"user{n}@example.com"}}"#);
            let resp = client
                .post(&url("/users"))
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 201);
        }
        let resp = client.get(&url("/users?limit=2")).send().await.unwrap();
        let page: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
        assert_eq!(page["users"].as_array().unwrap().len(), 2);
        let after = page["next_after"].as_i64().unwrap();
        let resp = client
            .get(&url(&format!("/users?limit=10&after={after}")))
            .send()
            .await
            .unwrap();
        let page: serde_json::Value = serde_json::from_slice(resp.body()).unwrap();
        assert_eq!(page["users"].as_array().unwrap().len(), 3);
        assert!(page["next_after"].is_null());
        let resp = client.get(&url("/users?limit=0")).send().await.unwrap();
        assert_eq!(resp.status(), 400);

        let resp = client.get(&url("/users/export")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.header("transfer-encoding"), Some("chunked"));
        let text = resp.text().unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["name"], "Ada King");

        // Delete.
        let resp = client
            .delete(&url(&format!("/users/{id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        let resp = client
            .get(&url(&format!("/users/{id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = client
            .delete(&url(&format!("/users/{id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        // With the cache unreachable: reads still work, /ready says no.
        let (acceptor, client) = tls();
        let missing = format!("redis+unix://{}", dir.join("nothing.sock").display());
        let base = serve(state(&db_url, &missing), acceptor);
        let resp = client
            .get(&format!("{base}/users/{after}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.header("x-cache"), Some("miss"));
        let resp = client.get(&format!("{base}/ready")).send().await.unwrap();
        assert_eq!(resp.status(), 503);
    });
    let _ = std::fs::remove_dir_all(&dir);
}
