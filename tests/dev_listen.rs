//! `giong dev` hands its listening sockets down (`GIONG_DEV_LISTEN`): every
//! worker's `TcpListener::bind` of such an address shares the one socket, a
//! byte goes to `GIONG_DEV_READY` for each, and the socket outlives the
//! process. The test starts a copy of itself with an inherited socket.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use giong::error::{self, Code};

fn listening_socket() -> (socket2::Socket, SocketAddr) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (socket2::Socket::from(l), addr)
}

/// Hands `fds` to the child only: they stay close-on-exec here, so a child
/// that another test starts at the same moment does not inherit them (and
/// keep the ready socket open after our child is gone).
fn pass_fds(cmd: &mut Command, fds: Vec<i32>) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: between fork and exec the closure calls only fcntl(2), which
    // is async-signal-safe, and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            for &fd in &fds {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        })
    }
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
                eprintln!("dev_child: worker {worker} binds {addr}");
                let l = giong::net::TcpListener::bind(addr.as_str()).unwrap();
                eprintln!("dev_child: worker {worker} bound; {}", ready_state());
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

/// What the child sees of its ready descriptor and parent, printed when
/// the test fails.
fn ready_state() -> String {
    let parent = std::env::var("GIONG_DEV_PARENT").unwrap_or_default();
    let Some(fd) = std::env::var("GIONG_DEV_READY")
        .ok()
        .and_then(|r| r.parse::<i32>().ok())
    else {
        return "no GIONG_DEV_READY".into();
    };
    // SAFETY: plain syscalls on a descriptor number; a zero-length send
    // carries nothing.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        let fd_err = std::io::Error::last_os_error();
        let mut ty: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let got = libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut ty as *mut libc::c_int).cast(),
            &mut len,
        );
        let ty_err = std::io::Error::last_os_error();
        let sent = libc::send(fd, b"".as_ptr().cast(), 0, libc::MSG_DONTWAIT);
        let send_err = std::io::Error::last_os_error();
        format!(
            "parent {} (GIONG_DEV_PARENT {parent}); ready fd {fd}: F_GETFD {flags} ({fd_err}), \
             SO_TYPE {got}/{ty} ({ty_err}), empty send {sent} ({send_err})",
            libc::getppid()
        )
    }
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
    // Connections made while one build stops and the next starts, answered
    // by the next one.
    let mut waiting: Vec<TcpStream> = Vec::new();
    for round in 0..3 {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "dev_child", "--nocapture", "--test-threads=1"])
            .env("GIONG_DEV_TEST_ADDR", addr.to_string())
            .env("GIONG_DEV_PARENT", std::process::id().to_string())
            .env("GIONG_DEV_LISTEN", format!("{}={addr}", sock.as_raw_fd()))
            .env("GIONG_DEV_READY", theirs.as_raw_fd().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut service = Service(
            pass_fds(&mut cmd, vec![sock.as_raw_fd(), theirs.as_raw_fd()])
                .spawn()
                .unwrap(),
        );
        drop(theirs);
        ours.set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        // Two workers, each binding twice: four bytes.
        let mut got = [0u8; 4];
        let mut have = 0;
        let read = loop {
            if have == got.len() {
                break Ok(());
            }
            match ours.read(&mut got[have..]) {
                Ok(0) => break Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => have += n,
                Err(e) => break Err(e),
            }
        };
        if let Err(e) = read {
            let status = service.0.try_wait();
            let _ = service.0.kill();
            let mut err = String::new();
            let _ = service.0.stderr.take().unwrap().read_to_string(&mut err);
            panic!(
                "round {round}: the build reported {have} of 4 binds ({:?}), then {e}; \
                 it had {status:?}; its standard error:\n{err}",
                &got[..have]
            );
        }
        assert_eq!(&got, b"\n\n\n\n", "round {round}");
        // Every connection is answered: those that waited between the
        // builds, and new ones.
        let mut conns = std::mem::take(&mut waiting);
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
        // Stop accepting: each worker's listener (the dropped duplicates
        // never accepted) answers that it holds nothing.
        ours.write_all(b"s").unwrap();
        let mut done = [0u8; 2];
        ours.read_exact(&mut done)
            .unwrap_or_else(|e| panic!("round {round}: no stop reply ({e})"));
        assert_eq!(&done, b"dd", "round {round}");
        // From now on the old build takes nothing: this one waits for the
        // next build, even while the old one still runs.
        waiting.push(TcpStream::connect(addr).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        if cfg!(giong_tsan) {
            // TSan holds a signal back until the receiving thread calls a
            // function it intercepts, and the workers wait in raw kevent and
            // io_uring_enter, so SIGTERM would not stop the build (the
            // runtime's SIGTERM tests are skipped under TSan for this).
            // What this test is about, the shared socket, holds either way.
            service.0.kill().unwrap();
            service.0.wait().unwrap();
        } else {
            // SAFETY: kill(2) on the child we started and have not reaped.
            assert_eq!(
                unsafe { libc::kill(service.0.id() as i32, libc::SIGTERM) },
                0
            );
            assert!(service.0.wait().unwrap().success(), "round {round}");
        }
        // Between two builds nothing accepts, but the socket kept here
        // listens: the connection waits in its queue instead of being refused.
        waiting.push(TcpStream::connect(addr).unwrap());
    }
}

/// A process whose parent is not the one `GIONG_DEV_PARENT` names (a program
/// the service runs) ignores the variables.
#[test]
fn only_the_process_giong_dev_started_takes_the_sockets() {
    if std::env::var_os("GIONG_DEV_TEST_ADDR").is_some() {
        return;
    }
    let (sock, addr) = listening_socket();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "malformed_child", "--nocapture"])
        .env("GIONG_DEV_MALFORMED", "ignored")
        .env("GIONG_DEV_PARENT", "1")
        .env("GIONG_DEV_LISTEN", format!("{}={addr}", sock.as_raw_fd()));
    let out = pass_fds(&mut cmd, vec![sock.as_raw_fd()]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_malformed_variable_is_a_coded_error() {
    if std::env::var_os("GIONG_DEV_TEST_ADDR").is_some() {
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "malformed_child", "--nocapture"])
        .env("GIONG_DEV_MALFORMED", "1")
        .env("GIONG_DEV_PARENT", std::process::id().to_string())
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
    let Ok(mode) = std::env::var("GIONG_DEV_MALFORMED") else {
        return;
    };
    if mode == "ignored" {
        // Not giong dev's child: the variable is not looked at, so the bind
        // is a bind of its own, refused because the parent holds the port.
        let listen = std::env::var("GIONG_DEV_LISTEN").unwrap();
        let addr = listen.split_once('=').unwrap().1;
        let err = giong::net::TcpListener::bind(addr).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
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
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "other_child", "--nocapture"])
        .env("GIONG_DEV_OTHER", "1")
        .env("GIONG_DEV_PARENT", std::process::id().to_string())
        .env("GIONG_DEV_LISTEN", format!("{}={addr}", sock.as_raw_fd()));
    let out = pass_fds(&mut cmd, vec![sock.as_raw_fd()]).output().unwrap();
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
