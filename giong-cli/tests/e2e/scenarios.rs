//! Whole workflows across commands, as a person or an agent runs them:
//! `giong new` -> `giong doctor` -> `giong dev` -> edit a handler -> the new
//! code answers, with no request dropped while it reloads.
//!
//! These build a generated project (a minute or more), so they run with
//! `GIONG_E2E_SLOW=1`, and only once the commands they use exist. The
//! generated project is pointed at this checkout's crates with a
//! `[patch.crates-io]` section, so the test measures the code under review,
//! not the last release.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::harness::{giong, grep_files, has_command, slow_enabled, TempDir, GIONG};

/// The workspace crates a generated project may depend on.
const CRATES: &[(&str, &str)] = &[
    ("giong", "."),
    ("giong-http", "giong-http"),
    ("giong-tls", "giong-tls"),
    ("giong-web", "giong-web"),
    ("giong-mcp", "giong-mcp"),
    ("giong-postgres", "giong-postgres"),
    ("giong-mysql", "giong-mysql"),
    ("giong-redis", "giong-redis"),
    ("giong-compat", "giong-compat"),
    ("giong-proto", "giong-proto"),
    ("giong-grpc", "giong-grpc"),
    ("giong-grpc-build", "giong-grpc-build"),
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Points the generated project at this checkout.
fn patch_to_checkout(project: &Path) {
    let manifest = project.join("Cargo.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    if text.contains("[patch.crates-io]") {
        return; // the template already did (a --local style flag)
    }
    text.push_str("\n[patch.crates-io]\n");
    for (name, rel) in CRATES {
        let p = workspace().join(rel);
        text.push_str(&format!(
            "{name} = {{ path = {:?} }}\n",
            p.display().to_string()
        ));
    }
    std::fs::write(manifest, text).unwrap();
}

/// Whether `giong <command>` takes `--<flag>`.
fn has_flag(command: &str, flag: &str) -> bool {
    let v = giong([command, "--help", "--json"]).run().report("help");
    v["data"]["flags"]
        .as_array()
        .is_some_and(|f| f.iter().any(|f| f["name"] == flag))
}

/// `giong new <name> <extra>` in `dir`, pointed at this checkout: with
/// `--giong-path` when the command has it, else with `[patch.crates-io]`.
fn new_project(dir: &TempDir, name: &str, extra: &[&str]) -> (crate::harness::Out, PathBuf) {
    let mut run = giong(["new", name, "--json"]);
    for e in extra {
        run = run.arg(e);
    }
    let local = has_flag("new", "giong-path");
    if local {
        run = run.arg("--giong-path").arg(workspace());
    }
    let out = run.dir(dir.path()).timeout(Duration::from_secs(120)).run();
    let project = dir.join(name);
    if !local && project.join("Cargo.toml").exists() {
        patch_to_checkout(&project);
    }
    (out, project)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One `GET path`, with `Connection: close`. Gives (status, body) or the
/// error as text.
fn get(addr: SocketAddr, path: &str) -> Result<(u16, String), String> {
    let mut s =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("no status line in {text:?}"))?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

/// `giong dev`, running, with what it printed collected.
struct Dev {
    child: Child,
    out: Arc<Mutex<String>>,
}

impl Dev {
    fn start(project: &Path, port: u16) -> Dev {
        Dev::start_on(project, &format!("127.0.0.1:{port}"), port)
    }

    fn start_on(project: &Path, listen: &str, port: u16) -> Dev {
        let mut child = Command::new(GIONG)
            .args(["dev", "--json"])
            .current_dir(project)
            // The templates read LISTEN; PORT for any other convention.
            .env("LISTEN", listen)
            .env("PORT", port.to_string())
            .env("WORKERS", "2")
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start giong dev");
        let out = Arc::new(Mutex::new(String::new()));
        for pipe in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let out = out.clone();
            std::thread::spawn(move || {
                let mut r = std::io::BufReader::new(pipe);
                let mut line = String::new();
                while std::io::BufRead::read_line(&mut r, &mut line).unwrap_or(0) > 0 {
                    out.lock().unwrap().push_str(&line);
                    line.clear();
                }
            });
        }
        Dev { child, out }
    }

    fn log(&self) -> String {
        self.out.lock().unwrap().clone()
    }

    fn wait_until(&mut self, what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if ok() {
                return;
            }
            if let Ok(Some(s)) = self.child.try_wait() {
                panic!(
                    "giong dev exited ({s:?}) while waiting for {what}\n{}",
                    self.log()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out after {timeout:?} waiting for {what}\n{}",
            self.log()
        );
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Dev {
    /// Ctrl+C, then a kill if it has not ended within 10 s.
    fn stop(&mut self) {
        // Already ended (std keeps the status), so no signal to a reused pid.
        if let Ok(Some(_)) = self.child.try_wait() {
            return;
        }
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status();
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn new_project_is_complete_and_doctor_is_happy() {
    if !has_command("new") || !slow_enabled("new_project_is_complete_and_doctor_is_happy") {
        return;
    }
    let dir = TempDir::new("scenario-new");
    let (out, app) = new_project(&dir, "app", &[]);
    out.report("new");
    out.expect_code(0);
    for f in [
        "Cargo.toml",
        "src/main.rs",
        ".gitignore",
        ".env.example",
        "AGENTS.md",
    ] {
        assert!(app.join(f).exists(), "giong new wrote no {f}");
    }
    // A second `giong new app` must not overwrite the project.
    let main_before = std::fs::read_to_string(app.join("src/main.rs")).unwrap();
    let again = giong(["new", "app", "--json"]).dir(dir.path()).run();
    let v = again.report("new");
    assert!(
        v.get("error").is_some(),
        "giong new over an existing project succeeded: {v:#}"
    );
    assert_eq!(
        std::fs::read_to_string(app.join("src/main.rs")).unwrap(),
        main_before
    );

    let v = giong(["doctor", "--json"])
        .dir(&app)
        .timeout(Duration::from_secs(120))
        .run()
        .report("doctor");
    let codes: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(
        !codes.contains(&"GIONG-E5012") && !codes.contains(&"GIONG-E5004"),
        "the template does not follow its own doctor: {v:#}"
    );
    let build = Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()))
        .args(["test", "--quiet"])
        .current_dir(&app)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "the generated project's own tests fail:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
}

/// The promise of `giong dev`: edit a handler, the new code answers, and a
/// client hammering the service meanwhile sees no failed request.
#[test]
fn dev_reloads_an_edit_without_dropping_a_request() {
    if !has_command("new")
        || !has_command("dev")
        || !slow_enabled("dev_reloads_an_edit_without_dropping_a_request")
    {
        return;
    }
    let dir = TempDir::new("scenario-dev");
    let (out, app) = new_project(&dir, "app", &[]);
    out.expect_code(0);
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut dev = Dev::start(&app, port);
    dev.wait_until(
        "the first build to answer",
        Duration::from_secs(600),
        || get(addr, "/health").is_ok_and(|(s, _)| s < 500),
    );

    // A route whose answer is a string literal of the source: that literal
    // is what we edit. Templates differ, so try a few.
    let (path, marker, files) = PROBES
        .iter()
        .find_map(|path| {
            let (status, body) = get(addr, path).ok()?;
            if status >= 400 {
                return None;
            }
            let mut words: Vec<&str> = body
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 4)
                .collect();
            words.sort_by_key(|w| std::cmp::Reverse(w.len()));
            words.into_iter().find_map(|w| {
                let marker = format!("\"{w}");
                let files = grep_files(&app.join("src"), "rs", &marker);
                (!files.is_empty()).then_some((*path, marker, files))
            })
        })
        .unwrap_or_else(|| {
            panic!(
                "none of {PROBES:?} answers with a string literal of src/\n{}",
                dev.log()
            )
        });

    // A client in a loop, counting failures, from here to the end.
    let stop = Arc::new(AtomicBool::new(false));
    let ok = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(Mutex::new(Vec::<String>::new()));
    // Several clients at once, as in a browser with a page open and an agent
    // probing: a reload must not drop a request that was queued meanwhile.
    let clients: Vec<_> = (0..4)
        .map(|_| {
            let (stop, ok, failures) = (stop.clone(), ok.clone(), failures.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match get(addr, path) {
                        Ok((s, _)) if s < 500 => {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok((s, b)) => failures.lock().unwrap().push(format!("{s} {b}")),
                        Err(e) => failures.lock().unwrap().push(e),
                    }
                }
            })
        })
        .collect();

    for round in 1..=3 {
        let new_text = format!("e2e reload {round}");
        for f in &files {
            let t = std::fs::read_to_string(f).unwrap();
            std::fs::write(
                f,
                t.replace(&marker_for(round, &marker), &format!("\"{new_text}")),
            )
            .unwrap();
        }
        dev.wait_until(
            &format!("edit {round} to answer"),
            Duration::from_secs(300),
            || get(addr, path).is_ok_and(|(_, b)| b.contains(&new_text)),
        );
    }
    // A build error keeps the old process serving.
    let broken = files[0].clone();
    let good = std::fs::read_to_string(&broken).unwrap();
    std::fs::write(&broken, format!("{good}\nfn this_does_not_compile( {{\n")).unwrap();
    std::thread::sleep(Duration::from_secs(5));
    let (s, b) = get(addr, path).expect("the old build stops answering after a failed build");
    assert!(s < 500 && b.contains("e2e reload 3"), "{s} {b}");
    std::fs::write(&broken, good).unwrap();

    stop.store(true, Ordering::Relaxed);
    for c in clients {
        c.join().unwrap();
    }
    let failures = failures.lock().unwrap();
    let served = ok.load(Ordering::Relaxed);
    eprintln!("dev reload: {served} requests over 3 reloads and a failed build");
    assert!(served > 50, "only {served} requests ran during the reloads");
    assert!(
        failures.is_empty(),
        "{} of {} requests failed during 3 reloads (want 0): first {:?}\n{}",
        failures.len(),
        served + failures.len() as u64,
        failures.first(),
        dev.log()
    );
}

/// The text to replace in round `round`: the original body first, then what
/// the round before wrote.
/// Paths tried for a route that returns a literal of the source.
const PROBES: &[&str] = &["/", "/hello/e2e", "/health"];

fn marker_for(round: u32, original: &str) -> String {
    if round == 1 {
        original.to_string()
    } else {
        format!("\"e2e reload {}", round - 1)
    }
}

/// Security point 1: `giong dev` asked to listen on every interface serves
/// on loopback instead and says so (GIONG-E5024), because on café or office
/// Wi-Fi that would put the service in development, its request log and its
/// giong-mcp port on the network. `--lan` is the way to mean it. The warning
/// comes when the sockets are bound, before any build.
#[cfg(unix)]
#[test]
fn dev_warns_before_listening_on_every_interface() {
    if !has_command("new")
        || !has_command("dev")
        || !crate::harness::toolchain("dev_warns_before_listening_on_every_interface")
    {
        return;
    }
    let dir = TempDir::new("scenario-dev-lan");
    let (out, app) = new_project(&dir, "app", &[]);
    out.expect_code(0);
    let port = free_port();
    let mut dev = Dev::start_on(&app, &format!("0.0.0.0:{port}"), port);
    dev.wait_until(
        "giong dev to bind its sockets",
        Duration::from_secs(60),
        || TcpStream::connect(("127.0.0.1", port)).is_ok(),
    );
    // This machine's address on its outgoing interface (no packet is sent).
    let lan = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|u| u.connect("10.255.255.255:9").map(|()| u))
        .and_then(|u| u.local_addr())
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified());
    if let Some(ip) = lan {
        let reached =
            TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2));
        assert!(
            reached.is_err(),
            "giong dev answers on {ip}:{port} without --lan"
        );
    }
    let start = Instant::now();
    while !dev.log().contains("GIONG-E5024") && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    dev.stop();
    std::thread::sleep(Duration::from_millis(500));
    let log = dev.log();
    assert!(
        log.contains("GIONG-E5024") && log.contains(&format!("127.0.0.1:{port}")),
        "no warning about listening on every interface:\n{log}"
    );
}
