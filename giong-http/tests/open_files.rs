//! A server out of file descriptors says so (GIONG-E1123) instead of
//! leaving new clients waiting in silence. The test runs itself again in a
//! child process with a low limit on open files, since the limit is
//! process-wide, and opens the connections from here.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use giong::net::TcpListener;
use giong_http::{Request, Response, Server};

const CHILD: &str = "GIONG_HTTP_TEST_OPEN_FILES_CHILD";

/// Open files the child may hold: its own descriptors and a few
/// connections, far fewer than the clients below.
const LIMIT: u64 = 48;

#[test]
fn a_server_out_of_file_descriptors_says_so() {
    if std::env::var_os(CHILD).is_some() {
        return child();
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_server_out_of_file_descriptors_says_so",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let addr = loop {
        let line = lines
            .next()
            .expect("the child printed its address")
            .unwrap();
        if let Some(addr) = line.strip_prefix("listening on ") {
            break addr.to_owned();
        }
    };
    // The kernel completes these in the listen queue; the server accepts
    // until it holds LIMIT descriptors.
    let clients: Vec<_> = (0..2 * LIMIT)
        .map(|_| TcpStream::connect(&addr).unwrap())
        .collect();
    let out = child.wait_with_output().unwrap();
    drop(clients);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let line = stderr
        .lines()
        .find(|l| l.contains("GIONG-E1123"))
        .unwrap_or_else(|| panic!("no GIONG-E1123 line in:\n{stderr}"));
    assert!(
        line.starts_with("giong-http: GIONG-E1123 could not accept "),
        "{line}"
    );
    assert!(line.contains("(os error 24)"), "{line}");
    assert!(
        line.contains(&format!("(open files: {LIMIT}, hard limit ")),
        "{line}"
    );
    assert!(line.ends_with("see docs/errors/E1123.md"), "{line}");
    // At most once a minute: one line, however many accepts failed.
    assert_eq!(stderr.matches("GIONG-E1123").count(), 1, "{stderr}");
}

/// Serves for a second and a half with at most LIMIT open files.
fn child() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` and `setrlimit` read and write the struct given.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
        lim.rlim_cur = LIMIT;
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &lim), 0);
    }
    let rt = giong::Builder::new()
        .workers(1)
        .raise_open_files_limit(false)
        .build()
        .unwrap();
    let handle = rt.handle();
    let server = std::thread::spawn(move || {
        rt.run(|_| async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            println!("listening on {}", listener.local_addr().unwrap());
            Server::new()
                .serve(listener, |_: Request| async { Response::text("ok") })
                .await;
        })
        .unwrap();
    });
    std::thread::sleep(Duration::from_millis(1500));
    handle.shutdown();
    server.join().unwrap();
}
