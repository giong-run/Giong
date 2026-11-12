//! Raw Tokio HTTP server: Tokio runtime + TcpStream, hand-rolled parser (no hyper, no axum).
use std::hint::black_box;
use std::io::Write;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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


#[tokio::main]
async fn main() {
    let l = tokio::net::TcpListener::bind(bench_addr(3005)).await.unwrap();
    loop {
        let (mut s, _) = l.accept().await.unwrap();
        let _ = s.set_nodelay(true);
        tokio::spawn(async move {
            let mut rbuf: Vec<u8> = Vec::with_capacity(8192);
            let mut wbuf: Vec<u8> = Vec::with_capacity(8192);
            let mut body: Vec<u8> = Vec::with_capacity(16384);
            loop {
                if rbuf.capacity() - rbuf.len() < 2048 { rbuf.reserve(8192); }
                match s.read_buf(&mut rbuf).await { Ok(0) | Err(_) => return, Ok(_) => {} }
                let mut off = 0;
                while let Some(end) = find_head_end(&rbuf[off..]) {
                    let head = &rbuf[off..off + end];
                    let total = end + 4 + content_length(head);
                    if rbuf.len() - off < total { break; }
                    respond(path_of(head), &mut wbuf, &mut body);
                    off += total;
                }
                rbuf.drain(..off);
                if rbuf.len() > 64 * 1024 { return; }
                if !wbuf.is_empty() {
                    if s.write_all(&wbuf).await.is_err() { return; }
                    wbuf.clear();
                }
            }
        });
    }
}

/// `BENCH_HOST` (default 127.0.0.1) lets bench/workers.sh drive the server
/// from another machine.
fn bench_addr(port: u16) -> String {
    let host = std::env::var("BENCH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    format!("{host}:{port}")
}
