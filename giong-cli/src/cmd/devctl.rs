//! The words of `giong dev` that control a runner instead of being one:
//! `--background`, `status`, `logs` and `stop`, and the list of running
//! `giong dev` processes behind `--all`.
//!
//! One project has one runner, in the foreground or in the background, and
//! these find it through `.giong/dev.json`. A runner is asked to stop with a
//! file, `.giong/stop`, which it looks for on each poll; it then stops its
//! service gracefully and removes its state. Only a runner that does not
//! react gets a signal. `stop` then stops what giong started for the project
//! besides (`.giong/processes.json`: a database container, its data kept).
//!
//! `--all` reads `~/.giong/run/<pid>.json` (or `$GIONG_HOME/run`): each runner
//! writes its file when it starts and removes it when it stops; a file whose
//! process is gone is removed by the next reader.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::dev::{alive, unix_ms};
use crate::args::Args;
use crate::code::Code;
use crate::procs;
use crate::project::Project;
use crate::report::{Failure, Finding, Report, Severity};

/// Set in the environment of a runner that `--background` started.
pub const BACKGROUND_VAR: &str = "GIONG_DEV_BACKGROUND";

/// How long `--background` waits for the first build to serve.
const START_WAIT: Duration = Duration::from_secs(600);

/// How long `stop` waits for a runner to stop by itself (it gives its
/// service `--grace`, 30 s by default, to finish its requests).
const STOP_WAIT: Duration = Duration::from_secs(35);

/// The folder of running `giong dev` processes, one file each.
pub fn registry_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("GIONG_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home).join("run"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".giong").join("run"))
}

/// Adds this process, serving `root`, to the list.
pub fn register(root: &Path) {
    let Some(dir) = registry_dir() else { return };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let doc = json!({"pid": std::process::id(), "root": root.display().to_string(),
                     "started_at": unix_ms()});
    let _ = fs::write(
        dir.join(format!("{}.json", std::process::id())),
        format!("{doc}\n"),
    );
}

/// Removes this process from the list.
pub fn unregister() {
    if let Some(dir) = registry_dir() {
        let _ = fs::remove_file(dir.join(format!("{}.json", std::process::id())));
    }
}

/// The projects a live `giong dev` serves, oldest first; files of processes
/// that are gone are removed.
pub fn registered() -> Vec<PathBuf> {
    let Some(dir) = registry_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(doc) = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let (Some(pid), Some(root)) = (doc["pid"].as_u64(), doc["root"].as_str()) else {
            continue;
        };
        let root = PathBuf::from(root);
        // The pid must still be the runner of that project (pids are reused).
        let state_pid = read_state(&root).and_then(|s| s["pid"].as_u64());
        if state_pid != Some(pid) || !alive(pid) {
            let _ = fs::remove_file(&path);
            continue;
        }
        found.push((doc["started_at"].as_u64().unwrap_or(0), root));
    }
    found.sort();
    let mut roots: Vec<PathBuf> = found.into_iter().map(|(_, r)| r).collect();
    roots.dedup();
    roots
}

/// `.giong/dev.json` of `root`.
pub fn read_state(root: &Path) -> Option<Value> {
    let text = fs::read_to_string(root.join(".giong").join("dev.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// The project root for the control words: the nearest directory with a
/// `.giong` folder (a workspace keeps it at its root, above the member the
/// command may run in), else the Cargo project around the current directory.
fn root_here() -> Result<PathBuf, Failure> {
    let cwd = std::env::current_dir().map_err(|e| {
        Failure::new(
            Code::NOT_A_PROJECT,
            format!("the current directory cannot be read: {e}"),
            "run giong from inside a project",
        )
    })?;
    if let Some(dir) = cwd.ancestors().find(|d| {
        d.join(".giong").join("dev.json").is_file() || d.join(".giong").join("dev.log").is_file()
    }) {
        return Ok(dir.to_path_buf());
    }
    Ok(Project::at(&cwd)?.root)
}

/// What runs for `root`: the fields of `giong dev status`, and findings when
/// its runner runs but does not serve.
pub fn project_status(root: &Path) -> (Value, Vec<Finding>) {
    let state = read_state(root).unwrap_or(Value::Null);
    let pid = state["pid"].as_u64();
    let running = pid.is_some_and(|p| p != u64::from(std::process::id()) && alive(p));
    let service_pid = state["service_pid"]
        .as_u64()
        .filter(|p| running && alive(*p));
    let addresses: Vec<String> = state["addresses"]
        .as_array()
        .or_else(|| state["listen"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_str().map(String::from))
        .collect();
    let urls: Vec<String> = addresses.iter().map(|a| format!("http://{a}")).collect();
    let build = if running {
        state["build"].clone()
    } else {
        Value::Null
    };
    let build_state = build["state"].as_str().unwrap_or("");
    let word = if !running {
        "stopped"
    } else if build_state == "building" {
        "building"
    } else if service_pid.is_some() {
        "serving"
    } else {
        "failed"
    };
    let mut findings = Vec::new();
    if running && build_state == "failed" {
        findings.push(
            Finding::new(Code::BUILD_FAILED, Severity::Error, "the last build failed")
                .detail(build["error"].as_str().unwrap_or("").to_string())
                .fix(if service_pid.is_some() {
                    "fix the error and save: giong dev builds again by itself (the previous build \
                 keeps serving until then)"
                } else {
                    "fix the error and save: giong dev builds again by itself"
                }),
        );
    } else if word == "failed" {
        findings.push(
            Finding::new(
                Code::SERVICE_EXITED,
                Severity::Error,
                "giong dev runs, but its service has stopped",
            )
            .fix("giong dev logs shows its last lines; fix the cause and save a file to start it again"),
        );
    }
    let processes: Vec<Value> = procs::list(root)
        .iter()
        .map(|e| {
            let mut v = procs::to_json(e);
            v["running"] = json!(container_running(&e.engine, &e.name));
            v
        })
        .collect();
    let log = root.join(".giong").join("dev.log");
    let runner_log = root.join(".giong").join("runner.log");
    let background = running && state["background"].as_bool().unwrap_or(false);
    let doc = json!({
        "root": root.display().to_string(),
        "running": running,
        "pid": pid.filter(|_| running),
        "service_pid": service_pid,
        "state": word,
        "urls": if running { json!(urls) } else { json!([]) },
        "addresses": if running { json!(addresses) } else { json!([]) },
        "build": build,
        "restarts": if running { state["restarts"].clone() } else { Value::Null },
        "log": log.is_file().then(|| log.display().to_string()),
        "runner_log": (background && runner_log.is_file()).then(|| runner_log.display().to_string()),
        "background": background,
        "started_at": if running { state["started_at"].clone() } else { Value::Null },
        "processes": processes,
    });
    (doc, findings)
}

/// Whether a container runs: `None` when its engine cannot be asked.
fn container_running(engine: &str, name: &str) -> Option<bool> {
    if engine.is_empty() {
        return None;
    }
    let out = Process::new(engine)
        .args(["inspect", "--format", "{{.State.Running}}", name])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim() == "true")
    } else {
        // No such container (removed by hand): not running.
        Some(false)
    }
}

/// Lines for people about one project's status.
fn status_lines(p: &Value) -> Vec<String> {
    let root = p["root"].as_str().unwrap_or("");
    let mut out = Vec::new();
    if p["running"] == true {
        let urls: Vec<&str> = p["urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let at = if urls.is_empty() {
            "at the address the service chose (see giong dev logs)".to_string()
        } else {
            format!("on {}", urls.join(", "))
        };
        let how = match p["state"].as_str() {
            Some("serving") => format!("serves {at}"),
            Some("building") if p["service_pid"].is_u64() => {
                format!("is building; the previous build serves {at}")
            }
            Some("building") => "is building".to_string(),
            _ => "runs, but nothing serves".to_string(),
        };
        out.push(format!(
            "giong dev {how} (pid {}{}, {} restarts) in {root}",
            p["pid"],
            if p["background"] == true {
                ", in the background"
            } else {
                ""
            },
            p["restarts"].as_u64().unwrap_or(0),
        ));
        out.push("  Output: giong dev logs. Stop: giong dev stop.".to_string());
    } else {
        out.push(format!("giong dev does not run in {root}"));
    }
    for e in p["processes"].as_array().into_iter().flatten() {
        let state = match e["running"].as_bool() {
            Some(true) => "running",
            Some(false) => "stopped",
            None => "unknown",
        };
        out.push(format!(
            "  {} {} ({}): {state}, started by {}",
            e["kind"].as_str().unwrap_or(""),
            e["name"].as_str().unwrap_or(""),
            e["engine"].as_str().unwrap_or(""),
            e["started_by"].as_str().unwrap_or(""),
        ));
    }
    out
}

/// `giong dev status [--all]`.
pub fn status(args: &Args) -> Result<Report, Failure> {
    let mut report = Report::new("dev");
    if args.has("all") {
        let roots = registered();
        let mut projects = Vec::new();
        for root in &roots {
            let (p, findings) = project_status(root);
            for l in status_lines(&p) {
                report.line(l);
            }
            for f in findings {
                report.push(f);
            }
            projects.push(p);
        }
        if roots.is_empty() {
            report.line("No giong dev runs on this machine.");
        }
        report.data("projects", json!(projects));
        return Ok(report);
    }
    let root = root_here()?;
    let (p, findings) = project_status(&root);
    for l in status_lines(&p) {
        report.line(l);
    }
    for f in findings {
        report.push(f);
    }
    if let Value::Object(fields) = p {
        for (k, v) in fields {
            report.data(&k, v);
        }
    }
    Ok(report)
}

/// `giong dev logs [--lines N]`: the end of the service's output.
pub fn logs(args: &Args) -> Result<Report, Failure> {
    let n = args.number::<usize>("lines")?.unwrap_or(50).min(100_000);
    let root = root_here()?;
    let path = root.join(".giong").join("dev.log");
    let mut report = Report::new("dev");
    let text = match fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => {
            report.line(format!(
                "No output yet: giong dev has not run in {}",
                root.display()
            ));
            report.data("log", Value::Null);
            report.data("lines", json!([]));
            return Ok(report);
        }
    };
    let all: Vec<&str> = text.lines().collect();
    let tail = &all[all.len().saturating_sub(n)..];
    for l in tail {
        report.line(*l);
    }
    report.data("log", json!(path.display().to_string()));
    report.data("lines", json!(tail));
    Ok(report)
}

/// `giong dev stop [--all]`.
pub fn stop(args: &Args) -> Result<Report, Failure> {
    let roots = if args.has("all") {
        registered()
    } else {
        vec![root_here()?]
    };
    let mut report = Report::new("dev");
    let mut stopped = Vec::new();
    for root in &roots {
        stop_project(root, &mut report, &mut stopped);
    }
    if stopped.is_empty() {
        report.line(if args.has("all") {
            "Nothing to stop: no giong dev runs on this machine.".to_string()
        } else {
            format!(
                "Nothing to stop: giong dev does not run in {}",
                roots[0].display()
            )
        });
    }
    report.data("stopped", json!(stopped));
    Ok(report)
}

fn stop_project(root: &Path, report: &mut Report, stopped: &mut Vec<Value>) {
    let state_dir = root.join(".giong");
    let state = read_state(root).unwrap_or(Value::Null);
    let pid = state["pid"].as_u64();
    if let Some(pid) = pid.filter(|p| *p != u64::from(std::process::id()) && alive(*p)) {
        let request = state_dir.join("stop");
        let _ = fs::write(&request, b"");
        // A runner in a build looks for the request only once cargo ends.
        let wait = if state["build"]["state"] == "building" {
            Duration::from_secs(5)
        } else {
            STOP_WAIT
        };
        let until = Instant::now() + wait;
        while alive(pid) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
        let forced = alive(pid);
        if forced {
            force_stop(pid, &state);
        }
        let _ = fs::remove_file(&request);
        if !alive(pid) {
            let _ = fs::remove_file(state_dir.join("dev.json"));
        }
        report.line(format!(
            "Stopped giong dev (pid {pid}){} in {}",
            if forced {
                " with a signal: it did not stop by itself"
            } else {
                ""
            },
            root.display()
        ));
        stopped.push(json!({"kind": "dev", "pid": pid, "root": root.display().to_string()}));
    } else if state.is_object() {
        // A runner that was killed: its service may still run.
        if let Some(service) = super::dev::stop_leftover(&state) {
            report.line(format!(
                "Stopped the service (pid {service}) a killed giong dev left in {}",
                root.display()
            ));
            stopped.push(json!({"kind": "service", "pid": service,
                                "root": root.display().to_string()}));
        }
        let _ = fs::remove_file(state_dir.join("dev.json"));
    }
    for e in procs::list(root) {
        if e.kind != "db" || e.engine.is_empty() {
            continue;
        }
        let out = Process::new(&e.engine)
            .args(["stop", &e.name])
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() || container_running(&e.engine, &e.name) == Some(false) => {
                let _ = procs::remove(root, &e.kind, &e.name);
                report.line(format!(
                    "Stopped the database container {} (its data is kept; giong db up starts it \
                     again)",
                    e.name
                ));
                stopped.push(json!({"kind": "db", "name": e.name, "engine": e.engine,
                                    "root": root.display().to_string()}));
            }
            Ok(o) => report.push(
                Finding::new(
                    Code::USAGE,
                    Severity::Warning,
                    format!("the database container {} did not stop", e.name),
                )
                .detail(String::from_utf8_lossy(&o.stderr).trim().to_string())
                .fix(format!("{} stop {}", e.engine, e.name)),
            ),
            Err(err) => report.push(
                Finding::new(
                    Code::USAGE,
                    Severity::Warning,
                    format!("{} cannot be run to stop {}: {err}", e.engine, e.name),
                )
                .fix(format!("{} stop {}", e.engine, e.name)),
            ),
        }
    }
}

/// A runner that did not stop by itself: SIGTERM, then SIGKILL, to its
/// process group when `--background` gave it one (cargo and the service go
/// with it), else to it and its service.
fn force_stop(pid: u64, state: &Value) {
    #[cfg(unix)]
    {
        let group = state["background"].as_bool().unwrap_or(false);
        let mut targets = vec![if group {
            format!("-{pid}")
        } else {
            pid.to_string()
        }];
        if let Some(s) = state["service_pid"].as_u64() {
            targets.push(s.to_string());
        }
        for signal in ["-TERM", "-KILL"] {
            let _ = Process::new("kill")
                .arg(signal)
                .arg("--")
                .args(&targets)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let until = Instant::now() + Duration::from_secs(3);
            while alive(pid) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(100));
            }
            if !alive(pid) {
                break;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = state;
        let _ = Process::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The arguments of the runner `--background` starts: the same, without
/// `--background`, with `--json` (its events go to `.giong/runner.log`).
fn runner_argv(argv: &[String]) -> Vec<String> {
    let split = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let (head, tail) = argv.split_at(split);
    let mut out: Vec<String> = head
        .iter()
        .filter(|a| *a != "--background")
        .cloned()
        .collect();
    if !out.iter().any(|a| a == "--json") {
        out.push("--json".into());
    }
    out.extend(tail.iter().cloned());
    out
}

/// A finding read back from a runner's JSON event.
fn finding_from_json(v: &Value) -> Option<Finding> {
    let code = v["code"].as_str()?;
    let number: u16 = code.strip_prefix("GIONG-E")?.parse().ok()?;
    let code = Code::of(number).unwrap_or_else(|| Code::runtime(number, ""));
    let severity = match v["severity"].as_str() {
        Some("warning") => Severity::Warning,
        Some("info") => Severity::Info,
        _ => Severity::Error,
    };
    let mut f = Finding::new(code, severity, v["title"].as_str().unwrap_or(""))
        .detail(v["detail"].as_str().unwrap_or(""))
        .fix(v["fix"].as_str().unwrap_or(""));
    if let (Some(file), Some(line)) = (v["file"].as_str(), v["line"].as_u64()) {
        f = f.at(file, line as u32, v["column"].as_u64().unwrap_or(1) as u32);
    }
    Some(f)
}

/// How a background start ended.
enum Outcome {
    Serving,
    Failed(Vec<Finding>),
    /// The runner itself ended (its last line is its report).
    Ended(Value),
}

/// `giong dev --background`: starts the runner detached from this terminal,
/// waits until the first build serves (or fails), says where, and returns.
pub fn background(args: &Args, argv: &[String]) -> Result<Report, Failure> {
    if args.has("once") {
        return Err(Failure::new(
            Code::USAGE,
            "--once and --background do not go together",
            "use giong dev --once to run the service until it exits, or giong dev --background \
             to keep it running and rebuilding",
        ));
    }
    let project = Project::here()?;
    let root = project.root.clone();
    let mut report = Report::new("dev");
    let (now, _) = project_status(&root);
    if now["running"] == true {
        report.line("giong dev already runs for this project:".to_string());
        return Ok(fill(report, &root));
    }
    let state_dir = root.join(".giong");
    let _ = fs::create_dir_all(&state_dir);
    let log_path = state_dir.join("runner.log");
    let cannot = |e: std::io::Error| {
        Failure::new(
            Code::USAGE,
            format!("{} cannot be written: {e}", log_path.display()),
            "check that the project directory is writable",
        )
    };
    let log = File::create(&log_path).map_err(cannot)?;
    let err = log.try_clone().map_err(cannot)?;
    let exe = std::env::current_exe().map_err(|e| {
        Failure::new(
            Code::USAGE,
            format!("giong cannot find itself: {e}"),
            "run giong by its full path",
        )
    })?;
    let mut cmd = Process::new(exe);
    cmd.args(runner_argv(argv))
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .env(BACKGROUND_VAR, "1");
    detach(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        Failure::new(
            Code::USAGE,
            format!("giong dev cannot be started in the background: {e}"),
            "run giong dev in a terminal of its own",
        )
    })?;
    let deadline = Instant::now() + START_WAIT;
    let mut reader = File::open(&log_path).map_err(cannot)?;
    let mut offset = 0u64;
    let mut partial = String::new();
    let mut outcome = None;
    let mut last_doc = Value::Null;
    let mut warnings = Vec::new();
    while outcome.is_none() {
        std::thread::sleep(Duration::from_millis(100));
        let ended = matches!(child.try_wait(), Ok(Some(_)));
        let mut chunk = String::new();
        if reader.seek(SeekFrom::Start(offset)).is_ok() {
            if let Ok(n) = reader.read_to_string(&mut chunk) {
                offset += n as u64;
            }
        }
        partial.push_str(&chunk);
        while let Some(end) = partial.find('\n') {
            let line: String = partial.drain(..=end).collect();
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            match v["event"].as_str() {
                Some("ready") => outcome = Some(Outcome::Serving),
                Some("build_failed") => {
                    let mut found: Vec<Finding> = v["findings"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(finding_from_json)
                        .filter(|f| f.severity == Severity::Error)
                        .collect();
                    if found.is_empty() {
                        found.push(
                            Finding::new(
                                Code::BUILD_FAILED,
                                Severity::Error,
                                v["message"].as_str().unwrap_or("the build failed"),
                            )
                            .fix("fix the error and save: giong dev builds again by itself"),
                        );
                    }
                    outcome = Some(Outcome::Failed(found));
                }
                Some("start_failed" | "exited") => {
                    let found = finding_from_json(&v["finding"]).unwrap_or_else(|| {
                        Finding::new(
                            Code::SERVICE_EXITED,
                            Severity::Error,
                            v["message"].as_str().unwrap_or("the service stopped"),
                        )
                    });
                    outcome = Some(Outcome::Failed(vec![found]));
                }
                // A warning with a code (LISTEN on every interface...).
                Some("warning") if v["code"].is_string() => {
                    warnings.push(
                        Finding::new(
                            finding_from_json(&json!({"code": v["code"]}))
                                .map(|f| f.code)
                                .unwrap_or(Code::USAGE),
                            Severity::Warning,
                            v["message"].as_str().unwrap_or(""),
                        )
                        .detail(v["detail"].as_str().unwrap_or(""))
                        .fix(v["fix"].as_str().unwrap_or("")),
                    );
                }
                Some(_) => {}
                None if v.get("command").is_some() => last_doc = v,
                None => {}
            }
            if outcome.is_some() {
                break;
            }
        }
        if outcome.is_none() && ended {
            outcome = Some(Outcome::Ended(last_doc.clone()));
        }
        if outcome.is_none() && Instant::now() > deadline {
            report.line(format!(
                "giong dev still builds after {} s; giong dev status says when it serves",
                START_WAIT.as_secs()
            ));
            return Ok(fill(report, &root));
        }
    }
    for w in warnings {
        report.push(w);
    }
    match outcome.expect("set above") {
        Outcome::Serving => {
            report.line("giong dev runs in the background:".to_string());
        }
        Outcome::Failed(findings) => {
            report.line(
                "giong dev runs in the background, but the service does not serve yet: fix the \
                 errors below and save, and it builds again by itself."
                    .to_string(),
            );
            for f in findings {
                report.push(f);
            }
        }
        Outcome::Ended(doc) => {
            if let Some(e) = doc.get("error") {
                let number = e["code"]
                    .as_str()
                    .and_then(|c| c.strip_prefix("GIONG-E"))
                    .and_then(|n| n.parse().ok())
                    .and_then(Code::of)
                    .unwrap_or(Code::USAGE);
                return Err(Failure::new(
                    number,
                    e["message"].as_str().unwrap_or("giong dev stopped"),
                    e["fix"].as_str().unwrap_or(""),
                ));
            }
            for f in doc["findings"].as_array().into_iter().flatten() {
                if let Some(f) = finding_from_json(f) {
                    report.push(f);
                }
            }
            if report.ok() {
                report.push(
                    Finding::new(
                        Code::SERVICE_EXITED,
                        Severity::Error,
                        "giong dev stopped right after it started",
                    )
                    .fix(format!("read {}", log_path.display())),
                );
            }
            return Ok(report);
        }
    }
    Ok(fill(report, &root))
}

/// The status of `root` into `report`, with how to follow and stop it.
fn fill(mut report: Report, root: &Path) -> Report {
    let (p, findings) = project_status(root);
    for l in status_lines(&p) {
        report.line(l);
    }
    // Findings of the status repeat those of the start (a failed build).
    if report.ok() {
        for f in findings {
            report.push(f);
        }
    }
    if let Value::Object(fields) = p {
        for (k, v) in fields {
            report.data(&k, v);
        }
    }
    report
}

/// Detaches the runner from this terminal: its own process group (Ctrl-C
/// and the end of an agent's shell do not reach it); on Windows, no console.
fn detach(cmd: &mut Process) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

/// The process that listens on `port`, when this machine says: its pid and
/// command line.
pub fn port_owner(port: u16) -> Option<(u32, String)> {
    #[cfg(target_os = "linux")]
    {
        linux_port_owner(port)
    }
    #[cfg(target_os = "macos")]
    {
        let out = Process::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let pid = text
            .lines()
            .find_map(|l| l.strip_prefix('p')?.parse().ok())?;
        let name = text
            .lines()
            .find_map(|l| l.strip_prefix('c'))
            .unwrap_or("")
            .to_string();
        Some((pid, name))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = port;
        None
    }
}

#[cfg(target_os = "linux")]
fn linux_port_owner(port: u16) -> Option<(u32, String)> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // local_address (hex ip:port), st (0A = LISTEN), inode.
            if cols.len() < 10 || cols[3] != "0A" {
                continue;
            }
            let local_port = cols[1]
                .rsplit_once(':')
                .and_then(|(_, p)| u16::from_str_radix(p, 16).ok());
            if local_port == Some(port) && cols[9] != "0" {
                inodes.push(format!("socket:[{}]", cols[9]));
            }
        }
    }
    if inodes.is_empty() {
        return None;
    }
    let mut pids: Vec<u32> = fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    pids.sort_unstable();
    for pid in pids {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if inodes.iter().any(|i| target.as_os_str() == i.as_str()) {
                let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                let mut text = String::from_utf8_lossy(&cmdline)
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if text.chars().count() > 100 {
                    text = text.chars().take(100).collect::<String>() + "...";
                }
                return Some((pid, text));
            }
        }
    }
    None
}

/// What to say about the process that holds `port`: who it is and how to
/// stop it, naming the project when a `giong dev` started it.
pub fn port_owner_fix(port: u16) -> Option<String> {
    let (pid, command) = port_owner(port)?;
    let shown = if command.is_empty() {
        format!("pid {pid}")
    } else {
        format!("pid {pid}, {command}")
    };
    for root in registered() {
        let state = read_state(&root).unwrap_or(Value::Null);
        let ours =
            [state["pid"].as_u64(), state["service_pid"].as_u64()].contains(&Some(u64::from(pid)));
        if ours {
            return Some(format!(
                "giong dev of {} listens there ({shown}): stop it with giong dev stop in that \
                 project (or giong dev stop --all), or set another LISTEN in .env",
                root.display()
            ));
        }
    }
    Some(format!(
        "{shown} listens there: stop it (kill {pid}), or set another LISTEN in .env"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn the_runner_gets_the_same_arguments_with_json_and_without_background() {
        assert_eq!(
            runner_argv(&v("dev --background --listen 127.0.0.1:8080")),
            v("dev --listen 127.0.0.1:8080 --json")
        );
        assert_eq!(
            runner_argv(&v("dev --json --background -- --background x")),
            v("dev --json -- --background x")
        );
    }

    #[test]
    fn findings_come_back_from_events() {
        let f = Finding::new(Code::BUILD_FAILED, Severity::Error, "mismatched types")
            .detail("expected u32")
            .fix("change it")
            .at("src/main.rs", 3, 9);
        assert_eq!(finding_from_json(&f.to_json()), Some(f));
        assert_eq!(finding_from_json(&json!({"title": "x"})), None);
    }

    #[test]
    fn a_project_without_a_runner_is_stopped() {
        let root = std::env::temp_dir().join(format!("giong-devctl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".giong")).unwrap();
        let (p, findings) = project_status(&root);
        assert_eq!(p["running"], false);
        assert_eq!(p["state"], "stopped");
        assert!(findings.is_empty());
        // A dev.json whose process is gone.
        fs::write(
            root.join(".giong").join("dev.json"),
            r#"{"pid": 4294967, "service_pid": null, "build": {"state": "failed"}}"#,
        )
        .unwrap();
        let (p, findings) = project_status(&root);
        assert_eq!(p["running"], false);
        assert!(findings.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_owner_of_a_port_is_found() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        if !Path::new("/proc/net/tcp").exists() {
            return;
        }
        let (pid, _) = port_owner(port).expect("this process listens");
        assert_eq!(pid, std::process::id());
    }
}
