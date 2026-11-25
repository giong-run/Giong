//! `giong bench` against servers that misbehave in the ways real ones do:
//! a stall, `Connection: close`, chunked bodies, big bodies, silence,
//! garbage. The numbers it prints are what people paste into PRs and K's
//! no-regression rule rests on, so a wrong number is a bug even when the
//! command "works". The builder's own tests (tests/bench.rs) cover the happy
//! paths and `--against`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::harness::*;

/// How a test server answers one request; `n` counts requests from 0.
type Answer = dyn Fn(u64, &mut TcpStream) -> std::io::Result<bool> + Send + Sync;

/// A keep-alive HTTP/1.1 server on a free loopback port. `answer` writes the
/// response and returns whether to keep the connection.
fn serve(answer: Arc<Answer>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let count = Arc::new(AtomicU64::new(0));
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let (answer, count) = (answer.clone(), count.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut got = Vec::new();
                loop {
                    while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got.extend_from_slice(&buf[..n]),
                        }
                    }
                    let end = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    got.drain(..end);
                    let n = count.fetch_add(1, Ordering::Relaxed);
                    match answer(n, &mut s) {
                        Ok(true) => {}
                        _ => return,
                    }
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

fn bench(url: &str, extra: &[&str], timeout: Duration) -> (Out, Value) {
    let dir = TempDir::new("bench");
    let mut run = giong(["bench", url]);
    for a in extra {
        run = run.arg(*a);
    }
    let out = run.arg("--json").dir(dir.path()).timeout(timeout).run();
    let v = out.report("bench");
    (out, v)
}

fn round(v: &Value) -> &Value {
    &v["data"]["rounds"][0]
}

fn us(v: &Value, k: &str) -> f64 {
    v[k].as_f64()
        .unwrap_or_else(|| panic!("no number {k} in {v}"))
}

/// The point of a fixed rate: a one-second stall is charged to every
/// request that was due during it, not only to the one that hit it.
#[test]
fn a_stall_is_charged_to_every_request_that_waited() {
    if !has_command("bench") {
        return;
    }
    let started = Arc::new(std::sync::OnceLock::<Instant>::new());
    let url = serve(Arc::new(move |_, s: &mut TcpStream| {
        let t0 = *started.get_or_init(Instant::now);
        // Once, about a second in, every connection stalls for a second.
        let e = t0.elapsed();
        if e > Duration::from_millis(1000) && e < Duration::from_millis(1100) {
            std::thread::sleep(Duration::from_millis(1000));
        }
        s.write_all(OK).map(|()| true)
    }));
    let (out, v) = bench(
        &url,
        &[
            "--rate",
            "200",
            "--conns",
            "8",
            "--duration",
            "3",
            "--warmup",
            "0",
        ],
        Duration::from_secs(60),
    );
    let r = round(&v);
    // About 200 requests were due in the stall: well over 1% of 600, so the
    // p99 must show it. A tool that measures from send time reports ~0.1 ms.
    assert!(
        us(r, "p99_us") >= 300_000.0,
        "p99 {} µs hides a 1 s stall\n{}",
        r["p99_us"],
        out.dump()
    );
    assert!(
        r["answered"].as_u64().unwrap() >= 550,
        "requests due in the stall were dropped from the count\n{}",
        out.dump()
    );
}

#[test]
fn a_fixed_delay_is_measured_as_that_delay() {
    if !has_command("bench") {
        return;
    }
    // The handler times its own sleep: on a starved machine the 20 ms sleep
    // itself overruns, and then no latency bound can judge giong bench.
    let sleeps_us = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let sleeps = Arc::clone(&sleeps_us);
    let url = serve(Arc::new(move |_, s: &mut TcpStream| {
        let t = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        sleeps.lock().unwrap().push(t.elapsed().as_micros() as u64);
        s.write_all(OK).map(|()| true)
    }));
    let (out, v) = bench(
        &url,
        &[
            "--rate",
            "50",
            "--conns",
            "4",
            "--duration",
            "2",
            "--warmup",
            "0.2",
        ],
        Duration::from_secs(60),
    );
    let r = round(&v);
    let p50 = us(r, "p50_us");
    assert!(
        p50 >= 19_000.0,
        "p50 {p50} µs for a 20 ms handler\n{}",
        out.dump()
    );
    assert_eq!(r["failed"], 0, "{}", out.dump());
    // The upper bound needs a machine that keeps time: none of the handler's
    // sleeps took twice as long as asked (past that, connections fall behind
    // their schedule and latency from the due time rightly grows), and giong
    // bench did not report its own generator late (GIONG-E5062).
    let mut sleeps = sleeps_us.lock().unwrap().clone();
    sleeps.sort_unstable();
    let slowest = sleeps.last().copied().unwrap_or(0);
    let median = sleeps.get(sleeps.len() / 2).copied().unwrap_or(0);
    let lagged = v["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|f| f["code"] == "GIONG-E5062");
    let timing = format!(
        "the handler's 20 ms sleeps: median {median} µs, slowest {slowest} µs, {} of them; \
         generator lag reported: {lagged}",
        sleeps.len()
    );
    if lagged || slowest >= 40_000 {
        eprintln!(
            "a_fixed_delay_is_measured_as_that_delay: this machine does not keep time \
             ({timing}); upper bound not checked (p50 {p50} µs)"
        );
        return;
    }
    assert!(
        p50 < 60_000.0,
        "p50 {p50} µs for a 20 ms handler ({timing})\n{}",
        out.dump()
    );
}

#[test]
fn connection_close_answers_are_not_failures() {
    if !has_command("bench") {
        return;
    }
    let url = serve(Arc::new(|_, s: &mut TcpStream| {
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")?;
        Ok(false)
    }));
    let (out, v) = bench(
        &url,
        &[
            "--rate",
            "200",
            "--conns",
            "4",
            "--duration",
            "1",
            "--warmup",
            "0",
        ],
        Duration::from_secs(60),
    );
    let r = round(&v);
    assert_eq!(r["failed"], 0, "{}", out.dump());
    assert!(r["answered"].as_u64().unwrap() >= 190, "{}", out.dump());
}

#[test]
fn chunked_and_big_bodies_are_read_whole() {
    if !has_command("bench") {
        return;
    }
    let chunked = serve(Arc::new(|_, s: &mut TcpStream| {
        s.write_all(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .map(|()| true)
    }));
    let big_body = Arc::new(vec![b'x'; 1 << 20]);
    let big = serve(Arc::new(move |_, s: &mut TcpStream| {
        s.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                big_body.len()
            )
            .as_bytes(),
        )?;
        s.write_all(&big_body).map(|()| true)
    }));
    for url in [chunked, big] {
        let (out, v) = bench(
            &url,
            &[
                "--rate",
                "100",
                "--conns",
                "2",
                "--duration",
                "1",
                "--warmup",
                "0",
            ],
            Duration::from_secs(60),
        );
        let r = round(&v);
        assert_eq!(r["failed"], 0, "{url}\n{}", out.dump());
        assert!(
            r["answered"].as_u64().unwrap() >= 95,
            "{url}\n{}",
            out.dump()
        );
    }
}

/// A server that accepts and never answers must end the run with failures
/// (or a coded failure), within the duration plus a bounded wait: the
/// loadgen promises "warmup plus duration, plus up to `timeout`".
#[test]
fn a_silent_server_does_not_hang_the_run() {
    if !has_command("bench") {
        return;
    }
    let url = serve(Arc::new(|_, _s: &mut TcpStream| {
        std::thread::sleep(Duration::from_secs(3600));
        Ok(false)
    }));
    let (out, v) = bench(
        &url,
        &[
            "--rate",
            "50",
            "--conns",
            "2",
            "--duration",
            "1",
            "--warmup",
            "0",
        ],
        Duration::from_secs(90),
    );
    assert_ne!(
        out.code(),
        0,
        "a server that never answers measured fine\n{}",
        out.dump()
    );
    assert!(
        v.get("error").is_some()
            || v["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["severity"] == "error"),
        "{}",
        out.dump()
    );
}

#[test]
fn garbage_answers_are_failures_not_a_crash() {
    if !has_command("bench") {
        return;
    }
    let url = serve(Arc::new(|_, s: &mut TcpStream| {
        s.write_all(b"this is not http\n")?;
        Ok(false)
    }));
    let (out, _v) = bench(
        &url,
        &[
            "--rate",
            "50",
            "--conns",
            "2",
            "--duration",
            "1",
            "--warmup",
            "0",
        ],
        Duration::from_secs(60),
    );
    assert_ne!(out.code(), 0, "{}", out.dump());
    assert!(!out.stderr.contains("panicked"), "{}", out.dump());
}

#[test]
fn an_unreachable_url_fails_fast_with_a_code() {
    if !has_command("bench") {
        return;
    }
    // A port nothing listens on. On Unix it stays bound without listening
    // for the whole test, so a connection is refused and no other test of
    // this run gets the same port from bind(0) and serves on it (one did:
    // giong dev's tests free and bind their service's port again).
    #[cfg(unix)]
    let held = {
        use socket2::{Domain, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        s.bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        s
    };
    #[cfg(unix)]
    let port = held.local_addr().unwrap().as_socket().unwrap().port();
    #[cfg(not(unix))]
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/");
    let (out, v) = bench(&url, &["--duration", "1"], Duration::from_secs(30));
    out.expect_code(2);
    assert_eq!(v["error"]["code"], "GIONG-E5060", "{}", out.dump());
    assert!(
        out.elapsed < Duration::from_secs(15),
        "took {:?}",
        out.elapsed
    );
    #[cfg(unix)]
    drop(held);
}

/// Values that make no sense are usage errors, before any load is sent; a
/// value that is accepted must not make the run endless.
#[test]
fn nonsense_flag_values_are_refused() {
    if !has_command("bench") {
        return;
    }
    let url = serve(Arc::new(|_, s: &mut TcpStream| {
        s.write_all(OK).map(|()| true)
    }));
    for bad in [
        // `--rate 0` means max, on purpose (a unit test of bench.rs says so).
        &["--rate", "-5"][..],
        &["--rate", "NaN"],
        &["--rate", "inf"],
        &["--duration", "0"],
        &["--duration", "-1"],
        &["--duration", "inf"],
        &["--duration", "1e300"],
        &["--warmup", "-1"],
        &["--warmup", "NaN"],
        &["--conns", "0"],
        &["--conns", "-1"],
        &["--conns", "99999999"],
    ] {
        let (out, v) = bench(&url, bad, Duration::from_secs(20));
        assert_eq!(out.code(), 2, "{bad:?} was accepted\n{}", out.dump());
        assert_eq!(v["error"]["code"], "GIONG-E5000", "{bad:?}\n{}", out.dump());
    }
}
