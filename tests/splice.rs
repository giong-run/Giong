//! `splice_to`: bytes moved from one stream to another inside the kernel.

use std::net::{Shutdown, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream, UnixStream};
use giong::time::{sleep, timeout};

fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
}

/// A connected pair of TCP streams on this worker.
async fn tcp_pair() -> (TcpStream, TcpStream) {
    let (l, addr) = listener();
    let connect = giong::spawn(async move { TcpStream::connect(addr).await.unwrap() });
    let b = l.accept().await.unwrap();
    (connect.await.unwrap(), b)
}

/// Deterministic payload.
fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| {
            (i as u32)
                .wrapping_mul(2_654_435_761)
                .wrapping_add(seed as u32) as u8
        })
        .collect()
}

async fn read_to_end(s: &TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let (r, buf) = s.read(Vec::with_capacity(64 * 1024)).await;
        if r.unwrap() == 0 {
            return out;
        }
        out.extend_from_slice(&buf);
    }
}

fn assert_idle() {
    let stats = giong::observe::worker_stats();
    assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
}

/// Eight MiB through a splicing proxy arrive intact and in order, and the
/// call returns the count when the source ends.
#[test]
fn a_spliced_transfer_is_intact() {
    const LEN: usize = 8 * 1024 * 1024;
    giong::block_on(async {
        let (client, proxy_in) = tcp_pair().await;
        let (proxy_out, sink) = tcp_pair().await;
        let writer = giong::spawn(async move {
            client.write_all(payload(LEN, 1)).await.0.unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            client
        });
        let reader = giong::spawn(async move { read_to_end(&sink).await });
        let moved = proxy_in.splice_to(&proxy_out, u64::MAX).await.unwrap();
        assert_eq!(moved, LEN as u64);
        proxy_out.shutdown(Shutdown::Write).unwrap();
        let got = reader.await.unwrap();
        assert!(got == payload(LEN, 1), "spliced bytes differ");
        drop(writer.await.unwrap());
        drop((proxy_in, proxy_out));
        sleep(Duration::from_millis(20)).await;
        assert_idle();
    });
}

/// With a limit, exactly that many bytes move; the rest stay readable on
/// the source.
#[test]
fn a_splice_stops_at_its_limit() {
    giong::block_on(async {
        let (client, proxy_in) = tcp_pair().await;
        let (proxy_out, sink) = tcp_pair().await;
        client.write_all(payload(1000, 2)).await.0.unwrap();
        let moved = proxy_in.splice_to(&proxy_out, 100).await.unwrap();
        assert_eq!(moved, 100);
        proxy_out.shutdown(Shutdown::Write).unwrap();
        assert_eq!(read_to_end(&sink).await, payload(1000, 2)[..100]);
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(read_to_end(&proxy_in).await, payload(1000, 2)[100..]);
        assert_eq!(proxy_in.splice_to(&proxy_out, 0).await.unwrap(), 0);
    });
}

/// TCP to a Unix stream and back: either kind can be either end.
#[test]
fn a_splice_works_between_tcp_and_unix_streams() {
    const LEN: usize = 1024 * 1024;
    giong::block_on(async {
        let (client, proxy_in) = tcp_pair().await;
        let (unix_a, unix_b) = UnixStream::pair().unwrap();
        let (proxy_out, sink) = tcp_pair().await;
        let writer = giong::spawn(async move {
            client.write_all(payload(LEN, 3)).await.0.unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            client
        });
        let hop = giong::spawn(async move {
            let n = proxy_in.splice_to(&unix_a, u64::MAX).await.unwrap();
            unix_a.shutdown(Shutdown::Write).unwrap();
            n
        });
        let reader = giong::spawn(async move { read_to_end(&sink).await });
        assert_eq!(
            unix_b.splice_to(&proxy_out, u64::MAX).await.unwrap(),
            LEN as u64
        );
        proxy_out.shutdown(Shutdown::Write).unwrap();
        assert_eq!(hop.await.unwrap(), LEN as u64);
        assert!(reader.await.unwrap() == payload(LEN, 3));
        drop(writer.await.unwrap());
    });
}

/// After `recv` has read from a stream, splicing from it would skip what
/// `recv` took in: it fails with GIONG-E0006 instead.
#[test]
fn splice_after_recv_is_refused_with_its_code() {
    giong::block_on(async {
        let (client, proxy_in) = tcp_pair().await;
        let (proxy_out, _sink) = tcp_pair().await;
        client.write_all(&b"hello"[..]).await.0.unwrap();
        let got = proxy_in.recv().await.unwrap();
        assert_eq!(&got[..], b"hello");
        drop(got);
        let err = proxy_in.splice_to(&proxy_out, u64::MAX).await.unwrap_err();
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::MixedReads),
            "{err}"
        );
        assert!(err.to_string().starts_with("GIONG-E0006"), "{err}");
    });
}

/// Dropping a splice that waits for data leaves no operation behind, and
/// the streams still work.
#[test]
fn dropping_a_waiting_splice_leaves_nothing_behind() {
    giong::block_on(async {
        let (client, proxy_in) = tcp_pair().await;
        let (proxy_out, sink) = tcp_pair().await;
        let (proxy_in, proxy_out) = (Rc::new(proxy_in), Rc::new(proxy_out));
        for _ in 0..20 {
            let res = timeout(
                Duration::from_micros(300),
                proxy_in.splice_to(&*proxy_out, u64::MAX),
            )
            .await;
            assert!(res.is_err(), "nothing was sent, so it waits");
        }
        sleep(Duration::from_millis(20)).await;
        assert_idle();
        client.write_all(&b"after"[..]).await.0.unwrap();
        assert_eq!(proxy_in.splice_to(&*proxy_out, 5).await.unwrap(), 5);
        proxy_out.shutdown(Shutdown::Write).unwrap();
        assert_eq!(read_to_end(&sink).await, b"after");
    });
}
