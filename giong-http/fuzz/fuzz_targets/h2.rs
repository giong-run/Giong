//! A whole HTTP/2 connection: frames built from the input (well delimited,
//! with header blocks a real encoder made, and raw bytes now and then)
//! after the client preface, delivered in arbitrary pieces, through
//! `Server::serve_connection` over an in-memory transport.
//!
//! Checks that the server never panics and always finishes (libFuzzer's
//! `-timeout` catches a hang), that handlers only see requests they can
//! trust, and that what the server writes back is a well-formed sequence
//! of frames that respects the client's flow-control windows.
#![no_main]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{mpsc, Mutex, OnceLock};

use giong_http::fuzzing::h2_connection::{pieces, server, Fuzzed, Memory};
use giong_http::fuzzing::{check_h2_output, h2_input};
use libfuzzer_sys::fuzz_target;

struct Job {
    input: VecDeque<Vec<u8>>,
    done: mpsc::Sender<Vec<u8>>,
}

/// One runtime for the whole fuzzing process: setting up an io_uring per
/// input would dominate the run time.
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
                        server().serve_connection(stream, handler.clone()).await;
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
    let Some((&seed, rest)) = data.split_first() else {
        return;
    };
    let input = h2_input(rest);
    let (done, result) = mpsc::channel();
    let job = Job {
        input: pieces(&input, seed),
        done,
    };
    if runtime().lock().unwrap().send(job).is_err() {
        panic!("the runtime thread died");
    }
    let output = result.recv().expect("the runtime thread died");
    check_h2_output(&input, &output);
});
