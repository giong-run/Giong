//! Cancellation safety and resource exhaustion.
//!
//! These are the cases a completion-based runtime gets wrong most easily:
//! futures dropped while the kernel still owns their buffers, and the shared
//! receive pool running dry.

use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong::time::{sleep, timeout};

fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
}

async fn pair() -> (TcpStream, TcpStream) {
    let (l, addr) = listener();
    let c = TcpStream::connect(addr).await.unwrap();
    let s = l.accept().await.unwrap();
    (c, s)
}

#[test]
fn recv_is_cancel_safe_and_loses_no_data() {
    giong::block_on(async {
        let (c, s) = pair().await;
        // Time out a few times with nothing to read.
        for _ in 0..5 {
            assert!(timeout(Duration::from_millis(2), s.recv()).await.is_err());
        }
        // Data sent while nobody is awaiting must be delivered to the next call.
        c.write_all(&b"first"[..]).await.0.unwrap();
        sleep(Duration::from_millis(20)).await;
        assert!(
            timeout(Duration::from_millis(1), sleep(Duration::from_secs(1)))
                .await
                .is_err()
        );
        let data = timeout(Duration::from_secs(5), s.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&*data, b"first");
    });
}

#[test]
fn dropping_an_in_flight_read_keeps_its_buffer_alive() {
    giong::block_on(async {
        let (c, s) = pair().await;
        // Start single-shot reads with owned buffers and abandon them mid-flight.
        // The kernel still holds pointers into those buffers; the driver must
        // keep them alive until the cancellation is confirmed.
        for _ in 0..200 {
            let res = timeout(Duration::from_micros(200), s.read(Vec::with_capacity(4096))).await;
            assert!(res.is_err());
        }
        // The stream still works afterwards.
        c.write_all(&b"still alive"[..]).await.0.unwrap();
        let (res, buf) = s.read(Vec::with_capacity(64)).await;
        assert_eq!(res.unwrap(), 11);
        assert_eq!(buf, b"still alive");

        sleep(Duration::from_millis(20)).await;
        let stats = giong::stats();
        assert_eq!(
            stats.tracked_ops, 0,
            "cancelled reads were not cleaned up: {stats:?}"
        );
    });
}

#[test]
fn dropping_an_in_flight_vectored_write_keeps_its_buffers_alive() {
    giong::block_on(async {
        let (c, s) = pair().await;
        // Fill the socket buffers so writes stay pending, then abandon them.
        let mut abandoned = 0;
        for _ in 0..20 {
            let pieces: Vec<Vec<u8>> = (0..4).map(|_| vec![7u8; 1024 * 1024]).collect();
            if timeout(Duration::from_millis(2), c.write_all_vectored(pieces))
                .await
                .is_err()
            {
                abandoned += 1;
            }
        }
        assert!(abandoned > 0, "expected at least one write to block");
        // Drain on the other side; every byte that arrives must be a 7.
        let mut total = 0usize;
        while let Ok(Ok(data)) = timeout(Duration::from_millis(100), s.recv()).await {
            if data.is_empty() {
                break;
            }
            assert!(data.iter().all(|&b| b == 7));
            total += data.len();
        }
        assert!(total > 0);
        drop((c, s));
        sleep(Duration::from_millis(20)).await;
        let stats = giong::stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn dropping_an_in_flight_write_keeps_its_buffer_alive() {
    giong::block_on(async {
        let (c, s) = pair().await;
        // Fill the socket buffers so writes stay pending, then abandon them.
        let big = vec![7u8; 4 * 1024 * 1024];
        let mut abandoned = 0;
        for _ in 0..20 {
            if timeout(Duration::from_millis(2), c.write_all(big.clone()))
                .await
                .is_err()
            {
                abandoned += 1;
            }
        }
        assert!(abandoned > 0, "expected at least one write to block");
        // Drain on the other side; every byte that arrives must be a 7.
        let mut total = 0usize;
        while let Ok(Ok(data)) = timeout(Duration::from_millis(100), s.recv()).await {
            if data.is_empty() {
                break;
            }
            assert!(data.iter().all(|&b| b == 7));
            total += data.len();
        }
        assert!(total > 0);
        drop((c, s));
        sleep(Duration::from_millis(20)).await;
        let stats = giong::stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

#[test]
fn aborting_a_task_blocked_on_io_cleans_up() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let reader = giong::spawn(async move {
            let (_res, _buf) = s.read(Vec::with_capacity(1024)).await;
            unreachable!("nothing is ever sent");
        });
        sleep(Duration::from_millis(10)).await;
        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        // The server side was dropped with the task: the client sees EOF.
        let eof = timeout(Duration::from_secs(5), c.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(eof.is_empty());
        drop(c);
        sleep(Duration::from_millis(20)).await;
        assert_eq!(giong::stats().tracked_ops, 0);
    });
}

#[test]
fn receive_pool_exhaustion_recovers() {
    // A deliberately tiny pool: 4 buffers of 256 bytes for 32 connections.
    giong::Builder::new()
        .recv_buffers(4, 256)
        .block_on(async {
            const CONNS: usize = 32;
            const BYTES: usize = 64 * 1024;
            let (l, addr) = listener();

            let server = giong::spawn(async move {
                let mut readers = Vec::new();
                for _ in 0..CONNS {
                    let s = l.accept().await.unwrap();
                    readers.push(giong::spawn(async move {
                        let mut total = 0usize;
                        let mut sum = 0u64;
                        loop {
                            let data = s.recv().await.unwrap();
                            if data.is_empty() {
                                break;
                            }
                            total += data.len();
                            sum += data.iter().map(|&b| b as u64).sum::<u64>();
                            drop(data);
                            // Be a slow consumer every now and then.
                            if total % 7 == 0 {
                                giong::yield_now().await;
                            }
                        }
                        (total, sum)
                    }));
                }
                let mut results = Vec::new();
                for r in readers {
                    results.push(r.await.unwrap());
                }
                results
            });

            let mut writers = Vec::new();
            for i in 0..CONNS {
                writers.push(giong::spawn(async move {
                    let c = TcpStream::connect(addr).await.unwrap();
                    c.write_all(vec![(i % 251) as u8; BYTES]).await.0.unwrap();
                }));
            }
            for w in writers {
                w.await.unwrap();
            }

            let mut results = timeout(Duration::from_secs(30), server)
                .await
                .expect("readers starved: pool exhaustion did not recover")
                .unwrap();
            results.sort();
            let mut expected: Vec<(usize, u64)> = (0..CONNS)
                .map(|i| (BYTES, (i % 251) as u64 * BYTES as u64))
                .collect();
            expected.sort();
            assert_eq!(results, expected);

            sleep(Duration::from_millis(20)).await;
            let stats = giong::stats();
            assert_eq!(stats.free_recv_buffers, 4, "{stats:?}");
        })
        .unwrap();
}

#[test]
fn a_stalled_reader_does_not_starve_other_connections() {
    giong::Builder::new()
        .recv_buffers(16, 1024)
        .block_on(async {
            let (l, addr) = listener();
            let noisy_client = TcpStream::connect(addr).await.unwrap();
            let noisy_server = l.accept().await.unwrap();
            let quiet_client = TcpStream::connect(addr).await.unwrap();
            let quiet_server = l.accept().await.unwrap();

            // Arm the noisy connection's receive, then never read from it again
            // while its peer floods it.
            assert!(timeout(Duration::from_millis(5), noisy_server.recv())
                .await
                .is_err());
            let noisy_client = Rc::new(noisy_client);
            let flood = {
                let c = noisy_client.clone();
                giong::spawn(async move {
                    let _ = c.write_all(vec![1u8; 1024 * 1024]).await;
                })
            };
            sleep(Duration::from_millis(50)).await;

            // The quiet connection must still get buffers.
            for round in 0..20u8 {
                quiet_client.write_all(vec![round; 100]).await.0.unwrap();
                let data = timeout(Duration::from_secs(5), quiet_server.recv())
                    .await
                    .expect("quiet connection starved by a stalled one")
                    .unwrap();
                assert!(data.iter().all(|&b| b == round));
            }
            let stats = giong::stats();
            assert!(
                stats.free_recv_buffers > 0,
                "stalled reader took the whole pool: {stats:?}"
            );

            flood.abort();
            drop((noisy_server, quiet_server, quiet_client, noisy_client));
        })
        .unwrap();
}

#[test]
fn runtime_teardown_with_everything_in_flight() {
    // Leave reads, writes, accepts, timers and tasks pending and just exit.
    for _ in 0..20 {
        giong::block_on(async {
            let (l, addr) = listener();
            let c = TcpStream::connect(addr).await.unwrap();
            let s = l.accept().await.unwrap();
            giong::spawn(async move {
                let _ = s.read(Vec::with_capacity(4096)).await;
            });
            giong::spawn(async move {
                let _ = c.recv().await;
                let _ = c.write_all(vec![0u8; 8 * 1024 * 1024]).await;
            });
            giong::spawn(async move {
                let _ = l.accept().await;
            });
            giong::spawn(async {
                sleep(Duration::from_secs(3600)).await;
            });
            sleep(Duration::from_millis(2)).await;
        });
    }
}

#[test]
fn io_objects_may_outlive_the_runtime() {
    // Not something to do on purpose, but it must be safe.
    let (stream, listener, buf) = giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(&b"kept"[..]).await.0.unwrap();
        let buf = s.recv().await.unwrap();
        (s, l, buf)
    });
    assert_eq!(&*buf, b"kept");
    drop(buf);
    drop(stream);
    drop(listener);
    // And a new runtime on the same thread is unaffected.
    giong::block_on(async {
        let (c, s) = pair().await;
        c.write_all(&b"ok"[..]).await.0.unwrap();
        assert_eq!(&*s.recv().await.unwrap(), b"ok");
    });
}

/// A buffer that records when it is freed.
struct Tracked {
    data: Vec<u8>,
    freed: Rc<std::cell::Cell<bool>>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.freed.set(true);
    }
}

// SAFETY: delegates to `Vec<u8>`, whose heap storage does not move with the value.
unsafe impl giong::IoBuf for Tracked {
    fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }
    fn len(&self) -> usize {
        self.data.len()
    }
}

unsafe impl giong::IoBufMut for Tracked {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }
    fn capacity(&self) -> usize {
        self.data.capacity()
    }
    unsafe fn set_init(&mut self, n: usize) {
        self.data.set_len(n);
    }
}

#[test]
fn a_cancelled_operation_owns_its_buffer_until_the_kernel_confirms() {
    giong::block_on(async {
        let (_c, s) = pair().await;
        // Let the cancellation of `pair`'s listener settle first.
        sleep(Duration::from_millis(20)).await;
        assert_eq!(giong::stats().tracked_ops, 0);
        let freed = Rc::new(std::cell::Cell::new(false));
        let buf = Tracked {
            data: Vec::with_capacity(4096),
            freed: freed.clone(),
        };

        // Start a read that cannot complete, and drop it while it is in flight.
        assert!(timeout(Duration::from_millis(5), s.read(buf))
            .await
            .is_err());

        // The future is gone, but the kernel has not confirmed the cancellation
        // yet (we have not even re-entered the kernel): the buffer must still
        // be alive, owned by the driver.
        assert!(
            !freed.get(),
            "buffer freed while the kernel could still write to it"
        );
        // io_uring keeps the operation until the kernel confirms; epoll was
        // only waiting for readiness, so nothing can write to the buffer and
        // the operation is gone at once (the buffer is dropped at the next
        // turn of the event loop).
        let expected = match giong::driver_kind() {
            giong::DriverKind::IoUring => 1,
            _ => 0,
        };
        assert_eq!(giong::stats().tracked_ops, expected);

        // After the completion arrives, the buffer is released — no leak.
        sleep(Duration::from_millis(20)).await;
        assert!(freed.get(), "buffer of a cancelled operation was leaked");
        assert_eq!(giong::stats().tracked_ops, 0);
    });
}

#[test]
fn a_completed_but_uncollected_operation_releases_its_buffer() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let freed = Rc::new(std::cell::Cell::new(false));
        let buf = Tracked {
            data: Vec::with_capacity(64),
            freed: freed.clone(),
        };

        // Poll the read once so it is submitted, let it complete, then drop the
        // future without ever collecting the result.
        let mut read = Box::pin(s.read(buf));
        assert!(timeout(Duration::from_millis(1), &mut read).await.is_err());
        c.write_all(&b"data"[..]).await.0.unwrap();
        sleep(Duration::from_millis(20)).await;
        drop(read);
        sleep(Duration::from_millis(5)).await;

        assert!(freed.get());
        assert_eq!(giong::stats().tracked_ops, 0);
    });
}
