//! The tools, and the state they read.
//!
//! Counters come from [`Handle::pulse`], which never disturbs a worker.
//! Details come from [`observe::snapshot`] and [`observe::take_profile`],
//! which run on the worker they describe; a worker that does not answer in
//! time is reported as such, with what its pulse says.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use giong::observe::{self, Location, PollProfile, Snapshot, WorkerState};
use giong::{ChosenBy, DriverChoice, Handle, JoinError};
use serde_json::{json, Map, Value};

use crate::proc;
use crate::schema;
use crate::watch::{Blocked, Watch};

pub(crate) struct State {
    target: Handle,
    snapshot_timeout: Duration,
    watch: RefCell<Watch>,
    /// A `profile` call is running.
    profiling: Cell<bool>,
}

impl State {
    pub(crate) fn new(target: Handle, snapshot_timeout: Duration) -> Self {
        Self {
            target,
            snapshot_timeout,
            watch: RefCell::new(Watch::default()),
            profiling: Cell::new(false),
        }
    }

    pub(crate) fn sample_now(&self) {
        let pulses = (0..self.target.workers())
            .map(|w| self.target.pulse(w))
            .collect();
        self.watch.borrow_mut().sample(pulses, Instant::now());
    }
}

/// A tool's entry in `tools/list`. `reads_only` is false for a tool that
/// changes what the service does while it runs, so that clients can ask
/// before calling it.
fn tool(
    name: &str,
    title: &str,
    description: &str,
    properties: Value,
    output: Value,
    reads_only: bool,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "additionalProperties": false,
        },
        "outputSchema": output,
        "annotations": {
            "readOnlyHint": reads_only,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false,
        },
    })
}

pub(crate) fn list() -> Value {
    json!([
        tool(
            "overview",
            "Overview",
            "Start here. The process (memory, threads, open descriptors, uptime), the runtime \
             (workers, shutdown), each worker's state, I/O driver, live tasks and load over the \
             last second, and warnings, such as a worker blocked inside one task.",
            json!({}),
            schema::overview(),
            true,
        ),
        tool(
            "workers",
            "Workers",
            "Each worker in detail: counters since start, rates and busy share over the last \
             second, timers, I/O operations in flight, receive buffers, and, for a blocked \
             worker, how long and in which task.",
            json!({}),
            schema::workers(),
            true,
        ),
        tool(
            "tasks",
            "Live tasks",
            "Live tasks grouped by the source line that spawned them, across workers, most \
             first. A count that keeps growing at one line between calls is a task leak.",
            json!({
                "worker": { "type": "integer", "minimum": 0, "description": "Only this worker." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 20,
                           "description": "How many spawn locations to list." },
            }),
            schema::tasks(),
            true,
        ),
        tool(
            "sources",
            "Sources",
            "What the service's registered sources report, such as giong-postgres, \
             giong-mysql and giong-redis connection pools: open, idle and busy connections, \
             waiters. Summed over workers, and per worker.",
            json!({}),
            schema::sources(),
            true,
        ),
        tool(
            "profile",
            "Profile",
            "Times every task poll on every worker for a while (default 1 s), then lists the \
             spawn locations the workers spent their time in, with poll counts, total, mean \
             and longest poll. While it runs, every worker reads the clock twice per poll, a \
             small cost the service pays. Refused while the service itself has profiling on.",
            json!({
                "duration_ms": { "type": "integer", "minimum": 100, "maximum": 10000,
                                 "default": 1000, "description": "How long to measure." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 20,
                           "description": "How many spawn locations to list." },
            }),
            schema::profile(),
            false,
        ),
    ])
}

/// `None` for an unknown tool; `Err` for a call the tool refuses, with a
/// message for the agent.
pub(crate) async fn call(
    state: &State,
    name: &str,
    args: &Map<String, Value>,
) -> Option<Result<Value, String>> {
    Some(match name {
        "overview" => no_args(args).map(|()| overview(state)),
        "workers" => match no_args(args) {
            Ok(()) => Ok(workers(state).await),
            Err(e) => Err(e),
        },
        "tasks" => tasks(state, args).await,
        "sources" => match no_args(args) {
            Ok(()) => Ok(sources(state).await),
            Err(e) => Err(e),
        },
        "profile" => profile(state, args).await,
        _ => return None,
    })
}

fn no_args(args: &Map<String, Value>) -> Result<(), String> {
    check_args(args, &[])
}

fn check_args(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match args.keys().find(|k| !allowed.contains(&k.as_str())) {
        None => Ok(()),
        Some(k) if allowed.is_empty() => {
            Err(format!("unknown argument {k:?}: this tool takes none"))
        }
        Some(k) => Err(format!(
            "unknown argument {k:?}: this tool takes {}",
            allowed.join(", ")
        )),
    }
}

fn int_arg(
    args: &Map<String, Value>,
    name: &str,
    default: Option<u64>,
    min: u64,
    max: u64,
) -> Result<Option<u64>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(default),
        // JSON Schema counts 2.0 as an integer, and some clients send one.
        Some(v) => match v.as_u64().or_else(|| {
            v.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                .map(|f| f as u64)
        }) {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(format!(
                "{name} must be a whole number from {min} to {max}, not {v}"
            )),
        },
    }
}

fn location(l: Location) -> String {
    format!("{}:{}:{}", l.file(), l.line(), l.column())
}

fn worker_state(s: WorkerState) -> &'static str {
    match s {
        WorkerState::Running => "running",
        WorkerState::Parked => "parked",
        WorkerState::Stopped => "stopped",
        _ => "unknown",
    }
}

fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

fn ms(d: Duration) -> f64 {
    round(d.as_secs_f64() * 1e3, 3)
}

fn us(d: Duration) -> f64 {
    round(d.as_secs_f64() * 1e6, 1)
}

/// `true` for a task this server spawned on a worker to ask it something.
fn ours(l: Location) -> bool {
    l.file() == file!()
}

fn blocked_json(b: Option<Blocked>, now: Instant) -> Value {
    match b {
        None => Value::Null,
        Some(b) => json!({
            "for_ms": now.duration_since(b.since).as_millis() as u64,
            "in_task": b.task.map(location),
        }),
    }
}

fn blocked_warning(w: usize, b: Blocked, now: Instant) -> String {
    let secs = now.duration_since(b.since).as_secs_f64();
    match b.task {
        Some(task) => format!(
            "worker {w} has been inside one poll of the task spawned at {} for {secs:.1} s: \
             that task blocks the thread (a blocking call, a long computation or a loop \
             without .await), and every other task on worker {w} waits",
            location(task)
        ),
        None => format!(
            "worker {w} has been awake without making progress for {secs:.1} s, outside any \
             task (in a destructor, a timer or the I/O driver)"
        ),
    }
}

/// Runs `f` on worker `w` and waits up to the snapshot timeout.
async fn ask<T, F, Fut>(target: &Handle, w: usize, timeout: Duration, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    let mut handle = target.spawn_on(w, f);
    match giong::time::timeout(timeout, &mut handle).await {
        Ok(Ok(value)) => Ok(value),
        // The worker is fine: what panicked is the code that answered on
        // it, such as a registered source.
        Ok(Err(JoinError::Panic(payload))) => {
            let what = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("a panic with no message");
            Err(format!(
                "worker {w} panicked while describing itself, most likely in a registered \
                 source's name() or stats(): {what}"
            ))
        }
        Ok(Err(_)) => Err(format!("worker {w} has stopped")),
        Err(_) => {
            handle.abort();
            Err(format!(
                "worker {w} did not answer within {} ms",
                timeout.as_millis()
            ))
        }
    }
}

/// Asks every worker at once.
async fn ask_all<T, F, Fut>(state: &State, f: F) -> Vec<Result<T, String>>
where
    T: Send + 'static,
    F: Fn() -> Fut + Clone + Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    let handles: Vec<_> = (0..state.target.workers())
        .map(|w| {
            let target = state.target.clone();
            let timeout = state.snapshot_timeout;
            let f = f.clone();
            giong::spawn(async move { ask(&target, w, timeout, f).await })
        })
        .collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(
            h.await
                .unwrap_or_else(|e| Err(format!("giong-mcp failed: {e}"))),
        );
    }
    out
}

async fn snapshots(state: &State) -> Vec<Result<Snapshot, String>> {
    ask_all(state, || async { observe::snapshot() })
        .await
        .into_iter()
        .map(|r| {
            r.map(|mut s| {
                let mut removed = 0;
                s.tasks_by_location.retain(|&(l, n)| {
                    if ours(l) {
                        removed += n;
                    }
                    !ours(l)
                });
                s.tasks = s.tasks.saturating_sub(removed);
                s
            })
        })
        .collect()
}

fn overview(state: &State) -> Value {
    state.sample_now();
    let now = Instant::now();
    let target = &state.target;
    let watch = state.watch.borrow();
    let mut workers = Vec::new();
    let mut warnings = Vec::new();
    // Workers that fell back to epoll, with the reason the first one gave.
    let mut on_epoll = 0;
    let mut epoll_reason = None;
    for w in 0..target.workers() {
        let p = target.pulse(w);
        let choice = target.driver_choice(w);
        if let Some(c) = &choice {
            if let (ChosenBy::Automatic, Some(e)) = (c.chosen_by, &c.io_uring_error) {
                on_epoll += 1;
                epoll_reason.get_or_insert_with(|| e.clone());
            }
        }
        let blocked = watch.blocked(w, now);
        let rates = watch.rates(w);
        if let Some(b) = blocked {
            warnings.push(blocked_warning(w, b, now));
        }
        workers.push(json!({
            "worker": w,
            "state": worker_state(p.state),
            "driver": p.driver.map(|d| d.name()),
            "driver_choice": choice.as_ref().map(driver_choice_json),
            "tasks": p.tasks,
            "runnable": p.runnable,
            "busy": rates.map(|r| round(r.busy, 2)),
            "polls_per_s": rates.map(|r| r.polls_per_s.round()),
            "blocked": blocked_json(blocked, now),
        }));
    }
    if let Some(reason) = epoll_reason {
        warnings.push(format!(
            "{on_epoll} of {} workers run on the epoll driver, because io_uring cannot be used \
             here: {reason}",
            target.workers()
        ));
    }
    let process = proc::process();
    if let (Some(open), Some(limit)) = (process["open_fds"].as_u64(), process["fd_limit"].as_u64())
    {
        if limit > 0 && open * 10 >= limit * 8 {
            warnings.push(format!(
                "{open} of the process's {limit} file descriptors are open: new connections \
                 will soon fail with \"too many open files\""
            ));
        }
    }
    json!({
        "giong_mcp": env!("CARGO_PKG_VERSION"),
        "process": process,
        "runtime": {
            "workers": target.workers(),
            "shutting_down": target.is_shutdown(),
            "profiling": target.is_profiling(),
        },
        "workers": workers,
        "warnings": warnings,
    })
}

/// How a worker got its driver: who chose it, why not io_uring, and the
/// kernel bugs giong works around.
fn driver_choice_json(c: &DriverChoice) -> Value {
    json!({
        "chosen_by": match c.chosen_by {
            ChosenBy::Builder => "builder",
            ChosenBy::Environment => "GIONG_DRIVER",
            _ => "automatic",
        },
        "io_uring_error": c.io_uring_error,
        "io_uring_error_code": c.io_uring_error_code.map(|code| code.to_string()),
        "kernel_workarounds": c
            .kernel_workarounds
            .iter()
            .map(|w| json!({ "name": w.name(), "description": w.description() }))
            .collect::<Vec<_>>(),
    })
}

async fn workers(state: &State) -> Value {
    let snaps = snapshots(state).await;
    state.sample_now();
    let now = Instant::now();
    let watch = state.watch.borrow();
    let list: Vec<Value> = snaps
        .into_iter()
        .enumerate()
        .map(|(w, snap)| {
            let p = state.target.pulse(w);
            let rates = watch.rates(w);
            let mut v = json!({
                "worker": w,
                "state": worker_state(p.state),
                "driver": p.driver.map(|d| d.name()),
                "counters": {
                    "ticks": p.ticks,
                    "polls": p.polls,
                    "parks": p.parks,
                    "idle_s": round(p.idle.as_secs_f64(), 3),
                    "spawned": p.spawned,
                },
                "last_second": rates.map(|r| json!({
                    "window_ms": r.window.as_millis() as u64,
                    "polls_per_s": r.polls_per_s.round(),
                    "ticks_per_s": r.ticks_per_s.round(),
                    "spawned_per_s": r.spawned_per_s.round(),
                    "busy": round(r.busy, 3),
                })),
                "blocked": blocked_json(watch.blocked(w, now), now),
            });
            let o = v.as_object_mut().expect("object");
            match snap {
                Ok(s) => {
                    o.insert("tasks".into(), json!(s.tasks));
                    o.insert("runnable".into(), json!(s.runnable));
                    o.insert("timers".into(), json!(s.timers));
                    o.insert("io_in_flight".into(), json!(s.io_in_flight));
                    o.insert(
                        "recv_buffers".into(),
                        json!({ "total": s.recv_buffers, "free": s.recv_buffers_free }),
                    );
                    o.insert("draining".into(), json!(s.draining));
                }
                Err(e) => {
                    o.insert("tasks".into(), json!(p.tasks));
                    o.insert("runnable".into(), json!(p.runnable));
                    o.insert("snapshot_error".into(), json!(e));
                }
            }
            v
        })
        .collect();
    json!({ "workers": list })
}

async fn tasks(state: &State, args: &Map<String, Value>) -> Result<Value, String> {
    check_args(args, &["worker", "limit"])?;
    let n = state.target.workers() as u64;
    let only = int_arg(args, "worker", None, 0, n.saturating_sub(1))?;
    let limit = int_arg(args, "limit", Some(20), 1, 500)?.unwrap_or(20) as usize;
    let snaps = snapshots(state).await;
    let mut by_location: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut workers = Vec::new();
    let mut total = 0;
    for (w, snap) in snaps.iter().enumerate() {
        if only.is_some_and(|o| o != w as u64) {
            continue;
        }
        match snap {
            Ok(s) => {
                total += s.tasks;
                workers.push(json!({ "worker": w, "tasks": s.tasks }));
                for &(l, count) in &s.tasks_by_location {
                    by_location
                        .entry(location(l))
                        .or_insert_with(|| vec![0; snaps.len()])[w] += count;
                }
            }
            Err(e) => workers.push(json!({ "worker": w, "error": e })),
        }
    }
    let mut rows: Vec<(String, Vec<usize>)> = by_location.into_iter().collect();
    rows.sort_by(|a, b| {
        let (sa, sb) = (a.1.iter().sum::<usize>(), b.1.iter().sum::<usize>());
        sb.cmp(&sa).then_with(|| a.0.cmp(&b.0))
    });
    let locations = rows.len();
    let rows: Vec<Value> = rows
        .into_iter()
        .take(limit)
        .map(|(l, per)| {
            let count: usize = per.iter().sum();
            match only {
                Some(_) => json!({ "location": l, "tasks": count }),
                None => json!({ "location": l, "tasks": count, "per_worker": per }),
            }
        })
        .collect();
    Ok(json!({
        "total_tasks": total,
        "locations": locations,
        "by_location": rows,
        "workers": workers,
    }))
}

/// What one source reported on one worker.
type Stats = Vec<(&'static str, u64)>;

async fn sources(state: &State) -> Value {
    let snaps = snapshots(state).await;
    let mut groups: BTreeMap<(String, String), Vec<(usize, Stats)>> = BTreeMap::new();
    let mut unanswered = Vec::new();
    for (w, snap) in snaps.into_iter().enumerate() {
        match snap {
            Ok(s) => {
                for src in s.sources {
                    groups
                        .entry((src.kind.to_owned(), src.name))
                        .or_default()
                        .push((w, src.stats));
                }
            }
            Err(e) => unanswered.push(json!({ "worker": w, "error": e })),
        }
    }
    let list: Vec<Value> = groups
        .into_iter()
        .map(|((kind, name), per)| {
            let mut sum: Vec<(&'static str, u64)> = Vec::new();
            for (_, stats) in &per {
                for &(k, v) in stats {
                    match sum.iter_mut().find(|(key, _)| *key == k) {
                        Some((_, total)) => *total = total.saturating_add(v),
                        None => sum.push((k, v)),
                    }
                }
            }
            let object = |stats: &[(&'static str, u64)]| {
                Value::Object(
                    stats
                        .iter()
                        .map(|&(k, v)| (k.to_owned(), json!(v)))
                        .collect(),
                )
            };
            json!({
                "kind": kind,
                "name": name,
                "total": object(&sum),
                "per_worker": per.iter().map(|(w, stats)| {
                    let mut o = object(stats);
                    o.as_object_mut().expect("object").insert("worker".into(), json!(w));
                    o
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut out = json!({ "sources": list, "unanswered": unanswered });
    if list_is_empty(&out) {
        out.as_object_mut().expect("object").insert(
            "note".into(),
            json!(
                "nothing is registered: giong-postgres, giong-mysql and giong-redis pools \
                   register themselves; other code can with giong::observe::register"
            ),
        );
    }
    out
}

fn list_is_empty(out: &Value) -> bool {
    out["sources"].as_array().is_some_and(Vec::is_empty)
}

/// Turns profiling off again however the call ends, including when the
/// client goes away mid-way.
struct ProfileGuard<'a>(&'a State);

impl Drop for ProfileGuard<'_> {
    fn drop(&mut self) {
        self.0.target.set_profiling(false);
        self.0.profiling.set(false);
    }
}

async fn profile(state: &State, args: &Map<String, Value>) -> Result<Value, String> {
    check_args(args, &["duration_ms", "limit"])?;
    let duration_ms = int_arg(args, "duration_ms", Some(1000), 100, 10_000)?.unwrap_or(1000);
    let limit = int_arg(args, "limit", Some(20), 1, 500)?.unwrap_or(20) as usize;
    if state.profiling.get() {
        return Err("a profile is already being taken; ask again when it is done".into());
    }
    if state.target.is_profiling() {
        return Err("profiling is already on, turned on by the service itself; \
                    giong-mcp leaves it alone"
            .into());
    }
    state.profiling.set(true);
    let guard = ProfileGuard(state);
    let window = Duration::from_millis(duration_ms);
    // Throw away whatever an earlier profile left behind.
    let _ = ask_all(state, || async { observe::take_profile() }).await;
    let before: Vec<_> = (0..state.target.workers())
        .map(|w| state.target.pulse(w))
        .collect();
    let start = Instant::now();
    state.target.set_profiling(true);
    giong::time::sleep(window).await;
    state.target.set_profiling(false);
    let elapsed = start.elapsed();
    let after: Vec<_> = (0..state.target.workers())
        .map(|w| state.target.pulse(w))
        .collect();
    let profiles = ask_all(state, || async { observe::take_profile() }).await;
    drop(guard);

    let secs = elapsed.as_secs_f64();
    let mut totals: BTreeMap<String, (u64, Duration, Duration)> = BTreeMap::new();
    let mut workers = Vec::new();
    for (w, result) in profiles.into_iter().enumerate() {
        let idle = after[w].idle.saturating_sub(before[w].idle);
        let busy = round((1.0 - idle.as_secs_f64() / secs).clamp(0.0, 1.0), 3);
        let list: Vec<PollProfile> = match result {
            Ok(list) => list.into_iter().filter(|p| !ours(p.location)).collect(),
            Err(e) => {
                workers.push(json!({
                    "worker": w,
                    "busy": busy,
                    "error": e,
                    "blocked_in": after[w].polling.map(location),
                }));
                continue;
            }
        };
        let in_tasks: Duration = list.iter().map(|p| p.total).sum();
        let polls: u64 = list.iter().map(|p| p.polls).sum();
        for p in &list {
            let e =
                totals
                    .entry(location(p.location))
                    .or_insert((0, Duration::ZERO, Duration::ZERO));
            e.0 += p.polls;
            e.1 += p.total;
            e.2 = e.2.max(p.max);
        }
        let top: Vec<Value> = list
            .iter()
            .take(limit)
            .map(|p| {
                json!({
                    "location": location(p.location),
                    "polls": p.polls,
                    "total_ms": ms(p.total),
                    "share": round(p.total.as_secs_f64() / secs, 3),
                    "mean_us": us(p.total / u32::try_from(p.polls.max(1)).unwrap_or(u32::MAX)),
                    "max_us": us(p.max),
                })
            })
            .collect();
        workers.push(json!({
            "worker": w,
            "busy": busy,
            "polls": polls,
            "in_tasks_ms": ms(in_tasks),
            "top": top,
        }));
    }
    let n = state.target.workers().max(1) as f64;
    let mut total: Vec<(String, (u64, Duration, Duration))> = totals.into_iter().collect();
    total.sort_by_key(|(_, (_, sum, _))| std::cmp::Reverse(*sum));
    let total: Vec<Value> = total
        .into_iter()
        .take(limit)
        .map(|(l, (polls, sum, max))| {
            json!({
                "location": l,
                "polls": polls,
                "total_ms": ms(sum),
                "share_of_all_workers": round(sum.as_secs_f64() / (secs * n), 3),
                "max_us": us(max),
            })
        })
        .collect();
    Ok(json!({
        "duration_ms": ms(elapsed),
        "total": total,
        "workers": workers,
        "note": "share: the part of the window a worker spent polling tasks spawned at that \
                 location. busy: the part it was awake; the rest of the awake time went to \
                 the kernel (I/O) and to the runtime itself.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_dropped_mid_window_turns_profiling_off() {
        let rt = giong::Builder::new().workers(1).build().unwrap();
        let target = rt.handle();
        let service =
            std::thread::spawn(move || rt.run(|_| async { giong::shutdown_signal().await }));
        let state = State::new(target.clone(), Duration::from_millis(500));
        giong::block_on(async {
            let args = json!({ "duration_ms": 5000 }).as_object().unwrap().clone();
            {
                let mut call = std::pin::pin!(profile(&state, &args));
                let waited = giong::time::timeout(Duration::from_millis(200), call.as_mut()).await;
                assert!(waited.is_err());
                assert!(target.is_profiling(), "on during the window");
                assert!(state.profiling.get());
            }
            // The client went away: the call is dropped half-way.
            assert!(!target.is_profiling());
            assert!(!state.profiling.get());
        });
        target.shutdown();
        service.join().unwrap().unwrap();
    }
}
