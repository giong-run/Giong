//! Choosing the I/O driver: `Builder::driver`, `GIONG_DRIVER`, and what a
//! worker reports with `giong::driver_kind`.

use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use giong::error::{self, Code};
use giong::net::{TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream};
use giong::{ChosenBy, DriverKind};

/// `true` if io_uring works here (it is blocked, for instance, under
/// `scripts/no-io-uring.py` or in a container). Where CI says it must work
/// (`GIONG_TEST_EXPECT_DRIVER=io_uring`), not working is a failure instead
/// of a reason to test epoll only.
fn io_uring_available() -> bool {
    let res = giong::Builder::new()
        .driver(DriverKind::IoUring)
        .block_on(async {});
    if let Err(e) = &res {
        assert_ne!(
            std::env::var("GIONG_TEST_EXPECT_DRIVER").as_deref(),
            Ok("io_uring"),
            "io_uring must work here, but: {e}"
        );
    }
    res.is_ok()
}

#[test]
fn a_named_driver_is_the_one_every_worker_gets() {
    let mut kinds = vec![DriverKind::Epoll];
    if io_uring_available() {
        kinds.push(DriverKind::IoUring);
    }
    for kind in kinds {
        giong::Builder::new()
            .workers(2)
            .driver(kind)
            .build()
            .unwrap()
            .run(move |_| async move {
                assert_eq!(giong::driver_kind(), kind);
            })
            .unwrap();
        let got = giong::Builder::new()
            .driver(kind)
            .block_on(async { giong::driver_kind() })
            .unwrap();
        assert_eq!(got, kind);
    }
}

#[test]
fn driver_names_are_what_giong_driver_accepts() {
    assert_eq!(DriverKind::IoUring.name(), "io_uring");
    assert_eq!(DriverKind::Epoll.to_string(), "epoll");
}

const CHILD: &str = "GIONG_TEST_DRIVER_CHILD";

/// Runs test `name` again in a child process with `GIONG_DRIVER` set, so
/// that the variable does not leak into the tests running alongside.
fn child_with_env(name: &str, value: &str) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .env("GIONG_DRIVER", value)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[test]
fn giong_driver_chooses_when_the_builder_does_not() {
    if std::env::var_os(CHILD).is_some() {
        let got = giong::Builder::new()
            .block_on(async { giong::driver_kind() })
            .unwrap();
        println!("got: {got}");
        // A driver named in code wins over the variable.
        let named = giong::Builder::new()
            .driver(DriverKind::Epoll)
            .block_on(async { giong::driver_kind() })
            .unwrap();
        println!("named: {named}");
        return;
    }
    let out = child_with_env("giong_driver_chooses_when_the_builder_does_not", "epoll");
    assert!(out.contains("got: epoll"), "{out}");
    assert!(out.contains("named: epoll"), "{out}");
    if io_uring_available() {
        let out = child_with_env("giong_driver_chooses_when_the_builder_does_not", "io_uring");
        assert!(out.contains("got: io_uring"), "{out}");
        assert!(out.contains("named: epoll"), "{out}");
    }
}

#[test]
fn an_unknown_giong_driver_is_an_error() {
    if std::env::var_os(CHILD).is_some() {
        let err = giong::Builder::new()
            .build()
            .err()
            .expect("build must fail");
        println!("build: {err}");
        println!("code: {:?}", error::code(&err));
        let err = giong::Builder::new().block_on(async {}).unwrap_err();
        println!("block_on: {err}");
        return;
    }
    let out = child_with_env("an_unknown_giong_driver_is_an_error", "kqueue");
    assert!(out.contains("code: Some(InvalidConfig)"), "{out}");
    for call in ["build", "block_on"] {
        let line = out
            .lines()
            .find(|l| l.starts_with(&format!("{call}: ")))
            .unwrap_or_else(|| panic!("no {call} line in {out}"));
        assert!(line.contains("GIONG-E0004"), "{line}");
        assert!(line.contains("\"kqueue\""), "{line}");
        assert!(line.contains("io_uring or epoll"), "{line}");
    }
    let _ = Code::InvalidConfig;
}

/// I/O that always finishes at once (on epoll, a UDP send never waits)
/// must still let the other tasks on the worker run.
#[test]
fn a_task_whose_io_never_waits_still_lets_others_run() {
    for kind in [DriverKind::Epoll, DriverKind::IoUring] {
        if kind == DriverKind::IoUring && !io_uring_available() {
            continue;
        }
        giong::Builder::new()
            .driver(kind)
            .block_on(async {
                let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
                let addr = sink.local_addr().unwrap();
                let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
                let ticks = Rc::new(Cell::new(0u32));
                let counter = {
                    let ticks = ticks.clone();
                    giong::spawn(async move {
                        loop {
                            ticks.set(ticks.get() + 1);
                            giong::yield_now().await;
                        }
                    })
                };
                // Let the counter start.
                giong::time::sleep(Duration::from_millis(1)).await;
                let before = ticks.get();
                for _ in 0..2_000 {
                    let (res, _) = sender.send_to(&b"x"[..], addr).await;
                    res.unwrap();
                }
                let ran = ticks.get() - before;
                counter.abort();
                assert!(
                    ran >= 10,
                    "{kind}: the counter ran {ran} times during 2000 sends"
                );
                drop(sink);
            })
            .unwrap();
    }
}

/// CI says which driver giong must choose by itself on each machine
/// (`GIONG_TEST_EXPECT_DRIVER`): `epoll` inside Docker's default seccomp
/// profile and on 5.15, `io_uring` everywhere else, including kernels with
/// bugs giong works around. A job meant to test io_uring then fails loudly
/// instead of quietly testing epoll. The choice is made in a child process
/// without `GIONG_DRIVER`, which the epoll rounds of `scripts/check.sh` set.
#[test]
fn the_default_driver_is_the_expected_one() {
    if std::env::var_os(CHILD).is_some() {
        let choice = giong::Builder::new()
            .block_on(async { giong::driver_choice() })
            .unwrap();
        println!("kind: {}", choice.kind);
        println!("choice: {choice}");
        return;
    }
    let Ok(expected) = std::env::var("GIONG_TEST_EXPECT_DRIVER") else {
        return;
    };
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_default_driver_is_the_expected_one",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env_remove("GIONG_DRIVER")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(
        stdout.contains(&format!("kind: {expected}\n")),
        "expected giong to choose {expected} by itself, but: {stdout}"
    );
}

/// `driver_choice` says who chose the driver, and with nothing named, that
/// giong chose io_uring where it works.
#[test]
fn driver_choice_says_who_chose() {
    if std::env::var_os(CHILD).is_some() {
        let choice = giong::Builder::new()
            .block_on(async { giong::driver_choice() })
            .unwrap();
        println!("chosen by: {:?}", choice.chosen_by);
        println!("kind: {}", choice.kind);
        return;
    }
    let named = giong::Builder::new()
        .driver(DriverKind::Epoll)
        .block_on(async { giong::driver_choice() })
        .unwrap();
    assert_eq!(named.kind, DriverKind::Epoll);
    assert_eq!(named.chosen_by, ChosenBy::Builder);
    assert_eq!(named.io_uring_error, None);
    assert_eq!(named.to_string(), "epoll (named by Builder::driver)");

    let out = child_with_env("driver_choice_says_who_chose", "epoll");
    assert!(out.contains("chosen by: Environment"), "{out}");
    assert!(out.contains("kind: epoll"), "{out}");

    if io_uring_available() && std::env::var_os("GIONG_DRIVER").is_none() {
        let rt = giong::Builder::new().workers(2).build().unwrap();
        let handle = rt.handle();
        assert_eq!(handle.driver_choice(0), None, "not started yet");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        rt.run(move |w| {
            let sink = sink.clone();
            async move { sink.lock().unwrap().push((w, giong::driver_choice())) }
        })
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        for (w, auto) in seen.iter() {
            assert_eq!(auto.kind, DriverKind::IoUring);
            assert_eq!(auto.chosen_by, ChosenBy::Automatic);
            assert_eq!(auto.io_uring_error, None);
            assert_eq!(auto.io_uring_error_code, None);
            // Kept after the runtime has stopped.
            assert_eq!(handle.driver_choice(*w).as_ref(), Some(auto));
            let line = auto.to_string();
            assert!(
                line.starts_with("io_uring (chosen automatically)"),
                "{line}"
            );
            for k in &auto.kernel_workarounds {
                assert!(line.contains(k.description()), "{line}");
            }
        }
    }
}

/// Sends `ping` over `stream` and reads the echo; `true` if that took less
/// than `limit`. A lost wake-up does not show as a hang here: the timeout's
/// own timer wakes the task, which then finds the echo waiting. Hence the
/// clock.
async fn echo_within<S: Echo>(stream: S, limit: Duration) -> bool {
    let start = std::time::Instant::now();
    let echoed = giong::time::timeout(2 * limit, stream.ping())
        .await
        .is_ok_and(|r| r.is_ok_and(|b| b == b"ping"));
    echoed && start.elapsed() < limit
}

trait Echo {
    async fn ping(self) -> std::io::Result<Vec<u8>>;
}

impl Echo for TcpStream {
    async fn ping(self) -> std::io::Result<Vec<u8>> {
        let (r, _) = self.write_all(b"ping".to_vec()).await;
        r?;
        let (r, buf) = self.read(vec![0u8; 16]).await;
        Ok(buf[..r?].to_vec())
    }
}

impl Echo for UnixStream {
    async fn ping(self) -> std::io::Result<Vec<u8>> {
        let (r, _) = self.write_all(b"ping".to_vec()).await;
        r?;
        let (r, buf) = self.read(vec![0u8; 16]).await;
        Ok(buf[..r?].to_vec())
    }
}

#[test]
fn a_failed_connect_does_not_stall_the_next_socket() {
    // A connect that fails gives its descriptor number back, and the next
    // socket usually gets the same number. The epoll driver once kept the
    // old registration on record, so the new socket was never woken.
    let mut kinds = vec![DriverKind::Epoll];
    if io_uring_available() {
        kinds.push(DriverKind::IoUring);
    }
    let dir = std::env::temp_dir().join(format!("giong-failed-connect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for kind in kinds {
        let dir = dir.clone();
        giong::Builder::new()
            .driver(kind)
            .block_on(async move {
                let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
                let tcp_addr = tcp.local_addr().unwrap();
                let path = dir.join(format!("{kind}.sock"));
                let unix = UnixListener::bind(&path).unwrap();
                giong::spawn(async move {
                    loop {
                        let s = tcp.accept().await.unwrap();
                        giong::spawn(async move {
                            let (r, buf) = s.read(vec![0u8; 16]).await;
                            let n = r.unwrap();
                            let _ = s.write_all(buf[..n].to_vec()).await;
                        });
                    }
                });
                giong::spawn(async move {
                    loop {
                        let s = unix.accept().await.unwrap();
                        giong::spawn(async move {
                            let (r, buf) = s.read(vec![0u8; 16]).await;
                            let n = r.unwrap();
                            let _ = s.write_all(buf[..n].to_vec()).await;
                        });
                    }
                });
                // A port nobody listens on, kept bound (without listening) for
                // the whole test. A port that was bound and closed again could
                // be taken by another test meanwhile, or become the source port
                // of the connect itself, and the connect would then succeed.
                let reserved =
                    socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
                        .unwrap();
                reserved
                    .bind(
                        &"127.0.0.1:0"
                            .parse::<std::net::SocketAddr>()
                            .unwrap()
                            .into(),
                    )
                    .unwrap();
                let refused = reserved.local_addr().unwrap().as_socket().unwrap();
                let missing = dir.join("missing.sock");
                // Generous: a working echo takes microseconds.
                let limit = Duration::from_secs(2);
                for _ in 0..3 {
                    assert!(TcpStream::connect(refused).await.is_err());
                    let s = TcpStream::connect(tcp_addr).await.unwrap();
                    assert!(
                        echo_within(s, limit).await,
                        "{kind}: TCP after a refused connect"
                    );
                    assert!(UnixStream::connect(&missing).await.is_err());
                    let s = UnixStream::connect(&path).await.unwrap();
                    assert!(
                        echo_within(s, limit).await,
                        "{kind}: Unix after a failed connect"
                    );
                }
            })
            .unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
