//! A whole connection: arbitrary bytes, delivered in arbitrary pieces, go
//! through `Server::serve_connection` over an in-memory transport.
//!
//! Checks that the server never panics and always finishes (libFuzzer's
//! `-timeout` catches a hang), that every request the handler sees is one it
//! can trust, and that what the server writes back is a well-formed sequence
//! of responses.
//!
//! The path picks what the handler does: `/s...` reads the body as a stream
//! (`h` in the path: only its first piece), `r` answers with a streamed
//! body (`x`: aborted after its first piece), `n` answers 204.
#![no_main]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::sync::{mpsc, Mutex, OnceLock};

use giong::error::Code;
use giong_http::{Body, Handler, Request, Response, Server, Transport};
use libfuzzer_sys::fuzz_target;

const MAX_HEAD: usize = 512;
const MAX_BODY: usize = 64;
/// What a streamed response sends, in pieces.
const STREAMED: &[u8] = b"abcde";

struct Memory {
    input: RefCell<VecDeque<Vec<u8>>>,
    output: Rc<RefCell<Vec<u8>>>,
}

impl Transport for Memory {
    type Chunk = Vec<u8>;

    async fn recv(&self) -> io::Result<Vec<u8>> {
        // Empty means end of stream.
        Ok(self.input.borrow_mut().pop_front().unwrap_or_default())
    }

    async fn write_all(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        self.output.borrow_mut().extend_from_slice(&buf);
        (Ok(()), buf)
    }

    async fn close_write(&self) {}
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn check_request(req: &Request) {
    assert!(is_token(req.method()), "method {:?}", req.method());
    assert!(!req.target().is_empty());
    assert!(req.version_minor() <= 1);
    for (name, value) in req.headers() {
        assert!(is_token(name), "header name {name:?}");
        assert!(!value.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)));
    }
    assert!(req.body().len() <= MAX_BODY);
}

struct Fuzzed;

impl Handler for Fuzzed {
    fn stream_body(&self, req: &Request) -> Option<u64> {
        check_request(req);
        assert!(req.body().is_empty());
        req.path().starts_with("/s").then_some(MAX_BODY as u64)
    }

    async fn call(&self, req: Request) -> Response {
        check_request(&req);
        let path = req.path().to_string();
        if let Some(body) = req.body_stream() {
            assert!(path.starts_with("/s") && req.body().is_empty());
            let mut total = 0u64;
            loop {
                match body.chunk().await {
                    Ok(Some(piece)) => {
                        assert!(!piece.is_empty());
                        total += piece.len() as u64;
                        assert!(total <= MAX_BODY as u64);
                        if path.contains('h') {
                            break;
                        }
                    }
                    Ok(None) => {
                        if let Some(length) = body.content_length() {
                            assert_eq!(total, length);
                        }
                        break;
                    }
                    Err(e) => {
                        let code = giong::error::code(&e);
                        assert!(
                            matches!(
                                code,
                                Some(
                                    Code::HttpBodyTooLarge
                                        | Code::HttpBodyIncomplete
                                        | Code::HttpBodyMalformed
                                )
                            ),
                            "{e}"
                        );
                        break;
                    }
                }
            }
            assert_eq!(body.bytes_read(), total);
        }
        let status = if path.contains('n') { 204 } else { 200 };
        if path.contains('r') && req.method() != "HEAD" {
            let (tx, body) = Body::channel();
            let abort = path.contains('x');
            drop(giong::spawn(async move {
                if tx.send(&STREAMED[..2]).await.is_err() {
                    return;
                }
                if abort {
                    return tx.abort();
                }
                let _ = tx.send(Vec::new()).await;
                let _ = tx.send(&STREAMED[2..]).await;
            }));
            return Response::new(status).body(body);
        }
        Response::new(status)
    }
}

/// A chunked body as the server writes it: its data, and the bytes after
/// it; `None` if the output ends inside it (an aborted body).
fn chunked_body(mut out: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut body = Vec::new();
    loop {
        let nl = out.windows(2).position(|w| w == b"\r\n")?;
        let line = std::str::from_utf8(&out[..nl]).expect("chunk size line");
        let size = usize::from_str_radix(line, 16).expect("chunk size");
        assert_eq!(line, format!("{size:x}"), "chunk size line");
        out = &out[nl + 2..];
        if size == 0 {
            assert!(out.starts_with(b"\r\n") || out.is_empty(), "trailers");
            if out.is_empty() {
                return None;
            }
            return Some((body, &out[2..]));
        }
        if out.len() < size + 2 {
            return None;
        }
        body.extend_from_slice(&out[..size]);
        assert_eq!(&out[size..size + 2], b"\r\n", "end of chunk");
        out = &out[size + 2..];
    }
}

/// Every response the server wrote is well formed, with an empty body,
/// or the streamed body (chunked, or to the end for HTTP/1.0; cut short
/// only at the end of the output).
fn check_output(mut out: &[u8]) {
    while !out.is_empty() {
        let end = out
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("unterminated response {:?}", String::from_utf8_lossy(out)));
        let head = std::str::from_utf8(&out[..end]).expect("response head is not UTF-8");
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap();
        let ok = (status_line.starts_with("HTTP/1.1 ") || status_line.starts_with("HTTP/1.0 "))
            && status_line.len() >= 13
            && status_line.as_bytes()[9..12].iter().all(u8::is_ascii_digit)
            && status_line.as_bytes()[12] == b' ';
        assert!(ok, "status line {status_line:?}");
        let status: u16 = status_line[9..12].parse().unwrap();
        let (mut length, mut chunked) = (None, false);
        for line in lines {
            let (name, value) = line.split_once(": ").expect("header line without ': '");
            if name.eq_ignore_ascii_case("content-length") {
                assert!(length.is_none(), "two lengths");
                length = Some(value.to_string());
            }
            if name.eq_ignore_ascii_case("transfer-encoding") {
                assert_eq!(value, "chunked");
                chunked = true;
            }
        }
        assert!(!(chunked && length.is_some()), "a length and chunked");
        out = &out[end + 4..];
        if status < 200 || status == 204 || status == 304 {
            assert!(length.is_none() && !chunked, "{status} with a body");
        } else if let Some(length) = length {
            assert_eq!(length, "0", "unexpected body");
        } else if chunked {
            match chunked_body(out) {
                Some((body, rest)) => {
                    assert_eq!(body, STREAMED);
                    out = rest;
                }
                None => {
                    // Aborted: the output ends inside the body.
                    return;
                }
            }
        } else {
            // HTTP/1.0 and streamed: the body runs to the end.
            assert!(status_line.starts_with("HTTP/1.0 "), "no length: {head:?}");
            assert!(STREAMED.starts_with(out), "{:?}", String::from_utf8_lossy(out));
            return;
        }
    }
}

struct Job {
    input: VecDeque<Vec<u8>>,
    server: Server,
    done: mpsc::Sender<Vec<u8>>,
}

/// One runtime for the whole fuzzing process: setting up an io_uring per input
/// would dominate the run time.
fn runtime() -> &'static Mutex<giong::sync::mpsc::Sender<Job>> {
    static JOBS: OnceLock<Mutex<giong::sync::mpsc::Sender<Job>>> = OnceLock::new();
    JOBS.get_or_init(|| {
        let (tx, mut rx) = giong::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            giong::Builder::new()
                .workers(1)
                .block_on(async move {
                    let handler = Rc::new(Fuzzed);
                    while let Some(job) = rx.recv().await {
                        let output = Rc::new(RefCell::new(Vec::new()));
                        let stream = Memory {
                            input: RefCell::new(job.input),
                            output: output.clone(),
                        };
                        job.server.serve_connection(stream, handler.clone()).await;
                        let _ = job.done.send(output.take());
                    }
                })
                .expect("runtime");
        });
        Mutex::new(tx)
    })
}

fuzz_target!(|data: &[u8]| {
    // Byte 0: seed for the piece sizes; 0 sends everything in one piece.
    let Some((&seed, bytes)) = data.split_first() else {
        return;
    };
    let mut input = VecDeque::new();
    if seed == 0 {
        input.push_back(bytes.to_vec());
    } else {
        let mut state = u32::from(seed);
        let mut rest = bytes;
        while !rest.is_empty() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let n = (1 + state as usize % 64).min(rest.len());
            input.push_back(rest[..n].to_vec());
            rest = &rest[n..];
        }
    }

    let server = Server::new()
        .max_head_size(MAX_HEAD)
        .max_body_size(MAX_BODY);
    let (done, result) = mpsc::channel();
    let job = Job {
        input,
        server,
        done,
    };
    if runtime().lock().unwrap().send(job).is_err() {
        panic!("the runtime thread died");
    }
    let output = result.recv().expect("the runtime thread died");
    check_output(&output);
});
