//! A whole connection: arbitrary bytes, delivered in arbitrary pieces, go
//! through `Server::serve_connection` over an in-memory transport.
//!
//! Checks that the server never panics and always finishes (libFuzzer's
//! `-timeout` catches a hang), that every request the handler sees is one it
//! can trust, and that what the server writes back is a well-formed sequence
//! of responses.
#![no_main]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::sync::{mpsc, Mutex, OnceLock};

use giong_http::{Request, Response, Server, Transport};
use libfuzzer_sys::fuzz_target;

const MAX_HEAD: usize = 512;
const MAX_BODY: usize = 64;

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

async fn handler(req: Request) -> Response {
    assert!(is_token(req.method()), "method {:?}", req.method());
    assert!(!req.target().is_empty());
    assert!(req.version_minor() <= 1);
    for (name, value) in req.headers() {
        assert!(is_token(name), "header name {name:?}");
        assert!(!value.iter().any(|b| matches!(b, b'\r' | b'\n' | 0)));
    }
    assert!(req.body().len() <= MAX_BODY);
    Response::new(200)
}

/// Every response the server wrote is a head with an empty body: the handler
/// sends none, and neither do the server's own error responses.
fn check_output(mut out: &[u8]) {
    while !out.is_empty() {
        let end = out
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap_or_else(|| panic!("unterminated response {:?}", String::from_utf8_lossy(out)));
        let head = std::str::from_utf8(&out[..end]).expect("response head is not UTF-8");
        let mut lines = head.split("\r\n");
        let status = lines.next().unwrap();
        let ok = (status.starts_with("HTTP/1.1 ") || status.starts_with("HTTP/1.0 "))
            && status.len() >= 13
            && status.as_bytes()[9..12].iter().all(u8::is_ascii_digit)
            && status.as_bytes()[12] == b' ';
        assert!(ok, "status line {status:?}");
        for line in lines {
            let (name, value) = line.split_once(": ").expect("header line without ': '");
            if name.eq_ignore_ascii_case("content-length") {
                assert_eq!(value, "0", "unexpected body");
            }
        }
        out = &out[end + 4..];
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
                    let handler = Rc::new(handler);
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
