//! `slow_requests`: why requests are slow, from the runtime's request
//! tracing (see `giong::observe`, "Why a request is slow").
//!
//! Every number is measured in the window the tool traces; the verdicts are
//! rules applied to those numbers, stated with the numbers they rest on, so
//! that an agent can check them. Nothing here changes the service: the
//! suggestions are for the agent, or a person, to weigh.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use giong::observe::{self, Breakdown, PollProfile, RequestRecord, RouteStats};
use serde_json::{json, Map, Value};

use crate::tools::{
    ask_all, check_args, http_connection, int_arg, location, ms, ours, round, snapshots, State,
};

/// Tail requests needed for a verdict of high confidence.
const ENOUGH: u64 = 20;
/// Below this mean, the slowest requests of a route are not worth a
/// verdict: the phases are then mostly the runtime's and the network's own
/// noise.
const NOT_SLOW: Duration = Duration::from_millis(1);

/// Turns request tracing back off and frees the tool if the call is
/// dropped half-way.
struct TraceGuard<'a>(&'a State);

impl Drop for TraceGuard<'_> {
    fn drop(&mut self) {
        self.0.target.set_request_tracing(false);
        self.0.tracing.set(false);
    }
}

/// The phases a request's time is split into, as they appear in answers,
/// with how to read each one.
const PHASES: [(&str, &str); 7] = [
    ("read", "receiving the request until its handler was called"),
    ("poll", "the handler's own code running on the worker"),
    ("pool", "waiting for a connection from a pool"),
    ("query", "waiting for database or cache commands"),
    ("upstream", "waiting for other services"),
    ("other", "handler time no code named: timers, locks, channels, spawned tasks, untimed clients, or runnable while the worker ran other tasks"),
    ("write", "writing the answer to the client"),
];

fn phases(b: &Breakdown) -> [Duration; 7] {
    [
        b.read, b.poll, b.pool, b.query, b.upstream, b.other, b.write,
    ]
}

fn mean(d: Duration, n: u64) -> Duration {
    if n == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(d.as_secs_f64() / n as f64)
    }
}

pub(crate) async fn slow_requests(
    state: &State,
    args: &Map<String, Value>,
) -> Result<Value, String> {
    check_args(args, &["duration_ms", "route", "quantile", "limit"])?;
    let duration_ms = int_arg(args, "duration_ms", Some(5000), 100, 30_000)?.unwrap_or(5000);
    let limit = int_arg(args, "limit", Some(10), 1, 100)?.unwrap_or(10) as usize;
    let quantile = match args.get("quantile") {
        None | Some(Value::Null) => 0.99,
        Some(v) => match v.as_f64() {
            Some(q) if (0.5..=0.9999).contains(&q) => q,
            _ => {
                return Err(format!(
                    "quantile must be a number from 0.5 to 0.9999, not {v}"
                ))
            }
        },
    };
    let route_filter = match args.get("route") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 512 => Some(s.clone()),
        Some(v) => return Err(format!("route must be a non-empty string, not {v}")),
    };
    if state.tracing.get() {
        return Err("slow_requests is already running; ask again when it is done".into());
    }
    let target = &state.target;
    if target.is_request_tracing() {
        return Err(
            "request tracing is already on, turned on by the service itself; \
                    giong-mcp leaves it alone"
                .into(),
        );
    }
    state.tracing.set(true);
    let guard = TraceGuard(state);
    let window = Duration::from_millis(duration_ms);
    // Throw away whatever an earlier window left behind.
    let _ = ask_all(state, || async { observe::take_requests() }).await;
    let before: Vec<_> = (0..target.workers()).map(|w| target.pulse(w)).collect();
    let start = Instant::now();
    target.set_request_tracing(true);
    giong::time::sleep(window).await;
    target.set_request_tracing(false);
    let elapsed = start.elapsed();
    let after: Vec<_> = (0..target.workers()).map(|w| target.pulse(w)).collect();
    let reports = ask_all(state, || async { observe::take_requests() }).await;
    let snaps = snapshots(state).await;
    drop(guard);

    let secs = elapsed.as_secs_f64();
    let busy: Vec<f64> = (0..before.len())
        .map(|w| {
            let idle = after[w].idle.saturating_sub(before[w].idle);
            round((1.0 - idle.as_secs_f64() / secs).clamp(0.0, 1.0), 3)
        })
        .collect();

    let mut routes: BTreeMap<String, RouteStats> = BTreeMap::new();
    let mut long_polls: BTreeMap<String, (u64, Duration, Duration, bool)> = BTreeMap::new();
    let mut unanswered = Vec::new();
    for (w, report) in reports.into_iter().enumerate() {
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                unanswered.push(json!({ "worker": w, "error": e }));
                continue;
            }
        };
        for r in report.routes {
            let name = route_name(&r);
            match routes.get_mut(&name) {
                Some(all) => all.merge(&r),
                None => {
                    routes.insert(name, r);
                }
            }
        }
        for p in report.long_polls.iter().filter(|p| !ours(p.location)) {
            add_long_poll(&mut long_polls, p);
        }
    }

    // Pools as their sources report them at the end of the window.
    let mut pools: Vec<Value> = Vec::new();
    for (w, snap) in snaps.into_iter().enumerate() {
        let Ok(snap) = snap else { continue };
        for src in snap
            .sources
            .into_iter()
            .filter(|s| s.kind.ends_with("pool"))
        {
            let mut o = Map::new();
            o.insert("kind".into(), json!(src.kind));
            o.insert("name".into(), json!(src.name));
            o.insert("worker".into(), json!(w));
            for (k, v) in src.stats {
                o.insert(k.into(), json!(v));
            }
            pools.push(Value::Object(o));
        }
    }

    let total_requests: u64 = routes.values().map(|r| r.requests).sum();
    let mut list: Vec<(String, RouteStats)> = routes
        .into_iter()
        .filter(|(name, _)| {
            route_filter
                .as_ref()
                .map_or(true, |f| name.contains(f.as_str()))
        })
        .collect();
    // The routes whose tail costs most first: requests past the quantile
    // times their mean.
    list.sort_by(|a, b| {
        let weight = |r: &RouteStats| {
            let t = r.breakdown(quantile, 1.0);
            t.total.as_secs_f64()
        };
        weight(&b.1).total_cmp(&weight(&a.1))
    });
    let shown = list.len().min(limit);
    let analysed: Vec<Analysis> = list
        .iter()
        .take(limit)
        .map(|(name, r)| analyse(name, r, quantile, secs, &busy, &pools))
        .collect();

    let mut lp: Vec<(String, (u64, Duration, Duration, bool))> = long_polls.into_iter().collect();
    lp.sort_by_key(|(_, (_, total, _, _))| std::cmp::Reverse(*total));
    let long_polls: Vec<Value> = lp
        .into_iter()
        .take(10)
        .map(|(l, (polls, total, max, http))| {
            json!({
                "location": l,
                "polls": polls,
                "total_ms": ms(total),
                "max_ms": ms(max),
                "giong_http_connection": http,
            })
        })
        .collect();

    let summary = if analysed.is_empty() {
        if total_requests == 0 {
            "no request finished during the window. Either none arrived, or the service does \
             not serve them through giong-http (only its servers trace requests); a request \
             still in flight when the window ends is not counted, so ask for a longer window \
             if requests take longer than it"
                .to_owned()
        } else {
            format!(
                "{total_requests} requests finished, none on a route matching {:?}",
                route_filter.unwrap_or_default()
            )
        }
    } else {
        analysed
            .iter()
            .take(3)
            .map(|a| a.headline.clone())
            .collect::<Vec<_>>()
            .join(" ")
    };

    Ok(json!({
        "duration_ms": ms(elapsed),
        "quantile": quantile,
        "summary": summary,
        "requests": total_requests,
        "routes": analysed.into_iter().map(|a| a.json).collect::<Vec<_>>(),
        "routes_not_shown": list.len() - shown,
        "workers": busy.iter().enumerate().map(|(w, b)| json!({ "worker": w, "busy": b })).collect::<Vec<_>>(),
        "long_polls": long_polls,
        "pools": pools,
        "unanswered": unanswered,
        "phases": PHASES.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>(),
        "note": "Measured: every request that finished during the window, on routes named by \
                 giong-web (or giong::observe::set_route); requests no code named are under \
                 \"(no route)\". Latencies are bucketed to within a quarter. The cause is the \
                 phase that accounts for most of the difference between the slow requests and \
                 the faster half (share_of_excess), or of the slow requests' time when the two \
                 do not differ; blocked is time the worker spent in other tasks' polls of 1 ms \
                 or more while the requests were in flight. HTTP/2 requests are timed from \
                 their handler's start to their answer, without read and write. Suggestions \
                 are proposals: check them against the evidence before changing anything.",
    }))
}

fn route_name(r: &RouteStats) -> String {
    r.route
        .as_ref()
        .map_or_else(|| "(no route)".to_owned(), |r| r.as_str().to_owned())
}

fn add_long_poll(into: &mut BTreeMap<String, (u64, Duration, Duration, bool)>, p: &PollProfile) {
    let e = into.entry(location(p.location)).or_insert((
        0,
        Duration::ZERO,
        Duration::ZERO,
        http_connection(p.location),
    ));
    e.0 += p.polls;
    e.1 += p.total;
    e.2 = e.2.max(p.max);
}

struct Analysis {
    headline: String,
    json: Value,
}

/// Whether other tasks' long polls, explaining `blocked` of the slow
/// requests' extra time, explain it better than the dominant phase does
/// with `phase`. Both are shares of the same quantity: blocked time is
/// part of a phase (the request is suspended while the worker runs other
/// tasks), so it cannot exceed the phase it sits in.
fn blocked_wins(blocked: f64, phase: f64) -> bool {
    blocked >= 0.3 && blocked >= phase * 0.8
}

/// What the slow requests of one route spent their time on, and why.
fn analyse(
    name: &str,
    r: &RouteStats,
    quantile: f64,
    secs: f64,
    busy: &[f64],
    pools: &[Value],
) -> Analysis {
    let tail = r.breakdown(quantile, 1.0);
    let fast = r.breakdown(0.0, 0.5);
    let n = tail.requests;
    let tail_total = mean(tail.total, n);
    let fast_total = mean(fast.total, fast.requests);
    let tp = phases(&tail);
    let fp = phases(&fast);
    let tail_means: Vec<Duration> = tp.iter().map(|d| mean(*d, n)).collect();
    let fast_means: Vec<Duration> = fp.iter().map(|d| mean(*d, fast.requests)).collect();
    let blocked = mean(tail.blocked, n);
    let fast_blocked = mean(fast.blocked, fast.requests);

    // Which phase makes the slow requests slow: the one that grows most
    // from the faster half to the tail, when the tail stands out; else the
    // largest one.
    let excess = tail_total.saturating_sub(fast_total);
    let distinct_tail =
        fast.requests > 0 && tail_total.as_secs_f64() >= 1.5 * fast_total.as_secs_f64();
    let (dominant, share_of_excess) = if distinct_tail && !excess.is_zero() {
        let (i, grow) = tail_means
            .iter()
            .zip(&fast_means)
            .map(|(t, f)| t.saturating_sub(*f))
            .enumerate()
            .max_by_key(|(_, g)| *g)
            .unwrap_or((0, Duration::ZERO));
        (i, Some(grow.as_secs_f64() / excess.as_secs_f64()))
    } else {
        let (i, _) = tail_means
            .iter()
            .enumerate()
            .max_by_key(|(_, d)| **d)
            .unwrap_or((0, &Duration::ZERO));
        (i, None)
    };
    let share_of_tail = if tail_total.is_zero() {
        0.0
    } else {
        tail_means[dominant].as_secs_f64() / tail_total.as_secs_f64()
    };
    let blocked_share = if tail_total.is_zero() {
        0.0
    } else {
        blocked.as_secs_f64() / tail_total.as_secs_f64()
    };

    let workers: Vec<usize> = {
        let mut w: Vec<usize> = r.slowest.iter().map(|s| s.worker).collect();
        w.sort_unstable();
        w.dedup();
        w
    };
    let max_busy = workers
        .iter()
        .filter_map(|&w| busy.get(w).copied())
        .fold(0.0f64, f64::max);

    let phase = PHASES[dominant].0;
    let mut cause = match phase {
        "read" => "slow_request_arrival",
        "poll" => "handler_cpu",
        "pool" => "pool_wait",
        "query" => "slow_queries",
        "upstream" => "slow_upstream",
        "write" => "slow_response_write",
        _ => "unattributed_wait",
    };
    let mut strength = share_of_excess.unwrap_or(share_of_tail);
    // Measured like the dominant phase: the share of the excess when the
    // tail stands out, else the share of the tail's time.
    let blocked_strength = match share_of_excess {
        // At most all of it: blocked time can grow more than the total
        // when another phase shrank.
        Some(_) => {
            (blocked.saturating_sub(fast_blocked).as_secs_f64() / excess.as_secs_f64()).min(1.0)
        }
        None => blocked_share,
    };
    if blocked_wins(blocked_strength, strength) {
        cause = "worker_blocked";
        strength = blocked_strength;
    } else if cause == "unattributed_wait" && max_busy >= 0.85 {
        cause = "worker_saturated";
    }
    let not_slow = tail_total < NOT_SLOW;
    if not_slow {
        cause = "not_slow";
    }
    let confidence = if not_slow {
        if n >= 5 {
            "high"
        } else {
            "low"
        }
    } else if n >= ENOUGH && strength >= 0.6 {
        "high"
    } else if n >= 5 && strength >= 0.4 {
        "medium"
    } else {
        "low"
    };

    let pct = |x: f64| format!("{:.0}%", x * 100.0);
    let fmt_ms = |d: Duration| {
        let v = d.as_secs_f64() * 1e3;
        if v >= 100.0 {
            format!("{v:.0} ms")
        } else if v >= 1.0 {
            format!("{v:.1} ms")
        } else {
            format!("{:.0} µs", v * 1e3)
        }
    };
    let tail_label = format!(
        "the requests from p{} up ({n} requests, from about {})",
        quantile_label(quantile),
        fmt_ms(lower_edge(r, quantile))
    );

    let mut evidence = Vec::new();
    evidence.push(format!(
        "{tail_label} took {} on average; the faster half took {}",
        fmt_ms(tail_total),
        fmt_ms(fast_total)
    ));
    evidence.push(format!(
        "{} ({}): {} on average, {} of their time{}",
        phase_name(phase),
        PHASES[dominant].1,
        fmt_ms(tail_means[dominant]),
        pct(share_of_tail),
        match share_of_excess {
            Some(s) => format!(
                "; {} in the faster half, so it accounts for {} of the difference",
                fmt_ms(fast_means[dominant]),
                pct(s)
            ),
            None => "; the slow requests do not stand out from the rest: requests are \
                     uniformly this slow"
                .to_owned(),
        }
    ));
    let what = |pick: fn(&RequestRecord) -> Option<&'static str>| -> Option<&'static str> {
        let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        for s in &r.slowest {
            if let Some(w) = pick(s) {
                *counts.entry(w).or_default() += 1;
            }
        }
        counts.into_iter().max_by_key(|(_, c)| *c).map(|(w, _)| w)
    };
    let system = match phase {
        "pool" => what(|s| s.pool.what),
        "query" => what(|s| s.query.what),
        "upstream" => what(|s| s.upstream.what),
        _ => None,
    };
    if let Some(system) = system {
        evidence.push(format!("the longest of those waits were for {system}"));
    }
    if blocked > Duration::ZERO {
        evidence.push(format!(
            "while they were in flight, their worker spent {} on average ({}) in other tasks' \
             long polls",
            fmt_ms(blocked),
            pct(blocked_share)
        ));
    }
    let culprit = r
        .slowest
        .iter()
        .filter_map(|s| s.blocked_by.as_ref())
        .max_by_key(|b| b.held);
    if let Some(b) = culprit {
        let whose = match &b.route {
            Some(route) => format!("the handler of a {route} request"),
            None if http_connection(b.location) => {
                "a giong-http connection's task (a request handler no route named, or TLS work)"
                    .to_owned()
            }
            None => "a task".to_owned(),
        };
        evidence.push(format!(
            "the worst hold: {whose}, spawned at {}, held the worker for {} of one slow request",
            location(b.location),
            fmt_ms(b.held)
        ));
    }
    if !workers.is_empty() {
        evidence.push(format!(
            "the slowest requests ran on worker{} {}, busy {} of the window at most",
            if workers.len() == 1 { "" } else { "s" },
            workers
                .iter()
                .map(|w| w.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            pct(max_busy)
        ));
    }
    let mut pool_lines = Vec::new();
    if cause == "pool_wait" {
        for p in pools {
            let kind = p["kind"].as_str().unwrap_or("");
            if system.map_or(true, |s| kind.starts_with(s)) {
                let n = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
                pool_lines.push(format!(
                    "{kind} {} on worker {} at the end of the window: {} of {} connections \
                     open, {} taken with Pool::get, {} commands in flight, {} tasks waiting, \
                     {} timeouts so far",
                    p["name"].as_str().unwrap_or("?"),
                    p["worker"],
                    n("connections"),
                    n("max_size"),
                    n("taken"),
                    n("in_flight"),
                    n("waiting"),
                    n("timeouts"),
                ));
            }
        }
        evidence.extend(pool_lines.iter().take(4).cloned());
    }
    if n < ENOUGH {
        evidence.push(format!(
            "only {n} requests are in the tail: trace for longer, or under more load, for a \
             surer answer"
        ));
    }

    let suggestion = suggestion(
        cause,
        system,
        culprit.map(|b| (b.location, b.route.clone())),
    );
    let headline = format!(
        "{name}: p{} {} over {} requests; {} ({} confidence).",
        quantile_label(quantile),
        fmt_ms(r.quantile(quantile)),
        r.requests,
        match cause {
            "not_slow" =>
                "nothing to explain: even its slowest requests take under 1 ms".to_owned(),
            "worker_blocked" => format!(
                "the slow requests waited while other tasks held their worker ({} of their time)",
                pct(blocked_share)
            ),
            "worker_saturated" => "the slow requests waited for a saturated worker".to_owned(),
            _ => format!(
                "{} of the slow requests' time is {}",
                pct(share_of_tail),
                PHASES[dominant].1
            ),
        },
        confidence
    );

    let breakdown_json = |means: &[Duration], total: Duration, blocked: Duration, count: u64| {
        let mut o = Map::new();
        o.insert("requests".into(), json!(count));
        o.insert("total_ms".into(), json!(ms(total)));
        for ((k, _), d) in PHASES.iter().zip(means) {
            o.insert(format!("{k}_ms"), json!(ms(*d)));
        }
        o.insert("blocked_ms".into(), json!(ms(blocked)));
        Value::Object(o)
    };
    let slowest: Vec<Value> = r.slowest.iter().take(3).map(record_json).collect();
    let json = json!({
        "route": name,
        "requests": r.requests,
        "per_second": round(r.requests as f64 / secs.max(1e-9), 1),
        "abandoned": r.abandoned,
        "p50_ms": ms(r.quantile(0.5)),
        "p90_ms": ms(r.quantile(0.9)),
        "p99_ms": ms(r.quantile(0.99)),
        "max_ms": ms(r.max),
        "tail": breakdown_json(&tail_means, tail_total, blocked, n),
        "faster_half": breakdown_json(&fast_means, fast_total, mean(fast.blocked, fast.requests), fast.requests),
        "verdict": {
            "cause": cause,
            "phase": phase,
            "confidence": confidence,
            "share_of_tail": round(share_of_tail, 3),
            "share_of_excess": share_of_excess.map(|s| round(s, 3)),
            "blocked_share": round(blocked_share, 3),
            "evidence": evidence,
            "suggestion": suggestion,
        },
        "slowest": slowest,
    });
    Analysis { headline, json }
}

/// The lower edge of the latencies in the tail, for the evidence line.
fn lower_edge(r: &RouteStats, quantile: f64) -> Duration {
    // The upper edge of the quantile's bucket, divided back by the bucket
    // width (a quarter): close to where the tail starts.
    let top = r.quantile(quantile);
    Duration::from_secs_f64(top.as_secs_f64() * 0.8).min(r.max)
}

/// How a phase is named in evidence lines.
fn phase_name(phase: &str) -> &'static str {
    match phase {
        "read" => "Receiving the request",
        "poll" => "The handler's own code",
        "pool" => "Waiting for pool connections",
        "query" => "Waiting for database commands",
        "upstream" => "Waiting for other services",
        "write" => "Writing the answer",
        _ => "Unnamed waits in the handler",
    }
}

fn quantile_label(q: f64) -> String {
    let p = q * 100.0;
    if (p - p.round()).abs() < 1e-9 {
        format!("{p:.0}")
    } else {
        let s = format!("{p:.2}");
        s.trim_end_matches('0').to_owned()
    }
}

fn record_json(s: &RequestRecord) -> Value {
    let wait = |w: &observe::WaitStats| {
        json!({
            "total_ms": ms(w.total),
            "count": w.count,
            "longest_ms": ms(w.longest),
            "what": w.what,
        })
    };
    json!({
        "worker": s.worker,
        "protocol": s.protocol,
        "status": s.status,
        "ago_ms": ms(s.ago),
        "total_ms": ms(s.total),
        "read_ms": ms(s.read),
        "poll_ms": ms(s.poll),
        "polls": s.polls,
        "longest_poll_ms": ms(s.longest_poll),
        "pool": wait(&s.pool),
        "query": wait(&s.query),
        "upstream": wait(&s.upstream),
        "other_ms": ms(s.other),
        "write_ms": ms(s.write),
        "blocked_ms": ms(s.blocked),
        "blocked_by": s.blocked_by.as_ref().map(|b| json!({
            "location": location(b.location),
            "route": b.route.as_ref().map(|r| r.as_str().to_owned()),
            "held_ms": ms(b.held),
        })),
    })
}

fn suggestion(
    cause: &str,
    system: Option<&str>,
    culprit: Option<(observe::Location, Option<observe::Route>)>,
) -> String {
    let system = system.unwrap_or("the");
    match cause {
        "pool_wait" => format!(
            "Requests queue for a {system} pool connection: every connection is in use when \
             they ask. The database itself is not what is slow here. Check whether the \
             database has headroom (CPU, its connection limit); if it does, raise the pool's \
             max_size. Otherwise shorten what holds connections (long transactions, \
             connections from Pool::get kept across other awaits) or limit how many requests \
             run at once. The sources tool shows the pool's taken and waiting counts live."
        ),
        "slow_queries" => format!(
            "The {system} server answers slowly: the time is in the commands themselves, not \
             in giong or the pool. Look at the server's slow query log, EXPLAIN the statements \
             of this route, and check for missing indexes and lock waits. Tuning the runtime \
             will not help."
        ),
        "slow_upstream" => format!(
            "A call to another service ({system}) is slow. Look at that service's latency; \
             consider a timeout, a cache, or calling it concurrently with other work."
        ),
        "handler_cpu" => "The handler's own code takes the time, on the I/O worker: while it \
             runs, every other request on that worker waits. Find the hot code with the \
             profile tool; move heavy computation off the I/O worker, or make it cheaper."
            .to_owned(),
        "worker_blocked" => match culprit {
            Some((l, Some(route))) => format!(
                "Requests to {route} hold the worker in long polls (their handler spawned at \
                 {}), and requests on the same worker wait behind them. Make that handler \
                 await instead of blocking (a blocking call, or a long computation without \
                 .await), or move its work off the I/O worker.",
                location(l)
            ),
            Some((l, None)) => format!(
                "The task spawned at {} holds the worker in long polls, and requests on the \
                 same worker wait behind it. Make it await instead of blocking (a blocking \
                 call, or a long computation without .await), or move its work off the I/O \
                 worker.",
                location(l)
            ),
            None => "Other tasks hold the worker in long polls; see long_polls for where they \
                     were spawned, and make them await instead of blocking."
                .to_owned(),
        },
        "not_slow" => "Nothing to do: the slowest requests of this route take under 1 ms on \
             average."
            .to_owned(),
        "worker_saturated" => "The worker is busy nearly all the time, so ready requests wait \
             their turn. Give the service more workers or CPUs, or spread connections more \
             evenly (bind_balanced); the profile tool shows where the CPU goes."
            .to_owned(),
        "slow_request_arrival" => "Requests take long to arrive: slow clients or networks, or \
             large uploads. Nothing on the server makes them faster; a lower request_timeout \
             bounds what slow clients can hold."
            .to_owned(),
        "slow_response_write" => "Answers take long to write: slow readers, slow networks, or \
             large answers. Compression or smaller answers help; a lower write_timeout bounds \
             what slow readers can hold."
            .to_owned(),
        _ => "The handler waits on something giong does not time: a timer, a lock, a channel, a \
              task it spawned, or a client that does not report its waits. Wrap such waits \
              in giong::observe::wait so that the next trace names them."
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS CI, #181: blocked time explained 91% of the excess (6.2 of
    /// 6.8 ms) and the suspended phase 99.9%. Compared with blocked time's
    /// share of the whole tail (77%) instead, it lost to the phase it is
    /// part of, and the verdict blamed unnamed waits.
    #[test]
    fn blocked_time_is_weighed_like_the_phase_it_sits_in() {
        assert!(blocked_wins(0.91, 0.999));
        assert!(!blocked_wins(0.768 * 0.9, 0.999));
        assert!(!blocked_wins(0.25, 0.3));
        assert!(blocked_wins(0.5, 0.6));
    }

    #[test]
    fn quantile_labels() {
        assert_eq!(quantile_label(0.99), "99");
        assert_eq!(quantile_label(0.999), "99.9");
        assert_eq!(quantile_label(0.5), "50");
    }
}
