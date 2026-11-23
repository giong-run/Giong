//! `giong dev`: build the service, run it, and on every save build and
//! restart it, without refusing a request.
//!
//! How a restart refuses nothing (Unix): `giong dev` binds the service's
//! listening sockets itself, once, and starts each build with them open
//! (`GIONG_DEV_LISTEN`), so the service's `TcpListener::bind` shares them and
//! the kernel's queue of connections outlives every build. The new build
//! starts while the old one still serves; when it says it accepts (a byte on
//! `GIONG_DEV_READY`), the old one gets `SIGTERM` and finishes what it was
//! doing within `--grace`. A build that fails leaves the old one serving.
//!
//! The addresses come from `--listen`, or `LISTEN` in the environment or in
//! `.env`. Without one, or on a giong older than 1.5, or on Windows, the old
//! build is stopped before the new one starts.
//!
//! On a laptop it starts the service with `WORKERS=2` unless `WORKERS` is set
//! (`--prod-like` leaves it to the service). It also sets `RUST_BACKTRACE=1`
//! and a per-process giong-mcp socket (`GIONG_MCP=unix:.giong/mcp-*.sock`, or under /tmp when that path is too long for a socket),
//! unless they are set, and every variable of `.env` the environment does not
//! set already.
//!
//! For other tools it keeps `.giong/dev.json` (what runs, where, the last
//! build) and `.giong/dev.log` (the service's output). With `--json` it prints
//! one JSON object per line for each event, and the service's output goes to
//! the log only.

// A finding is the error of the start-up steps: it goes into the report.
#![allow(clippy::result_large_err)]

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::build;
use crate::code::Code;
use crate::dotenv;
use crate::project::Project;
use crate::report::{Failure, Finding, Report, Severity};
use crate::watch::{self, Change, Snapshot};

pub const COMMAND: Command = Command {
    name: "dev",
    summary: "build and run the service, and restart it on every save without refusing a request",
    usage: "giong dev [--bin NAME] [--release] [--listen ADDR[,ADDR]] [--workers N] [--prod-like] \
            [--once] [--grace S] [--ready-timeout S] [--ui] [--ui-port N] [-- service arguments]\n\n\
            Builds the service and runs it; on every save it builds again and swaps the new build \
            in once it accepts connections, so no request is refused. A failed build leaves the \
            running one serving. A change to .env alone restarts without building. The service \
            gets the variables of .env, WORKERS=2 (unless set, or --prod-like), RUST_BACKTRACE=1 \
            and a giong-mcp socket. --once builds, runs the service until it exits, and stops. \
            The listening addresses come from --listen or LISTEN (environment, then .env); \
            0.0.0.0 or [::] is served on loopback (127.0.0.1, ::1) unless --lan is given. \
            --ui also serves the dashboard (giong dashboard) on 127.0.0.1:7700 or a port after it.",
    flags: &[
        Flag {
            name: "bin",
            kind: Kind::Value,
            help: "the binary to run, when the package has several",
        },
        Flag {
            name: "release",
            kind: Kind::Switch,
            help: "build with the release profile",
        },
        Flag {
            name: "listen",
            kind: Kind::Value,
            help: "the addresses the service listens on, such as 127.0.0.1:8080 (comma-separated)",
        },
        Flag {
            name: "lan",
            kind: Kind::Switch,
            help: "let other machines reach the service: keep 0.0.0.0 or [::] in LISTEN \
                   (otherwise giong dev serves those on 127.0.0.1 or ::1)",
        },
        Flag {
            name: "workers",
            kind: Kind::Value,
            help: "WORKERS for the service (default 2 on a laptop)",
        },
        Flag {
            name: "prod-like",
            kind: Kind::Switch,
            help: "leave WORKERS to the service (every core)",
        },
        Flag {
            name: "once",
            kind: Kind::Switch,
            help: "build, run until the service exits, and stop (no watching)",
        },
        Flag {
            name: "grace",
            kind: Kind::Value,
            help: "seconds an old build has to finish after SIGTERM (default 30)",
        },
        Flag {
            name: "ready-timeout",
            kind: Kind::Value,
            help: "seconds a new build has to start accepting (default 30)",
        },
        Flag {
            name: "ui",
            kind: Kind::Switch,
            help: "also serve the dashboard, a local page showing the service (giong dashboard)",
        },
        Flag {
            name: "ui-port",
            kind: Kind::Value,
            help: "the dashboard's port, or the first of ten to try (default 7700; implies --ui)",
        },
    ],
    max_words: 0,
    run,
};

/// How often the files are looked at.
const POLL: Duration = Duration::from_millis(250);
/// How long the files must stay unchanged before a build starts (an editor
/// saving several files, a formatter running after a save).
const SETTLE: Duration = Duration::from_millis(120);
/// How long a new build must stay up after it binds before it replaces the
/// running one.
const SETTLE_AFTER_READY: Duration = Duration::from_millis(200);
/// The last lines of a service's output kept for a report.
const TAIL: usize = 40;

fn run(args: &Args) -> Result<Report, Failure> {
    let project = Project::here()?;
    let mut dev = Dev::new(project, args)?;
    let result = dev.serve();
    dev.shutdown();
    result
}

/// Where a build is.
#[derive(Clone, Debug, Default)]
struct BuildState {
    state: &'static str,
    started_at: u64,
    finished_at: Option<u64>,
    took_ms: Option<u64>,
    errors: usize,
    warnings: usize,
    error: Option<String>,
}

/// A running build of the service.
struct Service {
    child: Child,
    pid: u32,
    mcp: PathBuf,
    tail: Arc<Mutex<VecDeque<String>>>,
    /// giong dev's end of the ready channel: `\n` comes for each bind of a
    /// handed-down socket, `s` asks to stop accepting, `d` answers it.
    #[cfg(unix)]
    ready: Option<std::os::unix::net::UnixStream>,
    /// The `\n` bytes read so far.
    #[cfg(unix)]
    binds: usize,
}

/// An old build finishing its requests.
struct Retiring {
    child: Child,
    kill_at: Instant,
}

struct Dev {
    project: Project,
    json: bool,
    package: String,
    bin: String,
    release: bool,
    service_args: Vec<String>,
    workers: Option<String>,
    prod_like: bool,
    once: bool,
    grace: Duration,
    ready_timeout: Duration,
    /// The dashboard's port, with --ui or --ui-port.
    ui: Option<u16>,
    listen: Vec<SocketAddr>,
    /// The addresses the service is told to listen on (`LISTEN`), after
    /// [`loopback_unless_lan`]; empty: the service's own choice.
    addresses: Vec<SocketAddr>,
    #[cfg(unix)]
    listeners: Vec<socket2::Socket>,
    exe: Option<PathBuf>,
    log_path: PathBuf,
    log: Arc<Mutex<File>>,
    current: Option<Service>,
    retiring: Vec<Retiring>,
    /// Builds started after the first one that served.
    restarts: u64,
    /// A build has started once.
    served: bool,
    /// Every start attempted: names each service's giong-mcp socket, so a new
    /// build never takes the socket of the one still serving.
    starts: u64,
    started_at: u64,
    build: BuildState,
    report: Report,
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn secs(args: &Args, name: &str, default: u64) -> Result<Duration, Failure> {
    Ok(Duration::from_secs(
        args.number::<u64>(name)?.unwrap_or(default).min(3600),
    ))
}

impl Dev {
    fn new(project: Project, args: &Args) -> Result<Dev, Failure> {
        let state_dir = project.root.join(".giong");
        refuse_if_running(&state_dir)?;
        fs::create_dir_all(&state_dir).map_err(|e| {
            Failure::new(
                Code::USAGE,
                format!("{} cannot be created: {e}", state_dir.display()),
                "check that the project directory is writable",
            )
        })?;
        let (package, bin) = pick_binary(&project, args.value("bin"))?;
        let workers = match args.number::<u32>("workers")? {
            Some(0) => {
                return Err(Failure::new(
                    Code::USAGE,
                    "--workers 0 runs nothing",
                    "give 1 or more",
                ))
            }
            n => n.map(|n| n.to_string()),
        };
        let env_file = dotenv::read(&project.root.join(".env"))
            .map(|(v, _)| v)
            .unwrap_or_default();
        let listen = listen_addresses(args.value("listen"), &env_file)?;
        let (listen, network) = loopback_unless_lan(listen, args.has("lan"));
        let log_path = state_dir.join("dev.log");
        let log = File::create(&log_path).map_err(|e| {
            Failure::new(
                Code::USAGE,
                format!("{} cannot be written: {e}", log_path.display()),
                "check that the project directory is writable",
            )
        })?;
        let mut dev = Dev {
            json: args.json(),
            package,
            bin,
            release: args.has("release"),
            service_args: args.rest.clone(),
            workers,
            prod_like: args.has("prod-like"),
            once: args.has("once"),
            grace: secs(args, "grace", 30)?,
            ready_timeout: secs(args, "ready-timeout", 30)?,
            ui: match args.number::<u16>("ui-port")? {
                Some(port) => Some(port),
                None => args.has("ui").then_some(crate::dashboard::PORT),
            },
            listen: Vec::new(),
            addresses: listen.clone(),
            #[cfg(unix)]
            listeners: Vec::new(),
            exe: None,
            log_path,
            log: Arc::new(Mutex::new(log)),
            current: None,
            retiring: Vec::new(),
            restarts: 0,
            starts: 0,
            served: false,
            started_at: unix_ms(),
            build: BuildState::default(),
            report: Report::new("dev"),
            project,
        };
        for finding in network {
            dev.say(
                "warning",
                finding.title.clone(),
                json!({"code": finding.code.to_string(), "detail": finding.detail, "fix": finding.fix}),
            );
            dev.report.push(finding);
        }
        dev.bind(listen)?;
        Ok(dev)
    }

    /// Binds the listening sockets the service will share (Unix, and a giong
    /// that takes them).
    fn bind(&mut self, listen: Vec<SocketAddr>) -> Result<(), Failure> {
        if listen.is_empty() {
            if self.once {
                return Ok(());
            }
            self.say(
                "note",
                "no LISTEN address (in --listen, the environment or .env): each restart stops \
                 the old build first, so connections are refused for a moment"
                    .into(),
                json!({}),
            );
            return Ok(());
        }
        #[cfg(unix)]
        {
            if !giong_takes_sockets(&self.project) {
                self.say(
                    "note",
                    "this project's giong is older than 1.5, which cannot share giong dev's \
                     sockets: each restart stops the old build first"
                        .into(),
                    json!({}),
                );
                return Ok(());
            }
            for addr in &listen {
                let sock = listen_on(*addr)?;
                self.listeners.push(sock);
            }
            self.listen = listen;
        }
        #[cfg(not(unix))]
        {
            let _ = listen;
            self.say(
                "note",
                "on Windows each restart stops the old build first".into(),
                json!({}),
            );
        }
        Ok(())
    }

    fn serve(&mut self) -> Result<Report, Failure> {
        let roots = vec![self.project.root.clone()];
        let mut seen = Snapshot::take(&roots);
        if seen.truncated {
            self.report.push(watch_limit());
            self.say(
                "warning",
                format!(
                    "more than {} files under the project: only the first are watched",
                    watch::MAX_FILES
                ),
                json!({}),
            );
        }
        if let Some(port) = self.ui {
            self.start_dashboard(port);
        }
        let changed_at = Instant::now();
        self.rebuild(changed_at);
        if self.once {
            return self.wait_once();
        }
        self.say(
            "watching",
            format!(
                "watching {} for changes (Ctrl-C stops)",
                self.project.root.display()
            ),
            json!({}),
        );
        loop {
            std::thread::sleep(POLL);
            self.reap();
            self.check_current();
            let now = Snapshot::take(&roots);
            let mut changed = now.changed_since(&seen);
            if changed.is_empty() {
                continue;
            }
            let changed_at = Instant::now();
            // Wait for the editor (or a formatter) to finish writing.
            let mut settled = now;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                std::thread::sleep(SETTLE);
                let again = Snapshot::take(&roots);
                let more = again.changed_since(&settled);
                if more.is_empty() || Instant::now() > deadline {
                    settled = again;
                    break;
                }
                changed.extend(more);
                settled = again;
            }
            changed.sort();
            changed.dedup();
            seen = settled;
            let shown: Vec<String> = changed
                .iter()
                .take(3)
                .map(|p| {
                    p.strip_prefix(&self.project.root)
                        .unwrap_or(p)
                        .display()
                        .to_string()
                })
                .collect();
            let more = changed.len().saturating_sub(3);
            self.say(
                "changed",
                format!(
                    "{}{} changed",
                    shown.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                ),
                json!({"files": changed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()}),
            );
            match watch::classify(&changed) {
                Some(Change::Env) => self.restart(changed_at),
                Some(Change::Code) => self.rebuild(changed_at),
                None => {}
            }
        }
    }

    /// Builds, and restarts if the build worked.
    fn rebuild(&mut self, changed_at: Instant) {
        if self.build_once(changed_at) {
            self.restart(changed_at);
        }
    }

    /// `--once`: the service runs until it exits.
    fn wait_once(&mut self) -> Result<Report, Failure> {
        let Some(mut service) = self.current.take() else {
            // The build failed or the service did not start: the findings
            // say why.
            self.report.data("build", self.build_json());
            return Ok(std::mem::replace(&mut self.report, Report::new("dev")));
        };
        let status = service.child.wait().ok();
        self.service_ended(&service, status);
        self.report.data("build", self.build_json());
        self.report
            .data("exit_code", json!(status.and_then(|s| s.code())));
        Ok(std::mem::replace(&mut self.report, Report::new("dev")))
    }

    /// Builds; on failure the diagnostics are shown and the old build keeps
    /// serving.
    fn build_once(&mut self, changed_at: Instant) -> bool {
        self.build = BuildState {
            state: "building",
            started_at: unix_ms(),
            ..BuildState::default()
        };
        self.write_state();
        self.say("build", "building".into(), json!({}));
        let mut cargo_args = vec![
            "--package".to_string(),
            self.package.clone(),
            "--bin".to_string(),
            self.bin.clone(),
        ];
        if self.release {
            cargo_args.push("--release".into());
        }
        // A release build has giong's socket hand-off only with its `dev`
        // feature: on for giong dev's own builds (debug builds always have
        // it, so --prod-like adds it too only for the same build either way).
        #[cfg(unix)]
        if (self.release || self.prod_like) && giong_takes_sockets(&self.project) {
            for f in dev_features(&self.project.metadata, &self.package) {
                cargo_args.push("--features".into());
                cargo_args.push(f);
            }
        }
        let outcome = match build::run(&self.project.root, "build", &cargo_args) {
            Ok(o) => o,
            Err(f) => {
                self.build.state = "failed";
                self.build.error = Some(f.what.clone());
                self.build.finished_at = Some(unix_ms());
                self.write_state();
                self.report
                    .push(Finding::new(f.code, Severity::Error, f.what).fix(f.fix));
                return false;
            }
        };
        let errors = outcome
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count();
        let warnings = outcome
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count();
        self.build.finished_at = Some(unix_ms());
        self.build.took_ms = Some(changed_at.elapsed().as_millis() as u64);
        self.build.errors = errors;
        self.build.warnings = warnings;
        if !self.json {
            // Errors first, as cargo would show them, in colour.
            let mut err = std::io::stderr().lock();
            for r in &outcome.rendered {
                let _ = write!(err, "{r}");
            }
        }
        if !outcome.ok {
            self.build.state = "failed";
            self.build.error = outcome.findings.first().map(|f| f.title.clone());
            self.write_state();
            let running = if self.current.is_some() {
                "; the running build keeps serving"
            } else {
                ""
            };
            self.say(
                "build_failed",
                format!(
                    "build failed ({errors} {}){running}",
                    if errors == 1 { "error" } else { "errors" }
                ),
                json!({"findings": outcome.findings.iter().map(Finding::to_json).collect::<Vec<_>>()}),
            );
            if self.once {
                for f in outcome.findings {
                    self.report.push(f);
                }
            }
            return false;
        }
        let exe = outcome
            .executables
            .iter()
            .find(|(_, name)| *name == self.bin)
            .or_else(|| outcome.executables.first())
            .map(|(p, _)| p.clone());
        let Some(exe) = exe else {
            self.build.state = "failed";
            self.build.error = Some(format!("cargo built no binary named {}", self.bin));
            self.write_state();
            self.say(
                "build_failed",
                format!("cargo built no binary named {}", self.bin),
                json!({}),
            );
            return false;
        };
        self.exe = Some(exe);
        self.build.state = "ok";
        self.write_state();
        self.say(
            "built",
            format!(
                "built in {:.1} s{}",
                outcome.took.as_secs_f64(),
                if warnings > 0 {
                    format!(
                        " ({warnings} {})",
                        if warnings == 1 { "warning" } else { "warnings" }
                    )
                } else {
                    String::new()
                }
            ),
            json!({"took_ms": outcome.took.as_millis() as u64, "warnings": warnings}),
        );
        true
    }

    /// Starts the latest build and retires the running one once the new one
    /// accepts (or first, without shared sockets).
    fn restart(&mut self, changed_at: Instant) {
        let Some(exe) = self.exe.clone() else {
            return;
        };
        let shared = self.shares_sockets();
        if !shared {
            if let Some(old) = self.current.take() {
                self.stop_now(old);
            }
        }
        match self.start(&exe, shared) {
            Ok(service) => {
                let pid = service.pid;
                if let Some(old) = self.current.replace(service) {
                    self.retire(old);
                }
                if self.served {
                    self.restarts += 1;
                }
                self.served = true;
                self.write_state();
                let took = changed_at.elapsed();
                let addrs: Vec<String> = self.listen.iter().map(|a| a.to_string()).collect();
                self.say(
                    "ready",
                    format!(
                        "ready in {:.1} s{}",
                        took.as_secs_f64(),
                        if addrs.is_empty() {
                            String::new()
                        } else {
                            format!(", on http://{}", addrs.join(", http://"))
                        }
                    ),
                    json!({"pid": pid, "took_ms": took.as_millis() as u64, "listen": addrs, "restarts": self.restarts}),
                );
            }
            Err(finding) => {
                let running = if self.current.is_some() {
                    "; the running build keeps serving"
                } else {
                    ""
                };
                self.say(
                    "start_failed",
                    format!("{}{running}", finding.title),
                    json!({"finding": finding.to_json()}),
                );
                self.report.push(finding);
                self.write_state();
            }
        }
    }

    fn shares_sockets(&self) -> bool {
        #[cfg(unix)]
        {
            !self.listeners.is_empty()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    fn start(&mut self, exe: &Path, shared: bool) -> Result<Service, Finding> {
        let state_dir = self.project.root.join(".giong");
        self.starts += 1;
        let mcp = mcp_socket(&state_dir, self.starts);
        let _ = fs::remove_file(&mcp);
        let mut cmd = Process::new(exe);
        cmd.args(&self.service_args)
            .current_dir(&self.project.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // .env, read again at each start: a change to it alone restarts.
        let env_file = dotenv::read(&self.project.root.join(".env"))
            .map(|(v, _)| v)
            .unwrap_or_default();
        let set =
            |key: &str| std::env::var_os(key).is_some() || env_file.iter().any(|v| v.key == key);
        for v in &env_file {
            if std::env::var_os(&v.key).is_none() {
                cmd.env(&v.key, &v.value);
            }
        }
        if let Some(n) = &self.workers {
            cmd.env("WORKERS", n);
        } else if !self.prod_like && !set("WORKERS") {
            cmd.env("WORKERS", "2");
        }
        if !set("RUST_BACKTRACE") {
            cmd.env("RUST_BACKTRACE", "1");
        }
        // The addresses giong dev listens on, which may differ from .env's
        // (--listen, or loopback for 0.0.0.0): the service must bind these.
        if !self.addresses.is_empty() {
            let text: Vec<String> = self.addresses.iter().map(|a| a.to_string()).collect();
            cmd.env("LISTEN", text.join(","));
        }
        let mcp_on = !set("GIONG_MCP");
        if mcp_on {
            cmd.env("GIONG_MCP", format!("unix:{}", mcp.display()));
        }
        #[cfg(unix)]
        let (child, ready) = if shared {
            let (ours, theirs) = std::os::unix::net::UnixStream::pair()
                .map_err(|e| start_failed(format!("a socket pair cannot be made: {e}")))?;
            let child = self.spawn_with_sockets(&mut cmd, &theirs)?;
            drop(theirs);
            (child, Some(ours))
        } else {
            let child = cmd
                .spawn()
                .map_err(|e| start_failed(format!("{} cannot be started: {e}", exe.display())))?;
            (child, None)
        };
        #[cfg(not(unix))]
        let child = {
            let _ = shared;
            cmd.spawn()
                .map_err(|e| start_failed(format!("{} cannot be started: {e}", exe.display())))?
        };
        let mut service = Service {
            pid: child.id(),
            child,
            mcp: if mcp_on { mcp } else { PathBuf::new() },
            tail: Arc::new(Mutex::new(VecDeque::new())),
            #[cfg(unix)]
            ready: None,
            #[cfg(unix)]
            binds: 0,
        };
        self.tee(&mut service);
        #[cfg(unix)]
        if let Some(ours) = ready {
            service.binds = self.wait_ready(&mut service, &ours)?;
            service.ready = Some(ours);
            return Ok(service);
        }
        // Without a ready signal: alive after a moment counts as started.
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            if let Ok(Some(status)) = service.child.try_wait() {
                // With --once, a program that finishes its work at once and
                // succeeds is a run like any other.
                if self.once && status.success() {
                    break;
                }
                return Err(self.exited_finding(&service, Some(status)));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(service)
    }

    /// Spawns with the listening sockets and the ready socket open in the
    /// child: they are made inheritable for the spawn only (spawns happen on
    /// this thread alone, so no other child can take them).
    #[cfg(unix)]
    fn spawn_with_sockets(
        &self,
        cmd: &mut Process,
        theirs: &std::os::unix::net::UnixStream,
    ) -> Result<Child, Finding> {
        use std::os::fd::AsRawFd;
        let listen: Vec<String> = self
            .listeners
            .iter()
            .zip(&self.listen)
            .map(|(s, a)| format!("{}={a}", s.as_raw_fd()))
            .collect();
        cmd.env("GIONG_DEV_LISTEN", listen.join(","))
            .env("GIONG_DEV_PARENT", std::process::id().to_string())
            .env("GIONG_DEV_READY", theirs.as_raw_fd().to_string());
        let theirs = socket2::SockRef::from(theirs);
        let inheritable = |on: bool| -> std::io::Result<()> {
            for s in &self.listeners {
                s.set_cloexec(!on)?;
            }
            theirs.set_cloexec(!on)
        };
        inheritable(true)
            .map_err(|e| start_failed(format!("the sockets cannot be handed down: {e}")))?;
        let spawned = cmd.spawn();
        let _ = inheritable(false);
        spawned.map_err(|e| start_failed(format!("the service cannot be started: {e}")))
    }

    /// Waits for the new build's first ready byte, then a moment more: a
    /// build that binds and then fails in its start-up must not replace
    /// the running one. Gives the number of binds seen.
    #[cfg(unix)]
    fn wait_ready(
        &self,
        service: &mut Service,
        ours: &std::os::unix::net::UnixStream,
    ) -> Result<usize, Finding> {
        let _ = ours.set_read_timeout(Some(Duration::from_millis(20)));
        let deadline = Instant::now() + self.ready_timeout;
        let mut settle_until: Option<Instant> = None;
        let mut binds = 0;
        let mut buf = [0u8; 64];
        loop {
            if let Ok(n) = (&*ours).read(&mut buf) {
                // 0: every copy of the other end is closed (it exited).
                binds += buf[..n].iter().filter(|b| **b == b'\n').count();
            }
            if binds > 0 && settle_until.is_none() {
                settle_until = Some(Instant::now() + SETTLE_AFTER_READY);
            }
            if let Ok(Some(status)) = service.child.try_wait() {
                if self.once && status.success() {
                    return Ok(binds);
                }
                return Err(self.exited_finding(service, Some(status)));
            }
            if settle_until.is_some_and(|t| Instant::now() >= t) {
                return Ok(binds);
            }
            if Instant::now() > deadline {
                let _ = service.child.kill();
                let _ = service.child.wait();
                let addrs: Vec<String> = self.listen.iter().map(|a| a.to_string()).collect();
                return Err(Finding::new(
                    Code::SERVICE_NOT_READY,
                    Severity::Error,
                    format!(
                        "the new build did not bind {} within {} s",
                        addrs.join(" or "),
                        self.ready_timeout.as_secs()
                    ),
                )
                .detail(tail_text(service))
                .fix(
                    "check that the service binds exactly the LISTEN address (giong dev hands \
                     that socket over), and that it starts within --ready-timeout",
                ));
            }
        }
    }

    /// Asks a build to stop taking connections from the shared queue, and
    /// waits until its listeners say they hold none (or a second passes): a
    /// connection it took after that would be closed by its shutdown.
    #[cfg(unix)]
    fn stop_accepting(service: &mut Service) {
        let Some(ready) = &service.ready else {
            return;
        };
        if (&*ready).write_all(b"s").is_err() {
            return;
        }
        let _ = ready.set_read_timeout(Some(Duration::from_millis(20)));
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut drained = 0;
        let mut last = Instant::now();
        let mut buf = [0u8; 64];
        while Instant::now() < deadline {
            match (&*ready).read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    service.binds += buf[..n].iter().filter(|b| **b == b'\n').count();
                    let d = buf[..n].iter().filter(|b| **b == b'd').count();
                    if d > 0 {
                        drained += d;
                        last = Instant::now();
                    }
                }
                Err(_) => {}
            }
            // Every bind answered, or quiet for a while after some did (a
            // listener that was dropped, or never accepted, holds nothing
            // and does not answer).
            if drained >= service.binds
                || (drained > 0 && last.elapsed() > Duration::from_millis(150))
            {
                return;
            }
            if let Ok(Some(_)) = service.child.try_wait() {
                return;
            }
        }
    }

    /// Copies the service's output to the terminal (unless `--json`) and to
    /// the log, keeping its last lines.
    fn tee(&self, service: &mut Service) {
        let streams: [(Option<Box<dyn Read + Send>>, bool); 2] = [
            (
                service
                    .child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn Read + Send>),
                false,
            ),
            (
                service
                    .child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn Read + Send>),
                true,
            ),
        ];
        for (stream, is_err) in streams {
            let Some(stream) = stream else { continue };
            let log = Arc::clone(&self.log);
            let tail = Arc::clone(&service.tail);
            let echo = !self.json;
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if !line.ends_with(b"\n") {
                        line.push(b'\n');
                    }
                    if let Ok(mut f) = log.lock() {
                        let _ = f.write_all(&line);
                    }
                    if echo {
                        let _ = if is_err {
                            std::io::stderr().lock().write_all(&line)
                        } else {
                            std::io::stdout().lock().write_all(&line)
                        };
                    }
                    if let Ok(mut t) = tail.lock() {
                        if t.len() == TAIL {
                            t.pop_front();
                        }
                        t.push_back(String::from_utf8_lossy(&line).trim_end().to_string());
                    }
                }
            });
        }
    }

    /// The old build gets SIGTERM and `--grace` to finish.
    #[cfg_attr(not(unix), allow(unused_mut))]
    fn retire(&mut self, mut old: Service) {
        #[cfg(unix)]
        Self::stop_accepting(&mut old);
        terminate(&old.child);
        let _ = fs::remove_file(&old.mcp);
        self.retiring.push(Retiring {
            child: old.child,
            kill_at: Instant::now() + self.grace,
        });
    }

    /// Stops a build and waits for it (no shared sockets: the port must be
    /// free before the next one binds it).
    fn stop_now(&mut self, mut old: Service) {
        terminate(&old.child);
        let until = Instant::now() + self.grace;
        loop {
            if let Ok(Some(_)) = old.child.try_wait() {
                break;
            }
            if Instant::now() > until {
                let _ = old.child.kill();
                let _ = old.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = fs::remove_file(&old.mcp);
    }

    /// Old builds that finished, or ran out of grace.
    fn reap(&mut self) {
        let now = Instant::now();
        self.retiring.retain_mut(|r| match r.child.try_wait() {
            Ok(Some(_)) | Err(_) => false,
            Ok(None) if now >= r.kill_at => {
                let _ = r.child.kill();
                let _ = r.child.wait();
                false
            }
            Ok(None) => true,
        });
    }

    /// The running build exited by itself.
    fn check_current(&mut self) {
        let exited = match self.current.as_mut().map(|s| s.child.try_wait()) {
            Some(Ok(Some(status))) => Some(status),
            _ => None,
        };
        if let Some(status) = exited {
            let service = self.current.take().expect("checked");
            self.service_ended(&service, Some(status));
            self.write_state();
        }
    }

    fn service_ended(&mut self, service: &Service, status: Option<ExitStatus>) {
        let _ = fs::remove_file(&service.mcp);
        if status.is_some_and(|s| s.success()) {
            let then = if self.once {
                ""
            } else {
                "; save a file to start it again"
            };
            self.say(
                "exited",
                format!("the service exited (status 0){then}"),
                json!({"pid": service.pid, "code": 0}),
            );
            return;
        }
        // Give the output threads a moment to take the last lines.
        std::thread::sleep(Duration::from_millis(50));
        let finding = self.exited_finding(service, status);
        self.say(
            "exited",
            if self.once {
                finding.title.clone()
            } else {
                format!("{}; fix it and save to start it again", finding.title)
            },
            json!({"pid": service.pid, "code": status.and_then(|s| s.code()), "finding": finding.to_json()}),
        );
        self.report.push(finding);
    }

    fn exited_finding(&self, service: &Service, status: Option<ExitStatus>) -> Finding {
        let how = match status {
            Some(s) => describe_status(s),
            None => "for an unknown reason".to_string(),
        };
        Finding::new(
            Code::SERVICE_EXITED,
            Severity::Error,
            format!("the service stopped {how}"),
        )
        .detail(tail_text(service))
        .fix(format!(
            "read its last lines above (all of them are in {}); a panic names the file and line",
            self.log_path.display()
        ))
    }

    fn build_json(&self) -> Value {
        json!({
            "state": if self.build.state.is_empty() { Value::Null } else { json!(self.build.state) },
            "started_at": self.build.started_at,
            "finished_at": self.build.finished_at,
            "took_ms": self.build.took_ms,
            "errors": self.build.errors,
            "warnings": self.build.warnings,
            "error": self.build.error,
        })
    }

    /// `.giong/dev.json`, replaced in one step (written aside, then renamed).
    fn write_state(&self) {
        let dir = self.project.root.join(".giong");
        let current = self.current.as_ref();
        let doc = json!({
            "pid": std::process::id(),
            "service_pid": current.map(|s| s.pid),
            "exe": self.exe.as_ref().map(|p| p.display().to_string()),
            "started_at": self.started_at,
            "mcp_socket": current
                .filter(|s| !s.mcp.as_os_str().is_empty())
                .map(|s| s.mcp.display().to_string()),
            "mcp_token": Value::Null,
            "listen": self.listen.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "log": self.log_path.display().to_string(),
            "restarts": self.restarts,
            "build": self.build_json(),
        });
        let tmp = dir.join(format!("dev.json.{}", std::process::id()));
        if fs::write(&tmp, format!("{doc:#}\n")).is_ok() {
            let _ = fs::rename(&tmp, dir.join("dev.json"));
        }
    }

    /// Serves the dashboard on a thread of its own. Ports in use are a
    /// warning: the service runs without the page.
    fn start_dashboard(&mut self, port: u16) {
        match crate::dashboard::spawn(self.project.root.clone(), port) {
            Ok(url) => {
                self.say(
                    "dashboard",
                    format!("dashboard on {url}"),
                    json!({ "url": url }),
                );
            }
            Err(e) => {
                let last = port.saturating_add(9);
                let (code, what, fix) = if e.kind() == std::io::ErrorKind::AddrInUse {
                    (
                        Code::DASHBOARD_PORT_IN_USE,
                        format!("ports {port}-{last} are in use: running without the dashboard"),
                        "give another port with --ui-port, or stop what holds them",
                    )
                } else {
                    (
                        Code::DASHBOARD_FAILED,
                        format!("the dashboard cannot listen on 127.0.0.1:{port}: {e}"),
                        "give another port with --ui-port, or run giong dashboard on its own",
                    )
                };
                self.say(
                    "warning",
                    format!("{code}: {what}"),
                    json!({ "code": code.to_string() }),
                );
                self.report
                    .push(Finding::new(code, Severity::Warning, what).fix(fix));
            }
        }
    }

    /// One event: a line for people on stderr, or a JSON object on stdout.
    fn say(&self, event: &str, message: String, fields: Value) {
        if self.json {
            let mut doc = json!({"event": event, "message": message, "time": unix_ms()});
            if let (Some(d), Value::Object(f)) = (doc.as_object_mut(), fields) {
                d.extend(f);
            }
            let _ = writeln!(std::io::stdout().lock(), "{doc}");
        } else {
            let _ = writeln!(std::io::stderr().lock(), "giong dev: {message}");
        }
    }

    /// Stops everything it started and removes its state file.
    fn shutdown(&mut self) {
        if let Some(s) = self.current.take() {
            self.stop_now(s);
        }
        for mut r in self.retiring.drain(..) {
            let _ = r.child.kill();
            let _ = r.child.wait();
        }
        let _ = fs::remove_file(self.project.root.join(".giong").join("dev.json"));
        // The folder of short socket paths, when the project's was too long.
        #[cfg(unix)]
        let _ = fs::remove_dir(short_socket_dir());
    }
}

/// Where the giong-mcp socket of the `n`th start goes: `.giong/` in the
/// project, unless that path is longer than a Unix socket address holds
/// (104 bytes on macOS, 108 on Linux, counting the final NUL); then a folder
/// of this process's own under /tmp, readable by this user only.
fn mcp_socket(state_dir: &Path, n: u64) -> PathBuf {
    let local = state_dir.join(format!("mcp-{}-{n}.sock", std::process::id()));
    #[cfg(unix)]
    if local.as_os_str().len() > MAX_SOCKET_PATH {
        let dir = short_socket_dir();
        if private_dir(&dir, state_dir) {
            return dir.join(format!("mcp-{n}.sock"));
        }
    }
    local
}

/// The longest socket path that fits everywhere (macOS: 103 bytes and a NUL).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

#[cfg(unix)]
fn short_socket_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/giong-dev-{}", std::process::id()))
}

/// Makes `dir` with mode 0700, or accepts it if it already is a folder of the
/// user who owns `owner_of` with that mode (another user could have made it
/// first, to read the service's state).
#[cfg(unix)]
fn private_dir(dir: &Path, owner_of: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if fs::DirBuilder::new().mode(0o700).create(dir).is_ok() {
        return true;
    }
    let (Ok(d), Ok(o)) = (fs::symlink_metadata(dir), fs::metadata(owner_of)) else {
        return false;
    };
    d.is_dir() && d.uid() == o.uid() && d.permissions().mode() & 0o077 == 0
}

fn start_failed(what: String) -> Finding {
    Finding::new(Code::SERVICE_EXITED, Severity::Error, what)
        .fix("check that the binary was built and can run on this machine")
}

fn watch_limit() -> Finding {
    Finding::new(
        Code::WATCH_LIMIT,
        Severity::Warning,
        format!("more than {} files to watch", watch::MAX_FILES),
    )
    .fix("move large data out of the project, or into a directory named target or node_modules")
}

fn tail_text(service: &Service) -> String {
    service
        .tail
        .lock()
        .map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

#[cfg(unix)]
fn describe_status(s: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("with status {c}"),
        (None, Some(9)) => "killed (SIGKILL; out of memory?)".into(),
        (None, Some(6)) => "on an abort (SIGABRT)".into(),
        (None, Some(11)) => "on a segmentation fault (SIGSEGV)".into(),
        (None, Some(sig)) => format!("on signal {sig}"),
        (None, None) => "for an unknown reason".into(),
    }
}

#[cfg(not(unix))]
fn describe_status(s: ExitStatus) -> String {
    match s.code() {
        Some(c) => format!("with status {c}"),
        None => "for an unknown reason".into(),
    }
}

/// SIGTERM on Unix (the service shuts down gracefully when it asked for
/// `shutdown_on_signals`); a kill elsewhere.
fn terminate(child: &Child) {
    #[cfg(unix)]
    {
        let _ = Process::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        // `Child::kill` needs `&mut`; taskkill does the same by pid.
        let _ = Process::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Whether a process is alive (`kill -0`).
pub(crate) fn alive(pid: u64) -> bool {
    if cfg!(unix) {
        Process::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    } else {
        false
    }
}

/// Refuses to start when another giong dev serves the project; stops the
/// service a killed giong dev left behind.
fn refuse_if_running(state_dir: &Path) -> Result<(), Failure> {
    let Ok(text) = fs::read_to_string(state_dir.join("dev.json")) else {
        return Ok(());
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Ok(());
    };
    if let Some(pid) = v["pid"].as_u64() {
        if pid != u64::from(std::process::id()) && alive(pid) {
            return Err(Failure::new(
                Code::DEV_RUNNING,
                format!("giong dev already serves this project (pid {pid})"),
                format!("use that one, or stop it first: kill {pid}"),
            ));
        }
    }
    // Its giong dev is gone; the service may not be (a giong older than 1.5,
    // or not Linux). Only stop it if it is the binary dev.json names.
    if let (Some(pid), Some(exe)) = (v["service_pid"].as_u64(), v["exe"].as_str()) {
        let ours = fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|p| p == Path::new(exe));
        if ours && alive(pid) {
            let _ = Process::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
    let _ = fs::remove_file(state_dir.join("dev.json"));
    Ok(())
}

/// The package and binary to run: `--bin`, else the package of the current
/// directory (or the only one with a binary), its `default-run` or its only
/// binary.
fn pick_binary(project: &Project, bin: Option<&str>) -> Result<(String, String), Failure> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| project.root.clone());
    let mut candidates: Vec<(String, String, Option<String>, PathBuf)> = Vec::new();
    for p in project.packages() {
        let name = p["name"].as_str().unwrap_or("").to_string();
        let dir = Path::new(p["manifest_path"].as_str().unwrap_or(""))
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let default_run = p["default_run"].as_str().map(String::from);
        for t in p["targets"].as_array().into_iter().flatten() {
            let is_bin = t["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k == "bin"));
            if is_bin {
                candidates.push((
                    name.clone(),
                    t["name"].as_str().unwrap_or("").to_string(),
                    default_run.clone(),
                    dir.clone(),
                ));
            }
        }
    }
    let names = |c: &[&(String, String, Option<String>, PathBuf)]| {
        c.iter()
            .map(|c| c.1.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(bin) = bin {
        return candidates
            .iter()
            .find(|c| c.1 == bin)
            .map(|c| (c.0.clone(), c.1.clone()))
            .ok_or_else(|| {
                Failure::new(
                    Code::USAGE,
                    format!("the project has no binary named {bin}"),
                    format!(
                        "its binaries: {}",
                        names(&candidates.iter().collect::<Vec<_>>())
                    ),
                )
            });
    }
    // The package the current directory is in, deepest first.
    let mut here: Vec<&(String, String, Option<String>, PathBuf)> = candidates
        .iter()
        .filter(|c| cwd.starts_with(&c.3))
        .collect();
    if let Some(depth) = here.iter().map(|c| c.3.components().count()).max() {
        here.retain(|c| c.3.components().count() == depth);
    }
    let pool: Vec<&(String, String, Option<String>, PathBuf)> = if here.is_empty() {
        candidates.iter().collect()
    } else {
        here
    };
    let packages: std::collections::BTreeSet<&str> = pool.iter().map(|c| c.0.as_str()).collect();
    if pool.is_empty() {
        return Err(Failure::new(
            Code::USAGE,
            "the project has no binary to run",
            "giong dev runs a service: add src/main.rs, or start one with giong new <name>",
        ));
    }
    if packages.len() == 1 {
        if pool.len() == 1 {
            return Ok((pool[0].0.clone(), pool[0].1.clone()));
        }
        if let Some(d) = &pool[0].2 {
            if let Some(c) = pool.iter().find(|c| &c.1 == d) {
                return Ok((c.0.clone(), c.1.clone()));
            }
        }
    }
    Err(Failure::new(
        Code::USAGE,
        "the project has several binaries and giong dev runs one",
        format!(
            "name it: giong dev --bin <name> (one of {}), or run giong dev in its package",
            names(&pool)
        ),
    ))
}

/// `--listen`, else `LISTEN` from the environment, else from `.env`.
fn listen_addresses(
    flag: Option<&str>,
    env_file: &[dotenv::Var],
) -> Result<Vec<SocketAddr>, Failure> {
    let (text, from) = match flag {
        Some(t) => (t.to_string(), "--listen"),
        None => match std::env::var("LISTEN").ok().filter(|v| !v.is_empty()) {
            Some(t) => (t, "LISTEN"),
            None => match env_file.iter().find(|v| v.key == "LISTEN") {
                Some(v) if !v.value.is_empty() => (v.value.clone(), ".env's LISTEN"),
                _ => return Ok(Vec::new()),
            },
        },
    };
    let mut out = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let addr: SocketAddr = part.parse().map_err(|_| {
            Failure::new(
                Code::HOST_NAME_BIND,
                format!("{from} is {part:?}, which is not an IP address and port"),
                "write the address as numbers, such as 127.0.0.1:8080 (localhost can mean \
                 127.0.0.1 or ::1, so giong dev cannot tell which socket the service binds)",
            )
        })?;
        if addr.port() == 0 {
            return Err(Failure::new(
                Code::USAGE,
                format!("{from} is {part}: port 0 means a new port at each start"),
                "give a fixed port, such as 127.0.0.1:8080",
            ));
        }
        out.push(addr);
    }
    Ok(out)
}

/// 0.0.0.0 and [::] (every network interface) become 127.0.0.1 and ::1 of
/// the same port unless `lan`: a template's LISTEN serves a container in
/// production, while on a laptop it would put the service under
/// development, its giong-mcp and its debug routes on the café's network.
/// Each address that would have been reachable from other machines gives a
/// warning: what giong dev did, or with `lan`, that it is reachable.
fn loopback_unless_lan(listen: Vec<SocketAddr>, lan: bool) -> (Vec<SocketAddr>, Vec<Finding>) {
    let mut findings = Vec::new();
    let listen = listen
        .into_iter()
        .map(|addr| {
            if !addr.ip().is_unspecified() {
                return addr;
            }
            let local = SocketAddr::new(
                match addr {
                    SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                },
                addr.port(),
            );
            findings.push(if lan {
                Finding::new(
                    Code::LISTENS_ON_NETWORK,
                    Severity::Warning,
                    format!("the service listens on {addr}: other machines on the network can reach it (--lan)"),
                )
                .detail("its routes, its debug pages and its giong-mcp answer anyone who can reach this machine")
                .fix(format!("drop --lan when you do not need another device: giong dev then serves it on {local}"))
            } else {
                Finding::new(
                    Code::LISTENS_ON_NETWORK,
                    Severity::Warning,
                    format!("LISTEN is {addr}, which other machines could reach: giong dev serves it on {local}"),
                )
                .detail("the service gets LISTEN={local}; 0.0.0.0 and [::] are for a container in production")
                .fix("pass --lan to keep it reachable from other devices (a phone on the same network), \
                      or set LISTEN to 127.0.0.1:<port> in .env")
            });
            if lan {
                addr
            } else {
                local
            }
        })
        .collect();
    (listen, findings)
}

/// A listening socket for `addr`, as the runtime makes one.
#[cfg(unix)]
fn listen_on(addr: SocketAddr) -> Result<socket2::Socket, Failure> {
    use socket2::{Domain, Protocol, Socket, Type};
    let in_use = |e: std::io::Error| {
        Failure::new(
            Code::PORT_IN_USE,
            format!("{addr} cannot be bound: {e}"),
            if e.kind() == std::io::ErrorKind::AddrInUse {
                "another program listens there (a service started by hand?): stop it, or set \
                 another LISTEN in .env"
            } else {
                "check the address: it must be one of this machine's"
            },
        )
    };
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(in_use)?;
    sock.set_reuse_address(true).map_err(in_use)?;
    if addr.is_ipv6() {
        sock.set_only_v6(true).map_err(in_use)?;
    }
    sock.bind(&addr.into()).map_err(in_use)?;
    sock.listen(1024).map_err(in_use)?;
    Ok(sock)
}

/// The `dev` features to turn on for `package`'s build: `giong/dev` when it
/// depends on giong itself (by its own name or a rename). A package that
/// reaches giong only through another crate gets that crate's feature from
/// it (giong-web's `dev` turns on giong's).
#[cfg(unix)]
fn dev_features(metadata: &Value, package: &str) -> Vec<String> {
    let Some(p) = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == package)
    else {
        return Vec::new();
    };
    p["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["name"] == "giong" && d["kind"].is_null())
        .map(|d| d["rename"].as_str().unwrap_or("giong").to_string() + "/dev")
        .take(1)
        .collect()
}

/// Whether the project's giong takes sockets from giong dev (1.5 and newer,
/// or a giong from a path, such as a checkout of the repository).
#[cfg(unix)]
fn giong_takes_sockets(project: &Project) -> bool {
    let Ok(lock) = fs::read_to_string(project.root.join("Cargo.lock")) else {
        // No lock file yet: the build just made one, or will. Assume a
        // current giong.
        return true;
    };
    lock_giong_version(&lock).map_or(true, |(v, from_path)| {
        from_path || v >= crate::host::Version(1, 5, 0)
    })
}

/// giong's version in a Cargo.lock, and whether it comes from a path (no
/// `source`).
fn lock_giong_version(lock: &str) -> Option<(crate::host::Version, bool)> {
    for block in lock.split("[[package]]") {
        let mut name = None;
        let mut version = None;
        let mut source = false;
        for line in block.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("name = ") {
                name = Some(v.trim_matches('"'));
            } else if let Some(v) = line.strip_prefix("version = ") {
                version = crate::host::parse_version(v.trim_matches('"'));
            } else if line.starts_with("source = ") {
                source = true;
            }
        }
        if name == Some("giong") {
            return version.map(|v| (v, !source));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Version;

    #[cfg(unix)]
    #[test]
    fn a_socket_path_too_long_moves_to_a_private_folder() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("giong-dev-sock-{}", std::process::id()));
        let short = base.join(".giong");
        fs::create_dir_all(&short).unwrap();
        assert_eq!(mcp_socket(&short, 1).parent(), Some(short.as_path()));
        let long = base.join("a".repeat(120)).join(".giong");
        fs::create_dir_all(&long).unwrap();
        let moved = mcp_socket(&long, 2);
        assert_eq!(moved, short_socket_dir().join("mcp-2.sock"));
        assert!(moved.as_os_str().len() <= MAX_SOCKET_PATH);
        let mode = fs::metadata(short_socket_dir())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        // Asked again (the next start), the folder is accepted as it is.
        assert_eq!(mcp_socket(&long, 3), short_socket_dir().join("mcp-3.sock"));
        let _ = fs::remove_dir(short_socket_dir());
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_release_build_turns_on_giongs_dev_feature() {
        let meta = json!({"packages": [
            {"name": "svc", "dependencies": [
                {"name": "serde", "rename": null, "kind": null},
                {"name": "giong", "rename": null, "kind": null},
                {"name": "giong", "rename": null, "kind": "dev"},
            ]},
            {"name": "renamed", "dependencies": [{"name": "giong", "rename": "rt", "kind": null}]},
            {"name": "web-only", "dependencies": [{"name": "giong-web", "rename": null, "kind": null}]},
        ]});
        assert_eq!(dev_features(&meta, "svc"), ["giong/dev"]);
        assert_eq!(dev_features(&meta, "renamed"), ["rt/dev"]);
        assert!(dev_features(&meta, "web-only").is_empty());
        assert!(dev_features(&meta, "nope").is_empty());
    }

    #[test]
    fn every_interface_becomes_loopback_unless_lan() {
        let addrs: Vec<SocketAddr> = vec![
            "0.0.0.0:8080".parse().unwrap(),
            "[::]:9000".parse().unwrap(),
            "127.0.0.1:7000".parse().unwrap(),
            "192.168.1.5:6000".parse().unwrap(),
        ];
        let (local, findings) = loopback_unless_lan(addrs.clone(), false);
        assert_eq!(
            local,
            [
                "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
                "[::1]:9000".parse().unwrap(),
                "127.0.0.1:7000".parse().unwrap(),
                "192.168.1.5:6000".parse().unwrap(),
            ]
        );
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|f| f.code == Code::LISTENS_ON_NETWORK
            && f.severity == Severity::Warning
            && f.fix.contains("--lan")));
        let (kept, findings) = loopback_unless_lan(addrs.clone(), true);
        assert_eq!(kept, addrs);
        assert_eq!(findings.len(), 2);
        assert!(findings[0].title.contains("other machines"));
    }

    #[test]
    fn listen_comes_from_the_flag_then_the_environment_then_env_file() {
        let env_file = vec![dotenv::Var {
            key: "LISTEN".into(),
            value: "127.0.0.1:8080".into(),
            line: 1,
        }];
        assert_eq!(
            listen_addresses(Some("127.0.0.1:1, [::1]:2"), &env_file).unwrap(),
            ["127.0.0.1:1".parse().unwrap(), "[::1]:2".parse().unwrap()]
        );
        if std::env::var_os("LISTEN").is_none() {
            assert_eq!(
                listen_addresses(None, &env_file).unwrap(),
                ["127.0.0.1:8080".parse::<SocketAddr>().unwrap()]
            );
            assert!(listen_addresses(None, &[]).unwrap().is_empty());
        }
        let e = listen_addresses(Some("localhost:8080"), &[]).unwrap_err();
        assert_eq!(e.code, Code::HOST_NAME_BIND);
        assert!(listen_addresses(Some("127.0.0.1:0"), &[]).is_err());
    }

    #[test]
    fn the_lock_file_says_which_giong() {
        let lock = "version = 4\n\n[[package]]\nname = \"giong\"\nversion = \"1.4.2\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"x\"\nversion = \"0.1.0\"\n";
        assert_eq!(lock_giong_version(lock), Some((Version(1, 4, 2), false)));
        let path = "[[package]]\nname = \"giong\"\nversion = \"1.3.0\"\ndependencies = []\n";
        assert_eq!(lock_giong_version(path), Some((Version(1, 3, 0), true)));
        assert_eq!(lock_giong_version("[[package]]\nname = \"a\"\n"), None);
    }
}
