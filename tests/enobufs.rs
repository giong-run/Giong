//! Sends, connects and new sockets that get `ENOBUFS`, as on macOS when
//! the system's network buffers run out: giong tries them again on a timer
//! for up to 2 s, then fails with GIONG-E0028. The failures come from
//! the test hook, so this file is built only with
//! `--cfg giong_inject_enobufs`; on Linux it runs on the epoll driver (or on
//! kqueue through libkqueue, with `GIONG_DRIVER=kqueue`).
#![cfg(giong_inject_enobufs)]

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use giong::error::{self, Code};
use giong::inject_enobufs as hook;
use giong::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use giong::time::{sleep, timeout};

/// The hook's counters are process-wide: one test at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Hooked(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Drop for Hooked {
    fn drop(&mut self) {
        hook::reset();
    }
}

fn hooked() -> Hooked {
    let guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    hook::reset();
    Hooked(guard)
}

/// Runs `fut` on the system's readiness driver, or on the one
/// `GIONG_DRIVER` names.
///
/// With precise timers: the tests count the retries' waits, and macOS adds
/// slack to a wait's time limit otherwise. Each 32 ms wait then ended about
/// 9 ms late on the CI runners, and a write gave up after 49 tries in 2 s,
/// not 67 (#482). The schedule is what the tests check; Linux and Windows
/// ignore the option.
fn run<F: Future>(fut: F) -> F::Output {
    let builder = giong::Builder::new().precise_timers(true);
    #[cfg(target_os = "linux")]
    let builder = if std::env::var_os("GIONG_DRIVER").is_some() {
        builder
    } else {
        builder.driver(giong::DriverKind::Epoll)
    };
    builder
        .block_on(async {
            // io_uring has no such failure, and no retry.
            assert_ne!(giong::driver_kind(), giong::DriverKind::IoUring);
            fut.await
        })
        .unwrap()
}

/// A connected pair: (client, server).
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).await.unwrap();
    let server = listener.accept().await.unwrap();
    (client, server)
}

/// Reads `n` bytes.
async fn read_n(stream: &TcpStream, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let buf = stream.recv().await.unwrap();
        assert!(!buf.is_empty(), "EOF after {} of {n} bytes", out.len());
        out.extend_from_slice(&buf);
    }
    out
}

/// Reads to the end.
async fn read_to_end(stream: &TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let buf = stream.recv().await.unwrap();
        if buf.is_empty() {
            return out;
        }
        out.extend_from_slice(&buf);
    }
}

fn assert_exhausted(err: &io::Error) {
    assert_eq!(
        error::code(err),
        Some(Code::NetworkBuffersExhausted),
        "{err}"
    );
    assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
    let source = std::error::Error::source(err.get_ref().unwrap())
        .and_then(|s| s.downcast_ref::<io::Error>())
        .expect("the kernel's error is the source");
    assert_eq!(source.raw_os_error(), Some(libc::ENOBUFS));
}

#[test]
fn a_write_succeeds_once_buffers_come_back() {
    let _h = hooked();
    run(async {
        let (client, server) = pair().await;
        hook::fail_sends(5);
        let start = Instant::now();
        let (res, _) = client.write_all(b"hello, pool".to_vec()).await;
        res.unwrap();
        let waited = start.elapsed();
        assert_eq!(hook::handed_out(), 5);
        // 1 + 2 + 4 + 8 + 16 ms, each at least three quarters of that.
        assert!(waited >= Duration::from_millis(23), "{waited:?}");
        assert_eq!(read_n(&server, 11).await, b"hello, pool");
    });
}

#[test]
fn a_write_fails_with_the_code_after_two_seconds() {
    let _h = hooked();
    run(async {
        let (client, server) = pair().await;
        hook::fail_sends(u32::MAX);
        let start = Instant::now();
        let (res, _) = client.write(b"never".to_vec()).await;
        let waited = start.elapsed();
        assert_exhausted(&res.unwrap_err());
        assert!(
            waited >= Duration::from_millis(1950) && waited < Duration::from_secs(3),
            "{waited:?}"
        );
        // Steps of at most 40 ms after the first few: no spinning.
        let tries = hook::handed_out();
        assert!((50..=100).contains(&tries), "{tries} tries in {waited:?}");
        // The stream is still usable once buffers are back.
        hook::reset();
        let (res, _) = client.write_all(b"after".to_vec()).await;
        res.unwrap();
        assert_eq!(read_n(&server, 5).await, b"after");
    });
}

#[test]
fn a_timeout_ends_a_waiting_write_with_nothing_sent() {
    let _h = hooked();
    run(async {
        let (client, server) = pair().await;
        hook::fail_sends(u32::MAX);
        let start = Instant::now();
        let res = timeout(
            Duration::from_millis(50),
            client.write_all(b"lost".to_vec()),
        )
        .await;
        assert!(res.is_err(), "the timeout fires first");
        assert!(start.elapsed() < Duration::from_millis(500));
        hook::reset();
        client.write_all(b"after".to_vec()).await.0.unwrap();
        drop(client);
        assert_eq!(read_to_end(&server).await, b"after");
    });
}

#[test]
fn a_write_all_dropped_while_waiting_leaves_the_stream_usable() {
    let _h = hooked();
    run(async {
        let (client, server) = pair().await;
        // More than the socket buffers hold: the write waits for the
        // reader, part of it sent.
        let big: Vec<u8> = (0..16 << 20).map(|i: u32| (i % 251) as u8).collect();
        let mut write = Box::pin(client.write_all(big.clone()));
        assert!(timeout(Duration::from_millis(50), &mut write)
            .await
            .is_err());
        // Then the pool runs dry, and the reader starts draining.
        hook::fail_sends(u32::MAX);
        let reader = giong::spawn(async move { read_to_end(&server).await });
        assert!(timeout(Duration::from_millis(300), &mut write)
            .await
            .is_err());
        assert!(hook::handed_out() > 0, "the write never met the shortage");
        drop(write);
        hook::reset();
        client.write_all(b"tail".to_vec()).await.0.unwrap();
        drop(client);
        let got = reader.await.unwrap();
        let (head, tail) = got.split_at(got.len() - 4);
        assert_eq!(tail, b"tail");
        assert!(!head.is_empty() && head.len() < big.len());
        assert!(
            head == &big[..head.len()],
            "the bytes before are delivered in order"
        );
    });
}

#[test]
fn other_tasks_run_while_a_write_waits() {
    let _h = hooked();
    run(async {
        let (client, _server) = pair().await;
        let ticks = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let counter = giong::spawn({
            let ticks = ticks.clone();
            async move {
                loop {
                    sleep(Duration::from_millis(1)).await;
                    ticks.set(ticks.get() + 1);
                }
            }
        });
        hook::fail_sends(u32::MAX);
        let res = timeout(Duration::from_millis(200), client.write(b"x".to_vec())).await;
        assert!(res.is_err());
        counter.abort();
        assert!(ticks.get() >= 20, "{} ticks", ticks.get());
        // 1 + 2 + 4 + 8 + 16 + 32 + ... ms: about a dozen tries in 200 ms.
        let tries = hook::handed_out();
        assert!((5..=25).contains(&tries), "{tries} tries");
    });
}

#[test]
fn a_refused_connect_is_made_again() {
    let _h = hooked();
    run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        hook::fail_connects(5);
        let client = TcpStream::connect(addr).await.unwrap();
        assert_eq!(hook::handed_out(), 5);
        let server = listener.accept().await.unwrap();
        client.write_all(b"up".to_vec()).await.0.unwrap();
        assert_eq!(read_n(&server, 2).await, b"up");
    });
}

#[test]
fn a_socket_refused_for_want_of_buffers_is_made_again() {
    let _h = hooked();
    run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        hook::fail_sockets(5);
        let start = Instant::now();
        let client = TcpStream::connect(addr).await.unwrap();
        let waited = start.elapsed();
        assert_eq!(hook::handed_out(), 5);
        // 1 + 2 + 4 + 8 + 16 ms.
        assert!(waited >= Duration::from_millis(31), "{waited:?}");
        let server = listener.accept().await.unwrap();
        client.write_all(b"up".to_vec()).await.0.unwrap();
        assert_eq!(read_n(&server, 2).await, b"up");
    });
}

#[test]
fn a_socket_refused_for_two_seconds_fails_with_the_code() {
    let _h = hooked();
    run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        hook::fail_sockets(u32::MAX);
        let start = Instant::now();
        let err = TcpStream::connect(addr).await.map(drop).unwrap_err();
        let waited = start.elapsed();
        assert_exhausted(&err);
        assert!(
            waited >= Duration::from_millis(1900) && waited < Duration::from_millis(2500),
            "{waited:?}"
        );
        let tries = hook::handed_out();
        assert!((50..=100).contains(&tries), "{tries} tries in {waited:?}");
        // Sockets come back: the next connect is made at once.
        hook::reset();
        let client = TcpStream::connect(addr).await.unwrap();
        let server = listener.accept().await.unwrap();
        client.write_all(b"up".to_vec()).await.0.unwrap();
        assert_eq!(read_n(&server, 2).await, b"up");
    });
}

#[test]
fn a_refused_unix_connect_is_made_again() {
    let _h = hooked();
    let dir = std::env::temp_dir().join(format!("giong-enobufs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sock");
    let _ = std::fs::remove_file(&path);
    run(async {
        let listener = UnixListener::bind(&path).unwrap();
        hook::fail_sockets(2);
        hook::fail_connects(3);
        let client = UnixStream::connect(&path).await.unwrap();
        assert_eq!(hook::handed_out(), 5);
        let server = listener.accept().await.unwrap();
        client.write_all(b"up".to_vec()).await.0.unwrap();
        assert_eq!(&server.recv().await.unwrap()[..], b"up");
    });
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
// The new socket gets the number of the one just closed, and libkqueue
// (Linux's stand-in for kqueue) loses track of a number reused that fast:
// it cannot see a close, and the new socket's connect waits for an event it
// never reports. The kernel's kqueue removes a closed socket's filters.
#[cfg_attr(
    giong_libkqueue,
    ignore = "libkqueue loses events of a reused descriptor"
)]
fn a_connect_broken_after_it_began_starts_over_on_a_new_socket() {
    let _h = hooked();
    run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        hook::fail_connects_late(2);
        let client = TcpStream::connect(addr).await.unwrap();
        assert_eq!(hook::handed_out(), 2);
        let mine = client.local_addr().unwrap();
        // The two broken connections reached the listener first, and were
        // closed; the third is the client's.
        loop {
            let server = listener.accept().await.unwrap();
            if server.peer_addr().unwrap() == mine {
                client.write_all(b"up".to_vec()).await.0.unwrap();
                assert_eq!(read_n(&server, 2).await, b"up");
                break;
            }
            assert!(server.recv().await.map_or(true, |b| b.is_empty()));
        }
    });
}

#[test]
#[cfg_attr(
    giong_libkqueue,
    ignore = "libkqueue loses events of a reused descriptor"
)]
fn a_connect_that_keeps_breaking_fails_with_the_code() {
    let _h = hooked();
    run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        hook::fail_connects_late(u32::MAX);
        let start = Instant::now();
        let err = TcpStream::connect(addr).await.map(drop).unwrap_err();
        let waited = start.elapsed();
        assert_exhausted(&err);
        assert!(
            waited >= Duration::from_millis(1900) && waited < Duration::from_secs(3),
            "{waited:?}"
        );
        let tries = hook::handed_out();
        assert!((50..=100).contains(&tries), "{tries} sockets in {waited:?}");
        drop(listener);
    });
}

#[test]
#[cfg_attr(
    giong_libkqueue,
    ignore = "libkqueue loses events of a reused descriptor"
)]
fn a_connect_waits_two_seconds_in_all() {
    let _h = hooked();
    run(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Broken after it began, then refused on every new socket: the
        // driver's wait on the new socket gets what is left of the 2 s, not
        // 2 s of its own.
        hook::fail_connects_late(1);
        hook::fail_connects(u32::MAX);
        let start = Instant::now();
        let err = TcpStream::connect(addr).await.map(drop).unwrap_err();
        let waited = start.elapsed();
        assert_exhausted(&err);
        assert!(
            waited >= Duration::from_millis(1900) && waited < Duration::from_millis(2500),
            "{waited:?}"
        );
        drop(listener);
    });
}
