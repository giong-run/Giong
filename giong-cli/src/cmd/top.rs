//! `giong top`: a live terminal view of a running service, read from its
//! giong-mcp: workers, tasks by the line that spawned them, pools, warnings
//! (a worker stuck in one task, named by its spawn line), and on request a
//! profile.
//!
//! Nothing is recorded while nobody looks: the view asks giong-mcp, which
//! reads counters the runtime keeps anyway. No rebuild with special flags.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::routes::MCP_FLAG;
use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::report::{Failure, Finding, Report, Severity};
use crate::service::{self, Service};

pub const COMMAND: Command = Command {
    name: "top",
    summary: "watch a running service: workers, tasks by spawn line, pools, stuck workers",
    usage:
        "giong top [--interval <ms>] [--once] [--profile <ms>] [--tasks <n>] [--mcp <address>]\n\n\
            Shows the running service's workers (state, driver, busy share, polls per second, \
            tasks, I/O in flight), its tasks grouped by the line that spawned them, its \
            database pools and its warnings, refreshed every second, until Ctrl-C. A worker \
            stuck inside one task is named with that task's spawn line. With --once, --json or \
            when the output is not a terminal, it prints one snapshot and exits; warnings \
            become findings (exit status 1 for a stuck worker). --profile <ms> times every \
            task poll for that long and lists where the workers spent their time (a small \
            cost to the service while it runs).",
    flags: &[
        Flag {
            name: "interval",
            kind: Kind::Value,
            help: "milliseconds between refreshes (200 to 60000, default 1000)",
        },
        Flag {
            name: "once",
            kind: Kind::Switch,
            help: "print one snapshot and exit",
        },
        Flag {
            name: "profile",
            kind: Kind::Value,
            help: "take a profile of this many milliseconds (100 to 10000) and show it",
        },
        Flag {
            name: "tasks",
            kind: Kind::Value,
            help: "how many spawn locations to show (1 to 500, default 10)",
        },
        MCP_FLAG,
    ],
    max_words: 0,
    run,
};

/// What one refresh read.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub overview: Value,
    pub workers: Value,
    pub tasks: Value,
    pub sources: Value,
    pub profile: Option<Value>,
}

fn take(svc: &Service, tasks: u64, profile_ms: Option<u64>) -> Result<Snapshot, Failure> {
    Ok(Snapshot {
        overview: svc.call("overview", json!({}))?,
        workers: svc.call("workers", json!({}))?,
        tasks: svc.call("tasks", json!({ "limit": tasks }))?,
        sources: svc.call("sources", json!({}))?,
        profile: match profile_ms {
            Some(ms) => Some(svc.call("profile", json!({ "duration_ms": ms, "limit": tasks }))?),
            None => None,
        },
    })
}

fn bytes(n: Option<u64>) -> String {
    match n {
        None => "?".into(),
        Some(n) if n >= 1 << 30 => format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64),
        Some(n) if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64),
        Some(n) if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / 1024.0),
        Some(n) => format!("{n} B"),
    }
}

fn num(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Number(n) => match (n.as_u64(), n.as_i64(), n.as_f64()) {
            (Some(u), _, _) => u.to_string(),
            (_, Some(i), _) => i.to_string(),
            (_, _, Some(f)) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{f:.0}"),
            (_, _, Some(f)) => format!("{f:.2}"),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// Cuts `s` to `max` characters, keeping the end (a file path's tail says
/// more than its head).
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max + 1;
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

/// The screen for one snapshot, `width` columns wide.
pub fn render(s: &Snapshot, width: usize) -> String {
    let width = width.clamp(60, 240);
    let mut out = String::new();
    let p = &s.overview["process"];
    let rt = &s.overview["runtime"];
    out.push_str(&format!(
        "giong top  pid {}  up {}s  rss {}  threads {}  fds {}/{}  workers {}{}{}\n",
        num(&p["pid"]),
        num(&p["uptime_s"]),
        bytes(p["rss_bytes"].as_u64()),
        num(&p["threads"]),
        num(&p["open_fds"]),
        num(&p["fd_limit"]),
        num(&rt["workers"]),
        if rt["shutting_down"] == true {
            "  SHUTTING DOWN"
        } else {
            ""
        },
        if rt["profiling"] == true {
            "  profiling"
        } else {
            ""
        },
    ));
    for w in s.overview["warnings"].as_array().into_iter().flatten() {
        out.push_str(&format!("! {}\n", w.as_str().unwrap_or("")));
    }
    out.push('\n');
    out.push_str(&format!(
        "{:>3} {:<8} {:<9} {:>5} {:>9} {:>6} {:>4} {:>6} {:>6} {:>8}  {}\n",
        "W",
        "STATE",
        "DRIVER",
        "BUSY",
        "POLLS/S",
        "TASKS",
        "RUN",
        "TIMERS",
        "IO",
        "BUFFERS",
        "BLOCKED"
    ));
    let overview_workers = s.overview["workers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for w in s.workers["workers"].as_array().into_iter().flatten() {
        let i = w["worker"].as_u64().unwrap_or(0) as usize;
        let ov = overview_workers.get(i).cloned().unwrap_or(Value::Null);
        let busy = ov["busy"]
            .as_f64()
            .or_else(|| w["last_second"]["busy"].as_f64())
            .map(|b| format!("{:.0}%", b * 100.0))
            .unwrap_or_else(|| "-".into());
        let bufs = match (
            w["recv_buffers"]["free"].as_u64(),
            w["recv_buffers"]["total"].as_u64(),
        ) {
            (Some(f), Some(t)) => format!("{f}/{t}"),
            _ => "-".into(),
        };
        let blocked = match w["blocked"].as_object() {
            Some(b) => format!(
                "{} ms in {}",
                num(&b["for_ms"]),
                b.get("in_task").and_then(Value::as_str).unwrap_or("?")
            ),
            None => match w.get("snapshot_error").and_then(Value::as_str) {
                Some(e) => format!("no answer: {e}"),
                None => String::new(),
            },
        };
        out.push_str(&format!(
            "{:>3} {:<8} {:<9} {:>5} {:>9} {:>6} {:>4} {:>6} {:>6} {:>8}  {}\n",
            i,
            w["state"].as_str().unwrap_or("?"),
            w["driver"].as_str().unwrap_or("-"),
            busy,
            num(&w["last_second"]["polls_per_s"]),
            num(&w["tasks"]),
            num(&w["runnable"]),
            num(&w["timers"]),
            num(&w["io_in_flight"]),
            bufs,
            blocked
        ));
    }
    out.push_str(&format!(
        "\nTASKS {} live at {} spawn locations\n",
        num(&s.tasks["total_tasks"]),
        num(&s.tasks["locations"])
    ));
    let loc_width = width.saturating_sub(24).max(30);
    for row in s.tasks["by_location"].as_array().into_iter().flatten() {
        let per = row["per_worker"]
            .as_array()
            .map(|a| a.iter().map(num).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        out.push_str(&format!(
            "{:>7}  {:<loc_width$}  {}\n",
            num(&row["tasks"]),
            tail(row["location"].as_str().unwrap_or("?"), loc_width),
            per
        ));
    }
    let sources = s.sources["sources"].as_array().cloned().unwrap_or_default();
    if !sources.is_empty() {
        out.push_str("\nSOURCES\n");
        for src in &sources {
            let stats = src["total"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| format!("{k} {}", num(v)))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "  {} {}: {stats}\n",
                src["kind"].as_str().unwrap_or("?"),
                src["name"].as_str().unwrap_or("?")
            ));
        }
    }
    if let Some(prof) = &s.profile {
        out.push_str(&format!(
            "\nPROFILE over {} ms (share of all workers' time)\n",
            num(&prof["duration_ms"])
        ));
        for row in prof["total"].as_array().into_iter().flatten() {
            let share = row["share_of_all_workers"]
                .as_f64()
                .map(|f| format!("{:.1}%", f * 100.0))
                .unwrap_or_else(|| "-".into());
            out.push_str(&format!(
                "{share:>7}  {:<loc_width$}  {} polls, max {} µs\n",
                tail(row["location"].as_str().unwrap_or("?"), loc_width),
                num(&row["polls"]),
                num(&row["max_us"])
            ));
        }
    }
    out
}

/// The service's warnings as findings.
pub fn findings(overview: &Value) -> Vec<Finding> {
    overview["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|w| {
            if w.contains("inside one poll") || w.contains("blocks the thread") {
                Finding::new(
                    Code::WORKER_BLOCKED,
                    Severity::Error,
                    "a worker is stuck in one task",
                )
                .detail(w)
                .fix(
                    "find the blocking call or long loop in the task named (std::fs, \
                         std::thread::sleep, std::net, a lock, a computation without .await): \
                         use giong::fs, giong::time::sleep or giong::net instead, move the work \
                         to a thread of its own, or yield with giong::yield_now().await",
                )
            } else {
                Finding::new(
                    Code::SERVICE_WARNING,
                    Severity::Warning,
                    "the service reports a warning",
                )
                .detail(w)
                .fix("read the warning; giong-mcp's README explains each kind")
            }
        })
        .collect()
}

/// A screen drawn over the last one without clearing it first (no flicker):
/// home, each line followed by erase-to-end-of-line, then erase below.
fn repaint(screen: &str) -> String {
    let mut out = String::from("\x1b[H");
    for line in screen.lines() {
        out.push_str(line);
        out.push_str("\x1b[K\n");
    }
    out.push_str("\x1b[J");
    out
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(120)
}

fn run(args: &Args) -> Result<Report, Failure> {
    let interval = args.number::<u64>("interval")?.unwrap_or(1000);
    if !(200..=60_000).contains(&interval) {
        return Err(Failure::new(
            Code::USAGE,
            format!("--interval {interval} is out of range"),
            "give --interval between 200 and 60000 milliseconds",
        ));
    }
    let tasks = args.number::<u64>("tasks")?.unwrap_or(10);
    if !(1..=500).contains(&tasks) {
        return Err(Failure::new(
            Code::USAGE,
            format!("--tasks {tasks} is out of range"),
            "give --tasks between 1 and 500",
        ));
    }
    let profile = args.number::<u64>("profile")?;
    if let Some(ms) = profile {
        if !(100..=10_000).contains(&ms) {
            return Err(Failure::new(
                Code::USAGE,
                format!("--profile {ms} is out of range"),
                "give --profile between 100 and 10000 milliseconds",
            ));
        }
    }
    let svc = service::locate(args.value("mcp"))?;
    let live = !args.has("once") && !args.json() && std::io::stdout().is_terminal();
    if !live {
        let snap = take(&svc, tasks, profile)?;
        let mut r = Report::new("top");
        r.line(render(&snap, terminal_width()).trim_end());
        for f in findings(&snap.overview) {
            r.push(f);
        }
        r.data("overview", snap.overview);
        r.data("workers", snap.workers);
        r.data("tasks", snap.tasks);
        r.data("sources", snap.sources);
        if let Some(p) = snap.profile {
            r.data("profile", p);
        }
        r.data("mcp", json!(svc.endpoint.to_string()));
        return Ok(r);
    }
    // Live: redraw until Ctrl-C or the service goes away. A profile is
    // taken once, on the first screen.
    let mut first = true;
    let mut out = std::io::stdout();
    loop {
        let started = Instant::now();
        let snap = take(&svc, tasks, if first { profile } else { None });
        let snap = match snap {
            Ok(s) => s,
            Err(f) if !first => {
                // The service stopped (or restarted under giong dev): say so
                // and keep trying, as top does for a process that is gone.
                let _ = write!(
                    out,
                    "{}",
                    repaint(&format!(
                        "{f}\n\nretrying every {interval} ms (Ctrl-C to stop)"
                    ))
                );
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(interval));
                continue;
            }
            Err(f) => return Err(f),
        };
        first = false;
        let screen = render(&snap, terminal_width());
        let _ = write!(
            out,
            "{}",
            repaint(&format!(
                "{screen}\n{} via {} (Ctrl-C to stop)",
                svc.found_in, svc.endpoint
            ))
        );
        let _ = out.flush();
        let spent = started.elapsed();
        std::thread::sleep(Duration::from_millis(interval).saturating_sub(spent));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repaints_erase_what_the_last_screen_left() {
        assert_eq!(repaint("a\nbc"), "\x1b[Ha\x1b[K\nbc\x1b[K\n\x1b[J");
    }

    fn sample() -> Snapshot {
        Snapshot {
            overview: json!({
                "process": {"pid": 7, "uptime_s": 12, "rss_bytes": 5_242_880, "threads": 4,
                            "open_fds": 20, "fd_limit": 1024},
                "runtime": {"workers": 2, "shutting_down": false, "profiling": false},
                "workers": [{"worker": 0, "busy": 0.25}, {"worker": 1, "busy": 1.0}],
                "warnings": ["worker 1 has been inside one poll of the task spawned at src/main.rs:40:9 for 2.3 s: that task blocks the thread"],
            }),
            workers: json!({"workers": [
                {"worker": 0, "state": "parked", "driver": "io_uring", "tasks": 3, "runnable": 0,
                 "timers": 1, "io_in_flight": 2, "recv_buffers": {"total": 64, "free": 60},
                 "last_second": {"polls_per_s": 120.0, "busy": 0.25}, "blocked": null},
                {"worker": 1, "state": "running", "driver": "epoll", "tasks": 1, "runnable": 1,
                 "snapshot_error": "worker 1 did not answer within 500 ms",
                 "last_second": null, "blocked": {"for_ms": 2300, "in_task": "src/main.rs:40:9"}}
            ]}),
            tasks: json!({"total_tasks": 4, "locations": 2, "by_location": [
                {"location": "giong-http/src/server.rs:200:13", "tasks": 3, "per_worker": [3, 0]},
                {"location": "src/main.rs:40:9", "tasks": 1, "per_worker": [0, 1]}
            ]}),
            sources: json!({"sources": [{"kind": "postgres pool", "name": "db:5432/app",
                "total": {"open": 4, "idle": 3}, "per_worker": []}], "unanswered": []}),
            profile: Some(json!({"duration_ms": 1000, "total": [
                {"location": "src/main.rs:40:9", "polls": 3, "share_of_all_workers": 0.5, "max_us": 999000.0}
            ]})),
        }
    }

    #[test]
    fn the_screen_shows_every_part() {
        let s = render(&sample(), 120);
        assert!(s.contains("pid 7"), "{s}");
        assert!(s.contains("rss 5.0 MiB"), "{s}");
        assert!(s.contains("fds 20/1024"), "{s}");
        assert!(s.contains("! worker 1 has been inside one poll"), "{s}");
        assert!(s.contains("io_uring"), "{s}");
        assert!(s.contains("25%"), "{s}");
        assert!(s.contains("100%"), "{s}");
        assert!(s.contains("60/64"), "{s}");
        assert!(s.contains("2300 ms in src/main.rs:40:9"), "{s}");
        assert!(s.contains("TASKS 4 live at 2 spawn locations"), "{s}");
        assert!(s.contains("giong-http/src/server.rs:200:13"), "{s}");
        assert!(
            s.contains("postgres pool db:5432/app: idle 3, open 4"),
            "{s}"
        );
        assert!(s.contains("    120 "), "{s}");
        assert!(s.contains("PROFILE over 1000 ms"), "{s}");
        assert!(s.contains("50.0%"), "{s}");
    }

    #[test]
    fn an_empty_or_odd_answer_still_renders() {
        let s = render(&Snapshot::default(), 10);
        assert!(s.starts_with("giong top"), "{s}");
        let odd = Snapshot {
            overview: json!({"process": "x", "workers": 3, "warnings": [1]}),
            workers: json!({"workers": [{"worker": "a"}]}),
            tasks: json!({"by_location": [{}]}),
            sources: json!({"sources": [{}]}),
            profile: Some(json!({})),
        };
        let _ = render(&odd, 80);
    }

    #[test]
    fn warnings_become_findings() {
        let f = findings(&sample().overview);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].code, Code::WORKER_BLOCKED);
        assert_eq!(f[0].severity, Severity::Error);
        let f = findings(&json!({"warnings": ["2 of 2 workers run on the epoll driver"]}));
        assert_eq!(f[0].code, Code::SERVICE_WARNING);
        assert_eq!(f[0].severity, Severity::Warning);
        assert!(findings(&json!({})).is_empty());
    }

    #[test]
    fn long_locations_keep_their_end() {
        assert_eq!(tail("abcdef", 4), "…def");
        assert_eq!(tail("abc", 4), "abc");
        assert_eq!(bytes(Some(512)), "512 B");
        assert_eq!(bytes(Some(2048)), "2.0 KiB");
        assert_eq!(bytes(None), "?");
    }
}
