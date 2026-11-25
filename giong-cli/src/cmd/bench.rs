//! `giong bench`: load at a fixed rate, latency without coordinated
//! omission, and `--against <ref>`: the working tree and an older commit
//! built in release mode and measured in alternating rounds, raw rounds in a
//! CSV file.
//!
//! The service is always a release build with every core (a `WORKERS` from
//! `.env`, which `giong dev` uses to keep a laptop cool, is ignored), started
//! on a free loopback port through `LISTEN`, with giong-mcp off. A URL
//! instead measures a service that is already running.

use std::fmt::Write as _;
use std::io::IsTerminal;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::Command as Cmd;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::dotenv;
use crate::loadgen::{self, Outcome, Plan, Request, MAX_CONNS};
use crate::project::{cargo, cargo_missing, find_manifest_dir, first_error, Project};
use crate::report::{Failure, Finding, Report, Severity};

pub const COMMAND: Cmd = Cmd {
    name: "bench",
    summary: "measure the service: latency at a fixed rate, or a change against an older commit",
    usage: "giong bench [<url>] [--rate <n>|auto|max] [--conns 50] [--duration 10] [--warmup 2]\n\
            giong bench --against <git ref> [--rounds 5] [--max-regression <percent>]\n\n\
            Without a URL, builds the project in release mode, starts it on a free port \
            (LISTEN), and measures it; with --against, also builds <ref> in a git worktree and \
            alternates the two builds round by round, so a slower machine affects both. \
            Latency counts from when each request was due, so a stall is charged for every \
            request that waited behind it. --rate auto (the default) first finds the most \
            the service answers and then measures at half of it; --rate max measures \
            throughput. Every round goes to .giong/bench/<time>.csv.",
    flags: &[
        Flag {
            name: "rate",
            kind: Kind::Value,
            help: "requests per second in total, auto (half of the most the service answers, found first), or max (as fast as it answers; 0 means max too)",
        },
        Flag {
            name: "conns",
            kind: Kind::Value,
            help: "connections, one request at a time on each (default 50)",
        },
        Flag {
            name: "duration",
            kind: Kind::Value,
            help: "seconds measured per round (default 10)",
        },
        Flag {
            name: "warmup",
            kind: Kind::Value,
            help: "seconds of load before each round is measured (default 2)",
        },
        Flag {
            name: "rounds",
            kind: Kind::Value,
            help: "rounds per build (default 1, or 5 with --against)",
        },
        Flag {
            name: "against",
            kind: Kind::Value,
            help: "a git commit, branch or tag to build and measure next to the working tree",
        },
        Flag {
            name: "max-regression",
            kind: Kind::Value,
            help: "with --against: an error (exit 1) when the working tree is slower in every round by more than this many percent",
        },
        Flag {
            name: "path",
            kind: Kind::Value,
            help: "the path to request, or a weighted mix: /users@9,/health@1 (default: the URL's path, else /)",
        },
        Flag {
            name: "method",
            kind: Kind::Value,
            help: "the HTTP method (default GET, or POST with --data)",
        },
        Flag {
            name: "data",
            kind: Kind::Value,
            help: "the request body; a body starting with { or [ is sent as application/json",
        },
        Flag {
            name: "header",
            kind: Kind::Value,
            help: "one more request header, as 'Name: value'",
        },
        Flag {
            name: "timeout",
            kind: Kind::Value,
            help: "seconds to wait for an answer before the request counts as failed (default 10)",
        },
        Flag {
            name: "bin",
            kind: Kind::Value,
            help: "the binary to run when the project builds more than one",
        },
        Flag {
            name: "workers",
            kind: Kind::Value,
            help: "WORKERS for the service (default: unset, one worker per core)",
        },
        Flag {
            name: "csv",
            kind: Kind::Value,
            help: "where to write the rounds (default .giong/bench/<time>.csv in a project)",
        },
    ],
    max_words: 1,
    run,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rate {
    Auto,
    Max,
    Fixed(f64),
}

pub fn parse_rate(v: Option<&str>) -> Result<Rate, Failure> {
    match v {
        None | Some("auto") => Ok(Rate::Auto),
        Some("max") | Some("0") => Ok(Rate::Max),
        Some(v) => v
            .parse::<f64>()
            .ok()
            .filter(|r| r.is_finite() && *r > 0.0)
            .map(Rate::Fixed)
            .ok_or_else(|| {
                Failure::new(
                    Code::USAGE,
                    format!("--rate {v:?} is not a rate"),
                    "give requests per second (--rate 5000), auto or max",
                )
            }),
    }
}

/// `/a@9,/b@1` as paths with weights.
pub fn parse_mix(v: &str) -> Result<Vec<(String, u32)>, Failure> {
    let bad = |what: String| {
        Failure::new(
            Code::USAGE,
            what,
            "write --path /users or a weighted mix: --path /users@9,/health@1",
        )
    };
    let mut out = Vec::new();
    for item in v.split(',') {
        let (path, weight) = match item.rsplit_once('@') {
            Some((p, w)) => (
                p,
                w.parse::<u32>()
                    .ok()
                    .filter(|w| (1..=100).contains(w))
                    .ok_or_else(|| {
                        bad(format!("the weight of {p:?} is not a number from 1 to 100"))
                    })?,
            ),
            None => (item, 1),
        };
        if !path.starts_with('/') || path.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(bad(format!(
                "{path:?} is not a path (it starts with / and has no spaces)"
            )));
        }
        out.push((path.to_string(), weight));
    }
    Ok(out)
}

/// The requests in the order each connection cycles through them: weights
/// spread out (`a@2,b@1` is a, b, a), not in runs.
pub fn cycle(mix: &[(String, u32)]) -> Vec<usize> {
    let total: u32 = mix.iter().map(|m| m.1).sum();
    let mut credit = vec![0i64; mix.len()];
    let mut out = Vec::with_capacity(total as usize);
    for _ in 0..total {
        for (c, m) in credit.iter_mut().zip(mix) {
            *c += i64::from(m.1);
        }
        let (best, _) = credit
            .iter()
            .enumerate()
            .max_by_key(|(i, c)| (**c, std::cmp::Reverse(*i)))
            .expect("a mix is never empty");
        credit[best] -= i64::from(total);
        out.push(best);
    }
    out
}

/// `http://host:port/path` as the address to connect to, the Host header and
/// the path.
pub fn parse_url(url: &str) -> Result<(String, String), Failure> {
    let bad = |what: String| {
        Failure::new(
            Code::BENCH_TARGET,
            what,
            "give a plain HTTP URL such as http://127.0.0.1:8080/",
        )
    };
    if url.starts_with("https://") {
        return Err(Failure::new(
            Code::BENCH_TARGET,
            "giong bench measures plain HTTP, not HTTPS",
            "start the service without TLS_CERT and TLS_KEY for the benchmark, and give its http:// URL",
        ));
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| bad(format!("{url:?} is not an http:// URL")))?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(bad(format!("{url:?} has no host (or carries a password)")));
    }
    Ok((authority.to_string(), path))
}

fn resolve(authority: &str) -> Result<SocketAddr, Failure> {
    let with_port = if authority.ends_with(']') || !authority.contains(':') {
        format!("{authority}:80")
    } else {
        authority.to_string()
    };
    with_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| {
            Failure::new(
                Code::BENCH_TARGET,
                format!("{authority} does not resolve to an address"),
                "check the host name in the URL",
            )
        })
}

/// What one round measures.
#[derive(Clone, Debug)]
struct Load {
    requests: Vec<Request>,
    conns: usize,
    warmup: Duration,
    duration: Duration,
    timeout: Duration,
}

impl Load {
    fn plan(&self, addr: SocketAddr, rate: Option<f64>) -> Plan {
        Plan {
            addr,
            requests: self.requests.clone(),
            conns: self.conns,
            rate,
            warmup: self.warmup,
            duration: self.duration,
            timeout: self.timeout,
        }
    }
}

/// One build of the service.
#[derive(Clone, Debug)]
struct Build {
    /// `base` or `change` with --against, else `service`.
    role: &'static str,
    label: String,
    binary: PathBuf,
    /// Where the service runs (the worktree for the older commit).
    dir: PathBuf,
}

struct Running {
    child: Child,
    addr: SocketAddr,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> Result<u16, Failure> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| {
            Failure::new(
                Code::BENCH_TARGET,
                format!("no free loopback port: {e}"),
                "check that 127.0.0.1 can be bound",
            )
        })
}

/// The last lines of the service's log, for a failure message.
fn log_tail(log: &Path) -> String {
    let text = crate::state::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(8).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut s = String::from("; its log ends with:");
    for l in lines.into_iter().rev() {
        let _ = write!(s, "\n  {l}");
    }
    s
}

/// The environment the service gets: `.env` under the process environment,
/// a free `LISTEN`, no giong-mcp, and `WORKERS` only from the process
/// environment or `--workers`. Returns the variables and notes for the
/// report.
fn service_env(
    root: &Path,
    workers: Option<&str>,
) -> (Vec<(String, String)>, Vec<&'static str>, Vec<String>) {
    let mut set = Vec::new();
    let mut notes = Vec::new();
    if let Some((vars, _)) = crate::state::read_to_string(&root.join(".env"))
        .ok()
        .map(|t| dotenv::parse(&t))
    {
        for v in vars {
            if v.key == "WORKERS" {
                if std::env::var_os("WORKERS").is_none() && workers.is_none() {
                    notes.push("WORKERS from .env is ignored: the service runs one worker per core (--workers N to choose)".to_string());
                }
                continue;
            }
            if std::env::var_os(&v.key).is_none() {
                set.retain(|(k, _): &(String, String)| *k != v.key);
                set.push((v.key, v.value));
            }
        }
    }
    if let Some(w) = workers {
        set.push(("WORKERS".into(), w.to_string()));
    }
    let remove = vec!["GIONG_MCP", "GIONG_MCP_TOKEN", "GIONG_DEV_LISTEN", "LISTEN"];
    (set, remove, notes)
}

fn start(
    build: &Build,
    env: &(Vec<(String, String)>, Vec<&'static str>, Vec<String>),
    log: &Path,
    banner: &str,
) -> Result<Running, Failure> {
    let port = free_port()?;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut file = crate::state::append(log).map_err(|e| {
        Failure::new(
            Code::BENCH_TARGET,
            format!("cannot write {}: {e}", log.display()),
            "check that .giong/ is writable",
        )
    })?;
    {
        use std::io::Write;
        let _ = writeln!(file, "== {banner}: {} on {addr}", build.binary.display());
    }
    let err = file.try_clone().map_err(|e| {
        Failure::new(
            Code::BENCH_TARGET,
            format!("cannot write {}: {e}", log.display()),
            "check that .giong/ is writable",
        )
    })?;
    let mut cmd = Command::new(&build.binary);
    cmd.current_dir(&build.dir)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err);
    for k in &env.1 {
        cmd.env_remove(k);
    }
    for (k, v) in &env.0 {
        cmd.env(k, v);
    }
    cmd.env("LISTEN", addr.to_string());
    let child = cmd.spawn().map_err(|e| {
        Failure::new(
            Code::BENCH_TARGET,
            format!("cannot start {}: {e}", build.binary.display()),
            "check that the build finished and the file is executable",
        )
    })?;
    let mut running = Running { child, addr };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(status)) = running.child.try_wait() {
            return Err(Failure::new(
                Code::BENCH_TARGET,
                format!(
                    "{} exited ({status}) before it listened on {addr}{}",
                    build.label,
                    log_tail(log)
                ),
                "read .giong/bench/service.log; giong env check finds missing settings",
            ));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(running);
        }
        if Instant::now() > deadline {
            return Err(Failure::new(
                Code::BENCH_TARGET,
                format!("{} did not listen on {addr} (from LISTEN) within 60 s{}", build.label, log_tail(log)),
                "make the service listen on the address in LISTEN (giong new services do), or start it yourself and give its URL: giong bench http://127.0.0.1:8080/",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Builds the package around `dir` in release mode and returns its binary.
fn build(
    dir: &Path,
    target_dir: Option<&Path>,
    bin: Option<&str>,
    quiet: bool,
) -> Result<PathBuf, Failure> {
    let extra: Vec<String> = target_dir
        .map(|t| vec!["--target-dir".to_string(), t.display().to_string()])
        .unwrap_or_default();
    build_release(dir, "build", &extra, bin, quiet, "giong bench")
}

/// `cargo <subcommand> --release` (`build`, or `zigbuild` to cross-build)
/// with `extra` arguments in `dir`; returns the one binary it built.
/// `command` names the giong command in messages.
pub(crate) fn build_release(
    dir: &Path,
    subcommand: &str,
    extra: &[String],
    bin: Option<&str>,
    quiet: bool,
    command: &str,
) -> Result<PathBuf, Failure> {
    let mut cmd = cargo();
    cmd.args([
        subcommand,
        "--release",
        "--message-format=json-render-diagnostics",
    ])
    .args(extra)
    .current_dir(dir)
    .stdout(Stdio::piped())
    .stderr(if quiet {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    if quiet {
        // Plain text, so the first error line can be found and quoted
        // (CARGO_TERM_COLOR=always would colour it).
        cmd.arg("--color=never");
    }
    if let Some(b) = bin {
        cmd.args(["--bin", b]);
    }
    let out = cmd.output().map_err(cargo_missing)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = if err.trim().is_empty() {
            "see the output above".to_string()
        } else {
            first_error(&err)
        };
        return Err(Failure::new(
            Code::CARGO_FAILED,
            format!(
                "cargo {subcommand} --release failed in {}: {first}",
                dir.display()
            ),
            format!("fix the build (cargo {subcommand} --release shows the errors)"),
        ));
    }
    let mut bins: Vec<(String, PathBuf)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(m) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if m["reason"] != "compiler-artifact" {
            continue;
        }
        let is_bin = m["target"]["kind"]
            .as_array()
            .is_some_and(|k| k.iter().any(|x| x == "bin"));
        if let (true, Some(exe)) = (is_bin, m["executable"].as_str()) {
            let name = m["target"]["name"].as_str().unwrap_or("").to_string();
            bins.push((name, PathBuf::from(exe)));
        }
    }
    match bins.len() {
        0 => Err(Failure::new(
            Code::BENCH_TARGET,
            format!("the project in {} builds no binary", dir.display()),
            format!("run {command} in the service's package"),
        )),
        1 => Ok(bins.remove(0).1),
        _ => Err(Failure::new(
            Code::USAGE,
            format!(
                "the project builds {} binaries: {}",
                bins.len(),
                bins.iter()
                    .map(|b| b.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!("choose one: {command} --bin {}", bins[0].0),
        )),
    }
}

/// Copies a binary into `.giong/bench/bin/<role>/` so a later build does not
/// replace it during the rounds.
fn keep(binary: &Path, root: &Path, role: &str) -> Result<PathBuf, Failure> {
    let name = binary.file_name().unwrap_or_default();
    let mut to = root
        .join(".giong")
        .join("bench")
        .join("bin")
        .join(role)
        .join(name);
    // Never through a link: .giong can come from a cloned repository.
    let mut copy = || -> std::io::Result<()> {
        to = crate::state::ensure_sub(root, &["bench", "bin", role])?.join(name);
        crate::state::replace(&to, &std::fs::read(binary)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    };
    copy().map_err(|e| {
        Failure::new(
            Code::BENCH_TARGET,
            format!("cannot copy {} to {}: {e}", binary.display(), to.display()),
            "check that .giong/ is writable",
        )
    })?;
    Ok(to)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git could not be started: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// A git worktree of the older commit, removed when dropped.
struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let path = self.path.display().to_string();
        let _ = git(&self.repo, &["worktree", "remove", "--force", &path]);
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = git(&self.repo, &["worktree", "prune"]);
    }
}

fn against(cwd: &Path, reference: &str) -> Result<(Worktree, PathBuf, String, String), Failure> {
    let fail = |what: String, fix: &str| Failure::new(Code::BENCH_AGAINST, what, fix);
    let top = git(cwd, &["rev-parse", "--show-toplevel"]).map_err(|e| {
        fail(
            format!("the project is not in a git repository: {e}"),
            "run git init and commit, or measure without --against",
        )
    })?;
    let top = PathBuf::from(top);
    let sha = git(cwd, &["rev-parse", "--verify", "--quiet", &format!("{reference}^{{commit}}")])
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(format!("{reference:?} does not name a commit"), "check it with git rev-parse --verify <ref>; fetch a remote branch first (git fetch origin main, then --against origin/main)"))?;
    let short = &sha[..sha.len().min(10)];
    // One path per repository and commit, so the older build's own files
    // keep their paths and cargo rebuilds only what changed since last time.
    let repo_id = &crate::sha256::hex(top.display().to_string().as_bytes())[..8];
    // Under ~/.giong, never the temporary directory: cargo reads
    // `.cargo/config.toml` from every folder above the one it builds in, and
    // any user can make /tmp/.cargo/config.toml (its rustc-wrapper would run
    // as this user).
    let path = crate::versions::giong_home()
        .map(|home| home.join("bench").join(format!("{repo_id}-{short}")))
        .ok_or_else(|| {
            fail(
                "there is no home folder to build the older commit in".to_string(),
                "set GIONG_HOME to a folder of your own",
            )
        })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            fail(
                format!("{} could not be made: {e}", parent.display()),
                "check that you may write to it, or set GIONG_HOME to a folder of your own",
            )
        })?;
    }
    let _ = std::fs::remove_dir_all(&path);
    let p = path.display().to_string();
    git(&top, &["worktree", "add", "--detach", "--force", &p, &sha]).map_err(|e| {
        fail(
            format!("git worktree add failed: {e}"),
            "run git worktree prune, then try again",
        )
    })?;
    let wt = Worktree {
        repo: top.clone(),
        path,
    };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let rel = canon(cwd)
        .strip_prefix(canon(&top))
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let dir = wt.path.join(rel);
    let head = git(cwd, &["rev-parse", "HEAD"]).unwrap_or_default();
    let dirty = !git(cwd, &["status", "--porcelain", "--untracked-files=no"])
        .unwrap_or_default()
        .is_empty();
    let change = if dirty {
        "working tree".to_string()
    } else {
        format!("HEAD {}", &head[..head.len().min(10)])
    };
    let base = format!("{reference} {short}");
    Ok((wt, dir, base, change))
}

/// A number taken from one round's outcome.
type Metric = Box<dyn Fn(&Outcome) -> f64>;

/// One measured round.
#[derive(Clone, Debug)]
struct Round {
    round: usize,
    role: &'static str,
    label: String,
    rate: Option<f64>,
    outcome: Outcome,
}

fn us(ns: u64) -> f64 {
    (ns as f64 / 100.0).round() / 10.0
}

/// A latency for people: ms with three significant digits.
pub fn ms(ns: u64) -> String {
    let m = ns as f64 / 1e6;
    if m >= 100.0 {
        format!("{m:.0} ms")
    } else if m >= 10.0 {
        format!("{m:.1} ms")
    } else {
        format!("{m:.2} ms")
    }
}

/// A count with thousands separators.
pub fn thousands(n: f64) -> String {
    let s = format!("{:.0}", n.max(0.0));
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// How the change compares on one metric: the median difference in percent
/// and whether every round of one build beat every round of the other.
#[derive(Clone, Debug, PartialEq)]
pub struct Compare {
    pub delta_pct: f64,
    /// `Some(true)`: the change is worse in every round; `Some(false)`:
    /// better in every round; `None`: the rounds overlap.
    pub every_round_worse: Option<bool>,
}

/// The fewest rounds a build for "faster or slower in every round".
pub const MIN_ROUNDS: usize = 3;

/// `higher_is_worse`: true for latency, false for throughput.
pub fn compare(base: &[f64], change: &[f64], higher_is_worse: bool) -> Compare {
    let mb = median(base.to_vec());
    let mc = median(change.to_vec());
    let delta_pct = if mb > 0.0 {
        (mc - mb) / mb * 100.0
    } else {
        0.0
    };
    let max = |v: &[f64]| v.iter().copied().fold(f64::MIN, f64::max);
    let min = |v: &[f64]| v.iter().copied().fold(f64::MAX, f64::min);
    let separated_up = min(change) > max(base);
    let separated_down = max(change) < min(base);
    // With n rounds each, identical builds separate completely by chance
    // with probability 2 / C(2n, n): 33% at 2 rounds, 10% at 3, 0.8% at 5.
    // Below 3 rounds a build, "every round" says nothing.
    let every_round_worse = if base.len() < MIN_ROUNDS || change.len() < MIN_ROUNDS {
        None
    } else if separated_up {
        Some(higher_is_worse)
    } else if separated_down {
        Some(!higher_is_worse)
    } else {
        None
    };
    Compare {
        delta_pct,
        every_round_worse,
    }
}

fn csv(rounds: &[Round], load: &Load) -> String {
    let mut s = String::from(
        "round,build,label,mode,rate,conns,warmup_s,duration_s,answered,rps,failed,error_statuses,p50_us,p90_us,p99_us,p999_us,max_us,generator_lag_p99_us\n",
    );
    for r in rounds {
        let o = &r.outcome;
        let h = &o.latency;
        let label = r.label.replace('"', "'");
        let _ = writeln!(
            s,
            "{},{},\"{label}\",{},{},{},{},{},{},{:.1},{},{},{},{},{},{},{},{}",
            r.round,
            r.role,
            if r.rate.is_some() { "fixed" } else { "max" },
            r.rate.map(|x| format!("{x:.0}")).unwrap_or_default(),
            load.conns,
            load.warmup.as_secs_f64(),
            load.duration.as_secs_f64(),
            o.answered(),
            o.rps(),
            o.failed,
            o.error_statuses(),
            us(h.quantile(0.5)),
            us(h.quantile(0.9)),
            us(h.quantile(0.99)),
            us(h.quantile(0.999)),
            us(h.max()),
            us(o.lag.quantile(0.99)),
        );
    }
    s
}

fn round_json(r: &Round) -> Value {
    let o = &r.outcome;
    let h = &o.latency;
    json!({
        "round": r.round, "build": r.role, "label": r.label, "rate": r.rate,
        "answered": o.answered(), "rps": (o.rps() * 10.0).round() / 10.0,
        "failed": o.failed, "first_error": o.first_error,
        "statuses": o.statuses.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
        "p50_us": us(h.quantile(0.5)), "p90_us": us(h.quantile(0.9)), "p99_us": us(h.quantile(0.99)),
        "p999_us": us(h.quantile(0.999)), "max_us": us(h.max()),
        "generator_lag_p99_us": us(o.lag.quantile(0.99)),
    })
}

fn round_line(r: &Round, width: usize) -> String {
    let o = &r.outcome;
    let h = &o.latency;
    let mut line = format!(
        "round {}  {:width$}  {:>9} req/s  p50 {:>9}  p99 {:>9}  p99.9 {:>9}  max {:>9}",
        r.round,
        r.label,
        thousands(o.rps()),
        ms(h.quantile(0.5)),
        ms(h.quantile(0.99)),
        ms(h.quantile(0.999)),
        ms(h.max()),
    );
    if o.failed > 0 || o.error_statuses() > 0 {
        let _ = write!(
            line,
            "  {} failed, {} error statuses",
            o.failed,
            o.error_statuses()
        );
    }
    line
}

fn progress(on: bool, msg: &str) {
    if on {
        eprintln!("giong bench: {msg}");
    }
}

fn seconds(args: &Args, name: &str, default: f64, min: f64) -> Result<Duration, Failure> {
    let v = args.number::<f64>(name)?.unwrap_or(default);
    if !v.is_finite() || v < min || v > 86_400.0 {
        return Err(Failure::new(
            Code::USAGE,
            format!("--{name} {v} is out of range"),
            format!("give --{name} a number of seconds from {min} to 86400"),
        ));
    }
    Ok(Duration::from_secs_f64(v))
}

fn run(args: &Args) -> Result<Report, Failure> {
    let json_out = args.json();
    let talk = !json_out && std::io::stderr().is_terminal();
    let rate = parse_rate(args.value("rate"))?;
    let conns = args.number::<usize>("conns")?.unwrap_or(50);
    if conns == 0 || conns > MAX_CONNS {
        return Err(Failure::new(
            Code::USAGE,
            format!("--conns {conns} is out of range"),
            format!("give --conns a number from 1 to {MAX_CONNS}"),
        ));
    }
    let duration = seconds(args, "duration", 10.0, 0.1)?;
    let warmup = seconds(args, "warmup", 2.0, 0.0)?;
    let timeout = seconds(args, "timeout", 10.0, 0.01)?;
    let reference = args.value("against");
    let rounds_n = args
        .number::<usize>("rounds")?
        .unwrap_or(if reference.is_some() { 5 } else { 1 });
    if rounds_n == 0 || rounds_n > 1000 {
        return Err(Failure::new(
            Code::USAGE,
            format!("--rounds {rounds_n} is out of range"),
            "give --rounds a number from 1 to 1000",
        ));
    }
    let max_regression = args.number::<f64>("max-regression")?;
    if max_regression.is_some() && rounds_n < MIN_ROUNDS {
        return Err(Failure::new(
            Code::USAGE,
            format!("--max-regression needs at least {MIN_ROUNDS} rounds a build, not {rounds_n}"),
            "leave out --rounds (5 with --against) or give --rounds 3 or more: fewer rounds cannot tell a slowdown from noise",
        ));
    }
    if max_regression.is_some() && reference.is_none() {
        return Err(Failure::new(
            Code::USAGE,
            "--max-regression needs --against",
            "write giong bench --against main --max-regression 5",
        ));
    }
    if let Some(w) = args.value("workers") {
        if w.parse::<u32>().map_or(true, |w| w == 0) {
            return Err(Failure::new(
                Code::USAGE,
                format!("--workers {w:?} is not a number of workers"),
                "give --workers a whole number, 1 or more",
            ));
        }
    }
    let url = args.words.first().map(String::as_str);
    if url.is_some() && reference.is_some() {
        return Err(Failure::new(
            Code::USAGE,
            "--against builds and starts both versions itself, so it takes no URL",
            "write giong bench --against main inside the project",
        ));
    }

    // Where to send requests, and the requests.
    let (url_target, url_path) = match url {
        Some(u) => {
            let (authority, path) = parse_url(u)?;
            (Some((resolve(&authority)?, authority)), Some(path))
        }
        None => (None, None),
    };
    let mix = match args.value("path") {
        Some(p) => parse_mix(p)?,
        None => vec![(url_path.unwrap_or_else(|| "/".into()), 1)],
    };
    let data = args.value("data").unwrap_or("");
    let method = args
        .value("method")
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| {
            if data.is_empty() {
                "GET".into()
            } else {
                "POST".into()
            }
        });
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(Failure::new(
            Code::USAGE,
            format!("--method {method:?} is not an HTTP method"),
            "write --method GET, POST, PUT, PATCH, DELETE or HEAD",
        ));
    }
    let mut headers = Vec::new();
    if let Some(h) = args.value("header") {
        let ok = h.split_once(':').is_some_and(|(n, _)| {
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
        if !ok || h.contains(['\r', '\n']) {
            return Err(Failure::new(
                Code::USAGE,
                format!("--header {h:?} is not 'Name: value'"),
                "write --header 'Authorization: Bearer x'",
            ));
        }
        headers.push(h.to_string());
    }
    let has_type = headers
        .iter()
        .any(|h| h.to_ascii_lowercase().starts_with("content-type:"));
    if !has_type && (data.starts_with('{') || data.starts_with('[')) {
        headers.push("Content-Type: application/json".into());
    }

    let mut r = Report::new("bench");
    let root = find_manifest_dir(&std::env::current_dir().unwrap_or_default());
    let host_for = |authority: &str| authority.to_string();
    let make_load = |authority: &str| {
        let order = cycle(&mix);
        Load {
            requests: order
                .iter()
                .map(|&i| {
                    Request::new(
                        &method,
                        &mix[i].0,
                        &host_for(authority),
                        &headers,
                        data.as_bytes(),
                    )
                })
                .collect(),
            conns,
            warmup,
            duration,
            timeout,
        }
    };

    let mut rounds: Vec<Round> = Vec::new();
    let mut fixed: Option<f64> = match rate {
        Rate::Fixed(x) => Some(x),
        _ => None,
    };
    let mut probe_rps = None;
    let mut builds: Vec<Build> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut csv_dir: Option<PathBuf> = None;

    if let Some((addr, authority)) = &url_target {
        let load = make_load(authority);
        if rate == Rate::Auto {
            progress(talk, "finding the most the service answers (3 s)");
            let p = probe(&load, *addr)?;
            probe_rps = Some(p);
            fixed = Some((p * 0.5).max(1.0));
        }
        for i in 1..=rounds_n {
            progress(
                talk,
                &format!("round {i}/{rounds_n} against {}", url.unwrap_or("")),
            );
            let outcome = loadgen::run(&load.plan(*addr, fixed));
            rounds.push(Round {
                round: i,
                role: "service",
                label: url.unwrap_or("").to_string(),
                rate: fixed,
                outcome,
            });
        }
        if let Some(root) = &root {
            csv_dir = Some(root.join(".giong").join("bench"));
        }
        finish(
            &mut r,
            &rounds,
            &load,
            rate,
            probe_rps,
            &notes,
            args,
            csv_dir,
            max_regression,
        )?;
        return Ok(r);
    }

    // Build and start the service ourselves.
    let root = root.ok_or_else(|| {
        Failure::new(
            Code::NOT_A_PROJECT,
            "no Cargo.toml here or above, and no URL to measure",
            "run giong bench inside the service's project, or give the URL of a running service",
        )
    })?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
    let project = Project::at(&cwd)?;
    let bench_dir = crate::state::ensure_sub(&root, &["bench"]).map_err(|e| {
        Failure::new(
            Code::BENCH_TARGET,
            format!("cannot create {}: {e}", root.join(".giong/bench").display()),
            "check that the project directory is writable",
        )
    })?;
    let _ = super::db::ignore_secrets(&root);
    let log = bench_dir.join("service.log");
    let _ = crate::state::create(&log);
    let bin = args.value("bin");

    let mut worktree = None;
    if let Some(reference) = reference {
        let (wt, dir, base_label, change_label) = against(&cwd, reference)?;
        progress(talk, &format!("building {base_label} (release)"));
        // Its own target directory: sharing the project's would let cargo
        // take one build's binary for the other's (same package, same
        // output file, newer than the sources). Kept between runs.
        let base_target = project.target_dir.join("giong-bench-base");
        let base_bin = build(&dir, Some(&base_target), bin, json_out).map_err(|f| {
            Failure::new(
                Code::BENCH_AGAINST,
                format!("{reference} does not build: {}", f.what),
                "pick a later commit, or build it by hand to see why",
            )
        })?;
        let base_bin = keep(&base_bin, &root, "base")?;
        progress(talk, &format!("building {change_label} (release)"));
        let change_bin = keep(&build(&cwd, None, bin, json_out)?, &root, "change")?;
        builds.push(Build {
            role: "base",
            label: base_label,
            binary: base_bin,
            dir: dir.clone(),
        });
        builds.push(Build {
            role: "change",
            label: change_label,
            binary: change_bin,
            dir: cwd.clone(),
        });
        worktree = Some(wt);
        let same_bytes =
            std::fs::read(&builds[0].binary).ok() == std::fs::read(&builds[1].binary).ok();
        if same_bytes {
            notes.push("the two builds are the same binary: the change does not reach the release build, so any difference is noise".into());
        }
    } else {
        progress(talk, "building the service (release)");
        let b = keep(&build(&cwd, None, bin, json_out)?, &root, "service")?;
        let label = b
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        builds.push(Build {
            role: "service",
            label,
            binary: b,
            dir: cwd.clone(),
        });
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    notes.push(format!(
        "the load generator shares this machine's {cores} cores with the service, so both builds pay for it alike; absolute numbers that matter come from two machines"
    ));

    let env = service_env(&root, args.value("workers"));
    notes.extend(env.2.iter().cloned());
    let load = make_load("127.0.0.1");

    if rate == Rate::Auto {
        progress(
            talk,
            &format!("finding the most {} answers (3 s)", builds[0].label),
        );
        let running = start(&builds[0], &env, &log, "probe")?;
        let p = probe(&load, running.addr)?;
        drop(running);
        probe_rps = Some(p);
        fixed = Some((p * 0.5).max(1.0));
    }
    for i in 1..=rounds_n {
        let order: Vec<&Build> = if i % 2 == 1 {
            builds.iter().collect()
        } else {
            builds.iter().rev().collect()
        };
        for b in order {
            progress(talk, &format!("round {i}/{rounds_n}: {}", b.label));
            let mut running = start(b, &env, &log, &format!("round {i} {}", b.label))?;
            let outcome = loadgen::run(&load.plan(running.addr, fixed));
            if let Ok(Some(status)) = running.child.try_wait() {
                return Err(Failure::new(
                    Code::BENCH_TARGET,
                    format!("{} exited ({status}) during round {i}{}", b.label, log_tail(&log)),
                    "read .giong/bench/service.log: a panic or a lost database connection is the usual cause",
                ));
            }
            drop(running);
            rounds.push(Round {
                round: i,
                role: b.role,
                label: b.label.clone(),
                rate: fixed,
                outcome,
            });
        }
    }
    drop(worktree);
    r.data(
        "builds",
        json!(builds
            .iter()
            .map(|b| json!({"build": b.role, "label": b.label, "binary": b.binary}))
            .collect::<Vec<_>>()),
    );
    finish(
        &mut r,
        &rounds,
        &load,
        rate,
        probe_rps,
        &notes,
        args,
        Some(bench_dir),
        max_regression,
    )?;
    Ok(r)
}

/// The most the service answers: 1 s of warm-up, 2 s measured, as fast as it
/// answers.
fn probe(load: &Load, addr: SocketAddr) -> Result<f64, Failure> {
    let mut l = load.clone();
    l.warmup = Duration::from_secs(1);
    l.duration = Duration::from_secs(2);
    let o = loadgen::run(&l.plan(addr, None));
    if o.answered() == 0 {
        return Err(Failure::new(
            Code::BENCH_TARGET,
            format!(
                "the service answered no request at {addr}: {}",
                o.first_error.as_deref().unwrap_or("no answer")
            ),
            "check the URL and path (curl -i the same URL), or give --rate",
        ));
    }
    Ok(o.rps())
}

#[allow(clippy::too_many_arguments)]
fn finish(
    r: &mut Report,
    rounds: &[Round],
    load: &Load,
    rate: Rate,
    probe_rps: Option<f64>,
    notes: &[String],
    args: &Args,
    csv_dir: Option<PathBuf>,
    max_regression: Option<f64>,
) -> Result<(), Failure> {
    let fixed = rounds.first().and_then(|x| x.rate);
    let mut head = match fixed {
        Some(x) => format!("{} req/s fixed rate", thousands(x)),
        None => "as fast as the service answers".to_string(),
    };
    if let (Rate::Auto, Some(p)) = (rate, probe_rps) {
        let _ = write!(
            head,
            " (half of the {} req/s it answered at most)",
            thousands(p)
        );
    }
    r.line(format!(
        "{head}, {} connections, {} s measured after {} s of warm-up.",
        load.conns,
        load.duration.as_secs_f64(),
        load.warmup.as_secs_f64()
    ));
    for n in notes {
        r.line(format!("Note: {n}."));
    }
    let width = rounds.iter().map(|x| x.label.len()).max().unwrap_or(0);
    for x in rounds {
        r.line(round_line(x, width));
    }

    // Generator lag and failures, over every round.
    let worst_lag = rounds
        .iter()
        .map(|x| x.outcome.lag.quantile(0.99))
        .max()
        .unwrap_or(0);
    if fixed.is_some() && worst_lag > 1_000_000 {
        r.push(
            Finding::new(Code::BENCH_GENERATOR_BEHIND, Severity::Warning, format!("the load generator sent requests up to {} late (p99)", ms(worst_lag)))
                .detail("those latencies include the generator's own delay: the machine is busy, or the rate needs more threads than it runs well")
                .fix("lower --rate or --conns, close other programs, or run giong bench from another machine with the service's URL"),
        );
    }
    let total: u64 = rounds
        .iter()
        .map(|x| x.outcome.answered() + x.outcome.failed)
        .sum();
    let failed: u64 = rounds.iter().map(|x| x.outcome.failed).sum();
    let statuses: u64 = rounds.iter().map(|x| x.outcome.error_statuses()).sum();
    if failed + statuses > 0 {
        let share = (failed + statuses) as f64 / total.max(1) as f64;
        let first = rounds.iter().find_map(|x| x.outcome.first_error.clone());
        let mut detail =
            format!("{failed} got no answer and {statuses} got a 4xx or 5xx status, of {total}");
        if let Some(e) = first {
            let _ = write!(detail, "; first: {e}");
        }
        r.push(
            Finding::new(
                Code::BENCH_ERRORS,
                if share > 0.01 {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                format!("{:.2}% of requests failed", share * 100.0),
            )
            .detail(detail)
            .fix(
                "check --path and --method, and read the service's log (.giong/bench/service.log)",
            ),
        );
    }

    // The comparison.
    let base: Vec<&Round> = rounds.iter().filter(|x| x.role == "base").collect();
    let change: Vec<&Round> = rounds.iter().filter(|x| x.role == "change").collect();
    let metric = |v: &[&Round], f: &dyn Fn(&Outcome) -> f64| {
        v.iter().map(|x| f(&x.outcome)).collect::<Vec<f64>>()
    };
    let p = |q: f64| move |o: &Outcome| o.latency.quantile(q) as f64;
    let mut summary = json!({});
    if !base.is_empty() && !change.is_empty() {
        let metrics: [(&str, Metric, bool); 4] = [
            ("rps", Box::new(|o: &Outcome| o.rps()), false),
            ("p50", Box::new(p(0.5)), true),
            ("p99", Box::new(p(0.99)), true),
            ("p999", Box::new(p(0.999)), true),
        ];
        r.line(String::new());
        r.line(format!(
            "{:width$}  {:>9}  {:>9}  {:>9}  {:>9}",
            "median", "req/s", "p50", "p99", "p99.9",
        ));
        for (name, set) in [(&base[0].label, &base), (&change[0].label, &change)] {
            let m = |f: &dyn Fn(&Outcome) -> f64| median(metric(set, f));
            r.line(format!(
                "{name:width$}  {:>9}  {:>9}  {:>9}  {:>9}",
                thousands(m(&metrics[0].1)),
                ms(m(&metrics[1].1) as u64),
                ms(m(&metrics[2].1) as u64),
                ms(m(&metrics[3].1) as u64),
            ));
        }
        let mut cells = Vec::new();
        let mut obj = serde_json::Map::new();
        for (name, f, higher_is_worse) in &metrics {
            let c = compare(
                &metric(&base, f.as_ref()),
                &metric(&change, f.as_ref()),
                *higher_is_worse,
            );
            let mark = match c.every_round_worse {
                Some(true) => "!",
                Some(false) => "*",
                None => " ",
            };
            cells.push(format!("{:>9}{mark:1}", format!("{:+.1}%", c.delta_pct)));
            obj.insert(
                name.to_string(),
                json!({"delta_pct": (c.delta_pct * 10.0).round() / 10.0, "every_round": match c.every_round_worse { Some(true) => "worse", Some(false) => "better", None => "overlap" }}),
            );
        }
        r.line(
            format!("{:width$}  {}", "change", cells.join(" "))
                .trim_end()
                .to_string(),
        );
        r.line("(* better in every round, ! worse in every round, no mark: the rounds overlap)");
        summary = Value::Object(obj);

        // The gate: p99 at a fixed rate, throughput as fast as it answers.
        let (key, f, higher_is_worse): (&str, &Metric, bool) = if fixed.is_some() {
            ("p99 latency", &metrics[2].1, true)
        } else {
            ("throughput", &metrics[0].1, false)
        };
        let c = compare(&metric(&base, f), &metric(&change, f), higher_is_worse);
        let worse_pct = if higher_is_worse {
            c.delta_pct
        } else {
            -c.delta_pct
        };
        if let (Some(limit), Some(true)) = (max_regression, c.every_round_worse) {
            if worse_pct > limit {
                r.push(
                    Finding::new(Code::BENCH_REGRESSION, Severity::Error, format!("{key} is {worse_pct:.1}% worse than {} in every round", base[0].label))
                        .detail(format!("the limit is {limit}% (--max-regression); rounds alternate between the builds, so the difference is not the machine"))
                        .fix("profile the change (giong top --profile on the running service), or raise --max-regression and say why"),
                );
            }
        }
    }

    // Raw rounds.
    let path = match args.value("csv") {
        Some(p) => Some(PathBuf::from(p)),
        None => csv_dir.map(|d| {
            let t = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            d.join(format!("{t}.csv"))
        }),
    };
    if let Some(path) = &path {
        let written = if args.value("csv").is_some() {
            // A path the user gave.
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(path, csv(rounds, load))
        } else {
            // .giong/bench: never through a link.
            let root = path.parent().and_then(Path::parent).and_then(Path::parent);
            root.map_or(Ok(()), |r| {
                crate::state::ensure_sub(r, &["bench"]).map(drop)
            })
            .and_then(|()| crate::state::replace(path, csv(rounds, load).as_bytes()))
        };
        written.map_err(|e| {
            Failure::new(
                Code::USAGE,
                format!("cannot write {}: {e}", path.display()),
                "give --csv a writable path",
            )
        })?;
        r.line(format!("Rounds: {}", path.display()));
    }
    r.data("mode", json!(if fixed.is_some() { "fixed" } else { "max" }));
    r.data("rate", json!(fixed));
    r.data("probe_rps", json!(probe_rps));
    r.data("conns", json!(load.conns));
    r.data("warmup_s", json!(load.warmup.as_secs_f64()));
    r.data("duration_s", json!(load.duration.as_secs_f64()));
    r.data(
        "rounds",
        json!(rounds.iter().map(round_json).collect::<Vec<_>>()),
    );
    r.data("compare", summary);
    r.data("csv", json!(path));
    r.data("notes", json!(notes));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse() {
        assert_eq!(parse_rate(None).unwrap(), Rate::Auto);
        assert_eq!(parse_rate(Some("max")).unwrap(), Rate::Max);
        assert_eq!(parse_rate(Some("0")).unwrap(), Rate::Max);
        assert_eq!(parse_rate(Some("2500")).unwrap(), Rate::Fixed(2500.0));
        for bad in ["-1", "fast", "inf", "NaN"] {
            assert_eq!(
                parse_rate(Some(bad)).unwrap_err().code,
                Code::USAGE,
                "{bad}"
            );
        }
    }

    #[test]
    fn mixes_parse_and_spread() {
        let m = parse_mix("/a@2,/b").unwrap();
        assert_eq!(m, [("/a".to_string(), 2), ("/b".to_string(), 1)]);
        assert_eq!(cycle(&m), [0, 1, 0]);
        let m = parse_mix("/users@9,/health@1").unwrap();
        let c = cycle(&m);
        assert_eq!(c.len(), 10);
        assert_eq!(c.iter().filter(|&&i| i == 1).count(), 1);
        assert_eq!(cycle(&parse_mix("/x").unwrap()), [0]);
        for bad in ["a", "/a@0", "/a@x", "/a b", "/a@101"] {
            assert!(parse_mix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_parse() {
        assert_eq!(
            parse_url("http://127.0.0.1:8080").unwrap(),
            ("127.0.0.1:8080".into(), "/".into())
        );
        assert_eq!(
            parse_url("http://h/a/b?c=1").unwrap(),
            ("h".into(), "/a/b?c=1".into())
        );
        assert_eq!(parse_url("http://h?x").unwrap(), ("h".into(), "/?x".into()));
        assert_eq!(
            parse_url("http://[::1]:9/").unwrap(),
            ("[::1]:9".into(), "/".into())
        );
        assert_eq!(
            parse_url("https://h/").unwrap_err().code,
            Code::BENCH_TARGET
        );
        assert!(parse_url("ftp://h").is_err());
        assert!(parse_url("http://u:p@h/").is_err());
        assert!(parse_url("http:///x").is_err());
        assert_eq!(resolve("127.0.0.1:9").unwrap().port(), 9);
        assert_eq!(resolve("127.0.0.1").unwrap().port(), 80);
        assert_eq!(resolve("[::1]").unwrap().port(), 80);
    }

    #[test]
    fn comparisons_need_every_round() {
        let c = compare(&[10.0, 11.0, 12.0], &[13.0, 14.0, 15.0], true);
        assert_eq!(c.every_round_worse, Some(true));
        assert!((c.delta_pct - 27.27).abs() < 0.01);
        assert_eq!(
            compare(&[10.0, 11.0, 12.0], &[13.0, 14.0, 15.0], false).every_round_worse,
            Some(false)
        );
        assert_eq!(
            compare(&[10.0, 14.0], &[12.0, 13.0], true).every_round_worse,
            None
        );
        assert_eq!(compare(&[], &[1.0], true).every_round_worse, None);
        // Two rounds a build cannot tell.
        assert_eq!(
            compare(&[1.0, 1.1], &[2.0, 2.1], true).every_round_worse,
            None
        );
        assert_eq!(median(vec![3.0, 1.0, 2.0, 10.0]), 2.5);
    }

    #[test]
    fn formats() {
        assert_eq!(thousands(1234567.4), "1,234,567");
        assert_eq!(thousands(999.0), "999");
        assert_eq!(ms(1_234_000), "1.23 ms");
        assert_eq!(ms(12_340_000), "12.3 ms");
        assert_eq!(ms(123_400_000), "123 ms");
        assert_eq!(us(1_234_567), 1234.6);
    }
}
