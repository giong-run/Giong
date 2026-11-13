//! How `Compat` behaves through the Tokio and futures-io traits: reads,
//! buffered writes, flushes, errors, shutdown, cancellation and leaks.
//!
//! The tests run on whichever driver giong picks; `scripts/check.sh fallback`
//! runs them on the older ring setups and on epoll too.

use std::net::SocketAddr;
use std::time::Duration;

use giong::error::{code, Code};
use giong::net::{TcpListener, TcpStream, UnixStream};
use giong::time::{sleep, timeout};
use giong_compat::{Compat, DEFAULT_WRITE_BUFFER};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const LIMIT: Duration = Duration::from_secs(20);

fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
}

/// A connected pair: (client, server).
async fn pair() -> (TcpStream, TcpStream) {
    let (l, addr) = listener();
    let c = TcpStream::connect(addr).await.unwrap();
    let s = l.accept().await.unwrap();
    (c, s)
}

/// Echoes with giong's own API until the peer closes, then closes.
async fn native_echo(s: TcpStream) {
    loop {
        let Ok(data) = s.recv().await else { return };
        if data.is_empty() {
            return;
        }
        let copy = data.to_vec();
        drop(data);
        if s.write_all(copy).await.0.is_err() {
            return;
        }
    }
}

/// Reads with giong's own API until the peer closes.
async fn native_read_all(s: &TcpStream) -> Vec<u8> {
    let mut all = Vec::new();
    loop {
        let data = s.recv().await.unwrap();
        if data.is_empty() {
            return all;
        }
        all.extend_from_slice(&data);
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 251) as u8).collect()
}

/// Lets the runtime turn a few times, so that cancellations complete.
async fn settle() {
    for _ in 0..5 {
        sleep(Duration::from_millis(5)).await;
    }
}

#[test]
fn echo_through_the_tokio_traits() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let server = giong::spawn(native_echo(s));
        let mut c = Compat::new(c);
        for len in [1, 100, 4096, 8192, 70_000, 300_000] {
            let msg = pattern(len);
            c.write_all(&msg).await.unwrap();
            c.flush().await.unwrap();
            let mut back = vec![0; len];
            timeout(LIMIT, c.read_exact(&mut back))
                .await
                .unwrap()
                .unwrap();
            assert!(back == msg, "{len} bytes came back different");
        }
        c.shutdown().await.unwrap();
        let mut rest = Vec::new();
        assert_eq!(c.read_to_end(&mut rest).await.unwrap(), 0);
        server.await.unwrap();
    });
}

#[test]
fn a_partly_read_chunk_does_not_hold_a_pool_buffer() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let msg = pattern(1000);
        s.write_all(msg.clone()).await.0.unwrap();
        let mut c = Compat::new(c);
        let mut first = [0; 10];
        c.read_exact(&mut first).await.unwrap();
        assert_eq!(first, msg[..10]);
        // The reader now does something else for a while: the rest of the
        // chunk must not keep a buffer of the worker's pool.
        settle().await;
        let stats = giong::stats();
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
        // One byte at a time, then all of it is there.
        let mut rest = Vec::new();
        let mut one = [0; 1];
        while rest.len() < 990 {
            assert_eq!(c.read(&mut one).await.unwrap(), 1);
            rest.push(one[0]);
        }
        assert_eq!(rest, msg[10..]);
    });
}

#[test]
fn buf_read_hands_out_received_bytes_and_moves_a_partly_consumed_rest() {
    giong::block_on(async {
        let (c, s) = pair().await;
        s.write_all(&b"alpha\nbeta\ngamma"[..]).await.0.unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut c = Compat::new(c);
        let mut lines = Vec::new();
        let mut line = String::new();
        while c.read_line(&mut line).await.unwrap() > 0 {
            lines.push(std::mem::take(&mut line));
            settle().await;
            let stats = giong::stats();
            assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
        }
        assert_eq!(lines, ["alpha\n", "beta\n", "gamma"]);
        // fill_buf at the end of the stream is empty, again and again.
        for _ in 0..3 {
            assert!(c.fill_buf().await.unwrap().is_empty());
        }
    });
}

#[test]
fn writes_wait_for_a_flush() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let mut c = Compat::new(c);
        c.write_all(b"held").await.unwrap();
        // Nothing reaches the peer before the flush ...
        assert!(timeout(Duration::from_millis(100), s.recv()).await.is_err());
        // ... and all of it right after.
        c.flush().await.unwrap();
        let data = timeout(LIMIT, s.recv()).await.unwrap().unwrap();
        assert_eq!(&*data, b"held");
    });
}

#[test]
fn a_read_that_waits_sends_what_was_written() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let server = giong::spawn(native_echo(s));
        let mut c = Compat::new(c);
        // A request/response client that forgets to flush still works:
        // waiting for the answer sends the question.
        for round in 0..50u32 {
            let msg = format!("question {round}");
            c.write_all(msg.as_bytes()).await.unwrap();
            let mut back = vec![0; msg.len()];
            timeout(LIMIT, c.read_exact(&mut back))
                .await
                .expect("the read waited without sending the request")
                .unwrap();
            assert_eq!(back, msg.as_bytes());
        }
        drop(c);
        server.await.unwrap();
    });
}

#[test]
fn a_large_transfer_with_a_small_buffer_and_separate_reader_and_writer_tasks() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let server = giong::spawn(native_echo(s));
        let (mut rd, mut wr) = tokio::io::split(Compat::new(c).write_buffer(4096));
        let data = pattern(8 << 20);
        let sent = data.clone();
        // The writer and the reader are different tasks, so each must be
        // woken by its own side of the stream.
        let writer = giong::spawn(async move {
            for piece in sent.chunks(10_007) {
                wr.write_all(piece).await.unwrap();
            }
            wr.flush().await.unwrap();
            wr.shutdown().await.unwrap();
        });
        let mut back = Vec::new();
        timeout(Duration::from_secs(60), rd.read_to_end(&mut back))
            .await
            .unwrap()
            .unwrap();
        assert!(
            back == data,
            "{} of {} bytes came back",
            back.len(),
            data.len()
        );
        writer.await.unwrap();
        server.await.unwrap();
    });
}

#[test]
fn a_failed_send_is_reported_once_as_is_then_with_a_code() {
    giong::block_on(async {
        let (c, s) = pair().await;
        // The peer resets the connection: no lingering on close.
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: a valid descriptor and a `linger` of the right size.
        let rc = unsafe {
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&s),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                (&linger as *const libc::linger).cast(),
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0);
        drop(s);
        settle().await;
        let mut c = Compat::new(c);
        let chunk = vec![7u8; 64 * 1024];
        let first = loop {
            c.write_all(&chunk)
                .await
                .unwrap_or_else(|e| panic!("write: {e}"));
            if let Err(e) = c.flush().await {
                break e;
            }
        };
        assert_eq!(
            code(&first),
            None,
            "the kernel's own error comes first: {first}"
        );
        let later = c.write_all(b"more").await.unwrap_err();
        assert_eq!(code(&later), Some(Code::CompatWriteFailed), "{later}");
        assert!(later.to_string().contains(&first.to_string()), "{later}");
        let flush = c.flush().await.unwrap_err();
        assert_eq!(code(&flush), Some(Code::CompatWriteFailed), "{flush}");
        let shut = c.shutdown().await.unwrap_err();
        assert_eq!(code(&shut), Some(Code::CompatWriteFailed), "{shut}");
    });
}

#[test]
fn writing_after_shutdown_fails_with_a_code_and_reading_goes_on() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let mut c = Compat::new(c);
        c.write_all(b"last words").await.unwrap();
        c.shutdown().await.unwrap();
        c.shutdown().await.unwrap(); // twice is fine
        c.flush().await.unwrap();
        let err = c.write_all(b"more").await.unwrap_err();
        assert_eq!(code(&err), Some(Code::CompatShutDown), "{err}");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        // The peer got everything, then the end of the stream ...
        assert_eq!(native_read_all(&s).await, b"last words");
        // ... and can still answer.
        s.write_all(&b"reply"[..]).await.0.unwrap();
        drop(s);
        let mut reply = String::new();
        c.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, "reply");
        // After the end, reads keep returning nothing.
        let mut buf = [0; 8];
        for _ in 0..3 {
            assert_eq!(c.read(&mut buf).await.unwrap(), 0);
        }
    });
}

#[test]
#[should_panic(expected = "GIONG-E1403")]
fn a_zero_write_buffer_is_refused_with_a_code() {
    let (a, _b) = UnixStream::pair().unwrap();
    let _ = Compat::new(a).write_buffer(0);
}

#[test]
#[should_panic(expected = "GIONG-E1403")]
fn a_write_buffer_above_a_gibibyte_is_refused_with_a_code() {
    let (a, _b) = UnixStream::pair().unwrap();
    let _ = Compat::new(a).write_buffer(giong_compat::MAX_WRITE_BUFFER + 1);
}

#[test]
fn debug_shows_the_settings_and_the_state() {
    let (a, _b) = UnixStream::pair().unwrap();
    let c = Compat::new(a);
    let text = format!("{c:?}");
    assert!(text.contains("UnixStream"), "{text}");
    assert!(
        text.contains(&format!("write_buffer: {DEFAULT_WRITE_BUFFER}")),
        "{text}"
    );
    assert!(text.contains("write_state: \"open\""), "{text}");
}

#[test]
fn vectored_writes_are_copied_in_order() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let mut c = Compat::new(c).write_buffer(10);
        assert!(tokio::io::AsyncWrite::is_write_vectored(&c));
        let parts = [
            std::io::IoSlice::new(b"abc"),
            std::io::IoSlice::new(b""),
            std::io::IoSlice::new(b"defghij"),
            std::io::IoSlice::new(b"klm"),
        ];
        // Up to the limit, in order.
        assert_eq!(c.write_vectored(&parts).await.unwrap(), 10);
        c.write_all(b"klm").await.unwrap();
        c.shutdown().await.unwrap();
        assert_eq!(native_read_all(&s).await, b"abcdefghijklm");
    });
}

#[test]
fn dropping_reads_writes_and_the_stream_midway_leaks_nothing() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let mut c = Compat::new(c);
        // A read that is dropped while waiting loses nothing.
        let mut buf = [0; 16];
        for _ in 0..20 {
            assert!(timeout(Duration::from_millis(1), c.read(&mut buf))
                .await
                .is_err());
        }
        s.write_all(&b"still here"[..]).await.0.unwrap();
        let n = timeout(LIMIT, c.read(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"still here");
        // A flush that cannot finish (the peer does not read) is dropped,
        // then the whole stream with sends in flight.
        let big = vec![1u8; 64 << 20];
        let flushed = timeout(Duration::from_millis(50), async {
            c.write_all(&big).await.unwrap();
            c.flush().await.unwrap();
        })
        .await;
        assert!(flushed.is_err(), "64 MiB fit in the socket buffers");
        drop(c);
        drop(s);
        settle().await;
        let stats = giong::stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
        assert_eq!(stats.tasks, 0, "{stats:?}");
    });
}

#[test]
fn into_inner_flushes_and_hands_back_the_unread_bytes() {
    giong::block_on(async {
        let (c, s) = pair().await;
        s.write_all(&b"hello world"[..]).await.0.unwrap();
        let mut c = Compat::new(c);
        let mut hello = [0; 5];
        c.read_exact(&mut hello).await.unwrap();
        assert_eq!(&hello, b"hello");
        c.write_all(b"unflushed").await.unwrap();
        let (tcp, unread) = c.into_inner().await.unwrap();
        assert_eq!(unread, b" world");
        // The stream goes on with giong's own API.
        let got = timeout(LIMIT, s.recv()).await.unwrap().unwrap();
        assert_eq!(&*got, b"unflushed");
        drop(got);
        tcp.write_all(&b"native"[..]).await.0.unwrap();
        let got = timeout(LIMIT, s.recv()).await.unwrap().unwrap();
        assert_eq!(&*got, b"native");
    });
}

#[test]
fn unix_streams_work_the_same() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut a, mut b) = (Compat::new(a), Compat::new(b));
        let data = pattern(1 << 20);
        let sent = data.clone();
        let writer = giong::spawn(async move {
            a.write_all(&sent).await.unwrap();
            a.shutdown().await.unwrap();
        });
        let mut back = Vec::new();
        timeout(LIMIT, b.read_to_end(&mut back))
            .await
            .unwrap()
            .unwrap();
        assert!(back == data);
        writer.await.unwrap();
    });
}

/// The futures-io traits, in a module of their own: their extension traits
/// have the same method names as Tokio's.
mod futures_traits {
    use super::*;
    use futures_util::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    #[test]
    fn echo_through_the_futures_traits() {
        giong::block_on(async {
            let (c, s) = pair().await;
            let server = giong::spawn(native_echo(s));
            let mut c = Compat::new(c);
            c.write_all(b"one\ntwo\n").await.unwrap();
            c.flush().await.unwrap();
            let mut line = String::new();
            c.read_line(&mut line).await.unwrap();
            assert_eq!(line, "one\n");
            let mut two = [0; 4];
            c.read_exact(&mut two).await.unwrap();
            assert_eq!(&two, b"two\n");
            let big = pattern(200_000);
            c.write_all(&big).await.unwrap();
            c.flush().await.unwrap();
            let mut back = vec![0; big.len()];
            timeout(LIMIT, c.read_exact(&mut back))
                .await
                .unwrap()
                .unwrap();
            assert!(back == big);
            c.close().await.unwrap();
            let mut rest = Vec::new();
            assert_eq!(c.read_to_end(&mut rest).await.unwrap(), 0);
            server.await.unwrap();
        });
    }
}

#[test]
fn wrapping_a_tcp_stream_turns_off_nagle() {
    giong::block_on(async {
        let (c, _s) = pair().await;
        let mut on: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&c);
        let c = Compat::new(c);
        // SAFETY: a valid descriptor, and an int-sized option buffer.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_NODELAY,
                (&mut on as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(on, 1, "TCP_NODELAY is off");
        drop(c);
    });
}
