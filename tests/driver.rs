//! Choosing the I/O driver: `Builder::driver`, `GIONG_DRIVER`, and what a
//! worker reports with `giong::driver_kind`.

use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;
use std::time::Duration;

use giong::error::{self, Code};
use giong::net::UdpSocket;
use giong::DriverKind;

/// `true` if io_uring works here (it is blocked, for instance, under
/// `scripts/no-io-uring.py` or in a container).
fn io_uring_available() -> bool {
    giong::Builder::new()
        .driver(DriverKind::IoUring)
        .block_on(async {})
        .is_ok()
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
        let err = giong::Builder::new().build().err().expect("build must fail");
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

/// CI's container job sets `GIONG_TEST_EXPECT_DRIVER=epoll`: inside Docker's
/// default seccomp profile, the default runtime must have chosen epoll.
#[test]
fn the_default_driver_is_the_expected_one() {
    let Ok(expected) = std::env::var("GIONG_TEST_EXPECT_DRIVER") else {
        return;
    };
    let got = giong::Builder::new()
        .block_on(async { giong::driver_kind() })
        .unwrap();
    assert_eq!(got.name(), expected);
}
