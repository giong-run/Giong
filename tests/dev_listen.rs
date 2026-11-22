//! `giong dev` hands its listening sockets down (`GIONG_DEV_LISTEN`): every
//! worker's `TcpListener::bind` of such an address shares the one socket, a
//! byte goes to `GIONG_DEV_READY` for each, and the socket outlives the
//! process. The test starts a copy of itself with an inherited socket.

#![cfg(unix)]

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::Duration;

use giong::error::{self, Code};

fn listening_socket() -> (socket2::Socket, SocketAddr) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let sock = socket2::Socket::from(l);
    sock.set_cloexec(false).unwrap();
    (sock, addr)
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: pipe(2) fills two descriptors we then own.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: both are open and owned by nobody else.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// The service: two workers bind the inherited address and answer every
/// connection with their worker number until SIGTERM.
#[test]
fn dev_child() {
    let Ok(addr) = std::env::var("GIONG_DEV_TEST_ADDR") else {
        return;
    };
    giong::Builder::new()
        .workers(2)
        .shutdown_on_signals(true)
        .build()
        .unwrap()
        .run(move |worker| {
            let addr = addr.clone();
            async move {
                let l = giong::net::TcpListener::bind(addr.as_str()).unwrap();
                assert_eq!(l.local_addr().unwrap().to_string(), addr);
                // Dropping a duplicate leaves the shared socket listening.
                drop(giong::net::TcpListener::bind(addr.as_str()).unwrap());
                while let Some(s) = giong::until_shutdown(l.accept()).await {
                    let s = s.unwrap();
                    s.write_all(format!("worker {worker}\n").into_bytes())
                        .await
                        .0
                        .unwrap();
                }
            }
        })
        .unwrap();
}

/// Kills the service if the test fails half way, so no copy is left behind.
struct Service(std::process::Child);

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn restarts_share_the_socket_and_refuse_nothing() {
    if std::env::var_os("GIONG_DEV_TEST_ADDR").is_some() {
        return;
    }
    let (sock, addr) = listening_socket();
    // A connection made while no build runs, answered by the next one.
    let mut waiting: Option<TcpStream> = None;
    for round in 0..3 {
        let (ready_r, ready_w) = pipe();
        // SAFETY: fcntl on the descriptor we own: keep it open across exec.
        unsafe {
            let flags = libc::fcntl(ready_w.as_raw_fd(), libc::F_GETFD);
            libc::fcntl(
                ready_w.as_raw_fd(),
                libc::F_SETFD,
                flags & !libc::FD_CLOEXEC,
            );
        }
        let mut service = Service(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "dev_child", "--nocapture", "--test-threads=1"])
                .env("GIONG_DEV_TEST_ADDR", addr.to_string())
                .env("GIONG_DEV_LISTEN", format!("{}={addr}", sock.as_raw_fd()))
                .env("GIONG_DEV_READY", ready_w.as_raw_fd().to_string())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        drop(ready_w);
        // Two workers, each binding twice: four bytes.
        let mut ready = std::fs::File::from(ready_r);
        let mut got = [0u8; 4];
        ready.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"\n\n\n\n", "round {round}");
        // Every connection is answered: the one that waited between the
        // builds, if any, and new ones.
        let mut conns: Vec<TcpStream> = waiting.take().into_iter().collect();
        while conns.len() < 20 {
            conns.push(TcpStream::connect(addr).unwrap());
        }
        for (i, mut c) in conns.into_iter().enumerate() {
            c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let mut line = String::new();
            c.read_to_string(&mut line)
                .unwrap_or_else(|e| panic!("round {round}, connection {i}: {e}"));
            assert!(
                line == "worker 0\n" || line == "worker 1\n",
                "round {round}, connection {i}: {line:?}"
            );
        }
        // SAFETY: kill(2) on the child we started and have not reaped.
        assert_eq!(
            unsafe { libc::kill(service.0.id() as i32, libc::SIGTERM) },
            0
        );
        assert!(service.0.wait().unwrap().success(), "round {round}");
        // Between two builds nothing accepts, but the socket kept here
        // listens: the connection waits in its queue instead of being refused.
        waiting = Some(TcpStream::connect(addr).unwrap());
    }
}

#[test]
fn a_malformed_variable_is_a_coded_error() {
    if std::env::var_os("GIONG_DEV_TEST_ADDR").is_some() {
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "malformed_child", "--nocapture"])
        .env("GIONG_DEV_MALFORMED", "1")
        .env("GIONG_DEV_LISTEN", "3=localhost:8080")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn malformed_child() {
    if std::env::var_os("GIONG_DEV_MALFORMED").is_none() {
        return;
    }
    let err = giong::net::TcpListener::bind("127.0.0.1:1").unwrap_err();
    assert_eq!(error::code(&err), Some(Code::InvalidConfig));
    assert!(err.to_string().contains("GIONG_DEV_LISTEN"), "{err}");
    // Port 0 never looks at the variable.
    giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
}

#[test]
fn an_address_it_does_not_name_binds_as_usual() {
    if std::env::var_os("GIONG_DEV_TEST_ADDR").is_some() {
        return;
    }
    let (sock, addr) = listening_socket();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "other_child", "--nocapture"])
        .env("GIONG_DEV_OTHER", "1")
        .env("GIONG_DEV_LISTEN", format!("{}={addr}", sock.as_raw_fd()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn other_child() {
    if std::env::var_os("GIONG_DEV_OTHER").is_none() {
        return;
    }
    giong::block_on(async {
        let l = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        // A fixed port the variable does not name: a socket of its own.
        let l = giong::net::TcpListener::bind(format!("127.0.0.1:{port}").as_str()).unwrap();
        drop(l);
    });
}
