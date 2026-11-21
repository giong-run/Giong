//! The benchmark routes on giong, hand-rolled HTTP parser.
//!
//! Deliberately the same program as `bench/monoio` and `bench/tokio-raw`:
//! same parser, same buffers, no timeouts. `examples/http.rs` in the main
//! crate is the fuller server (timeouts, limits, graceful shutdown).
use std::hint::black_box;
use std::io::Write;

use giong::net::{TcpListener, TcpStream};

fn fib(n: u32) -> u64 { if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) } }

fn find_head_end(b: &[u8]) -> Option<usize> { b.windows(4).position(|w| w == b"\r\n\r\n") }

fn content_length(head: &[u8]) -> usize {
    for line in head.split(|&c| c == b'\n') {
        if line.len() > 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
            return line[15..].iter().filter(|c| c.is_ascii_digit())
                .fold(0usize, |a, &c| a * 10 + (c - b'0') as usize);
        }
    }
    0
}

fn path_of(head: &[u8]) -> &[u8] {
    let mut it = head.split(|&c| c == b' ');
    it.next();
    it.next().unwrap_or(b"/")
}

fn query_n(q: &[u8]) -> u32 {
    q.split(|&c| c == b'&').find(|kv| kv.starts_with(b"n="))
        .map(|kv| kv[2..].iter().take_while(|c| c.is_ascii_digit())
            .fold(0u32, |a, &c| a * 10 + (c - b'0') as u32))
        .unwrap_or(25)
}

fn respond(path: &[u8], out: &mut Vec<u8>, body: &mut Vec<u8>) {
    body.clear();
    let (route, query) = match path.iter().position(|&c| c == b'?') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (path, &b""[..]),
    };
    match route {
        b"/json" => body.extend_from_slice(br#"{"message":"Hello, World!"}"#),
        b"/users" => {
            body.push(b'[');
            for i in 0..100u32 {
                if i > 0 { body.push(b','); }
                let _ = write!(body, r#"{{"id":{i},"name":"User {i}","email":"user{i}@example.com","active":{},"score":{}}}"#,
                    i % 2 == 0, i as f64 * 1.5);
            }
            body.push(b']');
        }
        b"/compute" => { let _ = write!(body, r#"{{"result":{}}}"#, fib(black_box(query_n(query)))); }
        _ => {
            out.extend_from_slice(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            return;
        }
    }
    let _ = write!(out, "HTTP/1.1 200 OK\r\nServer: native\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len());
    out.extend_from_slice(body);
}



/// Handles every complete request in `buf`; returns how many bytes were consumed.
fn handle_requests(buf: &[u8], out: &mut Vec<u8>, body: &mut Vec<u8>) -> usize {
    let mut used = 0;
    while let Some(end) = find_head_end(&buf[used..]) {
        let head = &buf[used..used + end];
        let total = end + 4 + content_length(head);
        if buf.len() - used < total {
            break;
        }
        respond(path_of(head), out, body);
        used += total;
    }
    used
}

async fn serve(stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let mut pending: Vec<u8> = Vec::new();
    let mut out: Vec<u8> = Vec::with_capacity(8192);
    let mut body: Vec<u8> = Vec::with_capacity(16384);
    loop {
        let chunk = match stream.recv().await {
            Ok(chunk) if !chunk.is_empty() => chunk,
            _ => return,
        };
        if pending.is_empty() {
            let used = handle_requests(&chunk, &mut out, &mut body);
            pending.extend_from_slice(&chunk[used..]);
        } else {
            pending.extend_from_slice(&chunk);
            let used = handle_requests(&pending, &mut out, &mut body);
            pending.drain(..used);
        }
        drop(chunk);
        if pending.len() > 64 * 1024 {
            return;
        }
        if !out.is_empty() {
            let (res, buf) = stream.write_all(out).await;
            out = buf;
            if res.is_err() {
                return;
            }
            out.clear();
        }
    }
}

fn main() {
    // One worker per CPU the process is allowed to run on (the default), each
    // with its own listener; the benchmark scripts choose the CPUs with taskset.
    // GIONG_SPIN_US=<microseconds> turns on busy-polling before sleeping.
    let mut builder = giong::Builder::new();
    if let Some(us) = std::env::var("GIONG_SPIN_US").ok().and_then(|v| v.parse().ok()) {
        builder = builder.spin_before_park(std::time::Duration::from_micros(us));
    }
    // GIONG_RECV_BUFS=<count>,<size> overrides the receive buffer pool.
    if let Some((count, size)) = std::env::var("GIONG_RECV_BUFS").ok().and_then(|v| {
        let (count, size) = v.split_once(',')?;
        Some((count.parse().ok()?, size.parse().ok()?))
    }) {
        builder = builder.recv_buffers(count, size);
    }
    let runtime = builder.build().expect("giong runtime");
    runtime
        .run(|_worker| async {
            let listener = TcpListener::bind(bench_addr(3006)).expect("bind");
            loop {
                if let Ok(stream) = listener.accept().await {
                    giong::spawn(serve(stream));
                }
            }
        })
        .expect("giong runtime");
}

/// `BENCH_HOST` (default 127.0.0.1) lets bench/workers.sh drive the server
/// from another machine.
fn bench_addr(port: u16) -> String {
    let host = std::env::var("BENCH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    format!("{host}:{port}")
}
