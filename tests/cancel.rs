//! Cancellation safety and resource exhaustion.
//!
//! These are the cases a completion-based runtime gets wrong most easily:
//! futures dropped while the kernel still owns their buffers, and the shared
//! receive pool running dry.

use std::future::Future;
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
        let stats = giong::observe::worker_stats();
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
        let stats = giong::observe::worker_stats();
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
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

// The control message is Linux's SO_TIMESTAMPING.
#[cfg(target_os = "linux")]
#[test]
fn dropping_an_in_flight_send_with_control_keeps_its_buffers_alive() {
    giong::block_on(async {
        let (c, s) = pair().await;
        // Fill the socket buffers so sends stay pending, then abandon them.
        // The control message (SO_TIMESTAMPING, no flags) must outlive the
        // send as much as the data.
        let big = vec![7u8; 4 * 1024 * 1024];
        // SAFETY: CMSG_SPACE and CMSG_LEN only compute sizes.
        let (space, len) = unsafe { (libc::CMSG_SPACE(4) as usize, libc::CMSG_LEN(4) as u64) };
        let mut control = vec![0u8; space];
        control[..8].copy_from_slice(&len.to_ne_bytes());
        control[8..12].copy_from_slice(&libc::SOL_SOCKET.to_ne_bytes());
        control[12..16].copy_from_slice(&libc::SO_TIMESTAMPING.to_ne_bytes());
        let mut abandoned = 0;
        for _ in 0..20 {
            let send = c.send_with_control(big.clone(), control.clone());
            if timeout(Duration::from_millis(2), send).await.is_err() {
                abandoned += 1;
            }
        }
        assert!(abandoned > 0, "expected at least one send to block");
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
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn a_dropped_send_file_leaves_nothing_behind() {
    giong::block_on(async {
        let (c, s) = pair().await;
        let path =
            std::env::temp_dir().join(format!("giong-cancel-{}-send-file", std::process::id()));
        std::fs::write(&path, vec![7u8; 8 * 1024 * 1024]).unwrap();
        let file = giong::fs::File::open(&path).await.unwrap();
        // Nobody reads: the sends fill the socket and wait until they are
        // dropped.
        let mut abandoned = 0;
        for _ in 0..20 {
            if timeout(
                Duration::from_millis(2),
                c.send_file(&file, 0, 8 * 1024 * 1024),
            )
            .await
            .is_err()
            {
                abandoned += 1;
            }
        }
        assert!(abandoned > 0, "expected at least one send to wait");
        let mut total = 0usize;
        while let Ok(Ok(data)) = timeout(Duration::from_millis(100), s.recv()).await {
            if data.is_empty() {
                break;
            }
            assert!(data.iter().all(|&b| b == 7));
            total += data.len();
        }
        assert!(total > 0);
        drop((c, s, file));
        std::fs::remove_file(&path).unwrap();
        sleep(Duration::from_millis(20)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
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
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
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
            let stats = giong::observe::worker_stats();
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

            // The quiet connection must still get buffers. A stream may hand
            // the 100 bytes over in more than one piece (where a buffer of
            // the packed ring fills up), so collect them all.
            for round in 0..20u8 {
                quiet_client.write_all(vec![round; 100]).await.0.unwrap();
                let mut got = 0;
                while got < 100 {
                    let data = timeout(Duration::from_secs(5), quiet_server.recv())
                        .await
                        .expect("quiet connection starved by a stalled one")
                        .unwrap();
                    assert!(!data.is_empty() && got + data.len() <= 100, "round {round}");
                    assert!(
                        data.iter().all(|&b| b == round),
                        "round {round}: {:?}",
                        &data[..]
                    );
                    got += data.len();
                }
            }
            let stats = giong::observe::worker_stats();
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
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
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
        assert_eq!(giong::observe::worker_stats().tracked_ops, expected);

        // After the completion arrives, the buffer is released — no leak.
        sleep(Duration::from_millis(20)).await;
        assert!(freed.get(), "buffer of a cancelled operation was leaked");
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
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
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
    });
}

/// How many descriptors of this process refer to `path`. (Counting all
/// descriptors would see those of tests running in parallel.)
#[cfg(target_os = "linux")]
fn open_fds(path: &std::path::Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target == path)
        .count()
}

/// How many descriptors of this process refer to `path`. macOS has no
/// `/proc`: the descriptors are listed in `/dev/fd` and `F_GETPATH` names
/// their file, with symbolic links resolved (`/var` is `/private/var`).
#[cfg(target_os = "macos")]
fn open_fds(path: &std::path::Path) -> usize {
    use std::os::unix::ffi::OsStrExt;
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let fds: Vec<i32> = std::fs::read_dir("/dev/fd")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    fds.into_iter()
        .filter(|&fd| {
            let mut buf = [0u8; libc::PATH_MAX as usize];
            // SAFETY: F_GETPATH writes at most PATH_MAX bytes into `buf`.
            if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } < 0 {
                return false;
            }
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            std::ffi::OsStr::from_bytes(&buf[..len]) == path.as_os_str()
        })
        .count()
}

#[test]
fn a_dropped_open_closes_the_descriptor_it_produced() {
    // The result of an open is a new descriptor. When the future was
    // dropped after the open was submitted, nobody took it and it stayed
    // open: one descriptor per cancelled open, until EMFILE.
    let path = std::env::temp_dir().join(format!("giong-cancel-open-{}", std::process::id()));
    std::fs::write(&path, b"x").unwrap();
    let path = path.canonicalize().unwrap();
    giong::block_on(async {
        // io_uring closes a dropped file through the ring, a turn later.
        drop(giong::fs::File::open(&path).await.unwrap());
        sleep(Duration::from_millis(5)).await;
        assert_eq!(open_fds(&path), 0);
        for round in 0..200 {
            let mut open = Box::pin(giong::fs::File::open(&path));
            // Submitted, then dropped: in flight, as when a timeout fires.
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(
                    open.as_mut().poll(cx).is_pending()
                ))
                .await
            );
            if round % 2 == 1 {
                // Completed but never collected.
                sleep(Duration::from_millis(2)).await;
            }
            drop(open);
        }
        // Let every open finish and its descriptor be closed.
        for _ in 0..100 {
            if open_fds(&path) == 0 && giong::observe::worker_stats().tracked_ops == 0 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(open_fds(&path), 0, "descriptors left open");
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
    });
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_dropped_close_still_closes_on_the_epoll_driver() {
    // The system's readiness driver: kqueue on macOS works the same way.
    #[cfg(target_os = "linux")]
    const READINESS: giong::DriverKind = giong::DriverKind::Epoll;
    #[cfg(target_os = "macos")]
    const READINESS: giong::DriverKind = giong::DriverKind::Kqueue;
    // The epoll driver closes on a blocking thread. A close still waiting
    // for a thread when its future was dropped was skipped as cancelled,
    // and the descriptor stayed open with nobody owning it.
    let dir = std::env::temp_dir().join(format!("giong-drop-close-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let path = dir.join("file");
    std::fs::write(&path, b"x").unwrap();
    // Opening a FIFO for reading blocks until a writer opens it: each open
    // holds a blocking thread.
    let fifo = dir.join("fifo");
    let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid C string.
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
    giong::Builder::new()
        .driver(READINESS)
        .block_on(async {
            let file = giong::fs::File::open(&path).await.unwrap();
            // More opens than the pool has threads (64): every thread
            // waits for the writer, and jobs queued after them wait too.
            let mut opens: Vec<_> = (0..64)
                .map(|_| Box::pin(giong::fs::File::open(&fifo)))
                .collect();
            for open in &mut opens {
                let open = open.as_mut();
                assert!(poll_once(open).await.is_pending());
            }
            // Queued behind the opens, then dropped, as when a timeout fires.
            let mut close = Box::pin(file.close());
            assert!(poll_once(close.as_mut()).await.is_pending());
            drop(close);
            assert_eq!(open_fds(&path), 1);
            // Let the opens finish; the close runs after them.
            let writer = std::thread::spawn({
                let fifo = fifo.clone();
                move || std::fs::OpenOptions::new().write(true).open(fifo).unwrap()
            });
            for open in opens {
                open.await.unwrap();
            }
            let writer = writer.join().unwrap();
            for _ in 0..200 {
                if open_fds(&path) == 0 && giong::observe::worker_stats().tracked_ops == 0 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
            drop(writer);
            assert_eq!(
                open_fds(&path),
                0,
                "a dropped close left its descriptor open"
            );
            assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
        })
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Polls `fut` once and reports what it returned.
async fn poll_once<F: Future>(fut: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    let mut fut = Some(fut);
    std::future::poll_fn(move |cx| std::task::Poll::Ready(fut.take().unwrap().poll(cx))).await
}

/// A waker that runs `f` the first time it is woken. The driver wakes tasks
/// while it is borrowed, so `f` runs inside the driver, as code in a
/// waker's `wake` or `drop` can.
fn waker_running(f: impl FnOnce() + 'static) -> std::task::Waker {
    use std::cell::RefCell;
    use std::task::{RawWaker, RawWakerVTable, Waker};

    type Slot = RefCell<Option<Box<dyn FnOnce()>>>;
    fn run(data: *const ()) {
        // SAFETY: `data` is an `Rc<Slot>` pointer this vtable created.
        let slot = unsafe { &*(data as *const Slot) };
        let f = slot.borrow_mut().take();
        if let Some(f) = f {
            f();
        }
    }
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        // SAFETY (all four): `data` came from `Rc::into_raw` of an
        // `Rc<Slot>`, and the test runs on one thread.
        |data| unsafe {
            Rc::increment_strong_count(data as *const Slot);
            RawWaker::new(data, &VTABLE)
        },
        |data| {
            run(data);
            drop(unsafe { Rc::from_raw(data as *const Slot) });
        },
        run,
        |data| drop(unsafe { Rc::from_raw(data as *const Slot) }),
    );
    let slot: Rc<Slot> = Rc::new(RefCell::new(Some(Box::new(f))));
    let raw = RawWaker::new(Rc::into_raw(slot) as *const (), &VTABLE);
    // SAFETY: the vtable upholds the `RawWaker` contract on one thread.
    unsafe { Waker::from_raw(raw) }
}

/// Arms `stream`'s multishot receive with `waker`, which the driver wakes
/// when data arrives.
fn arm_recv(stream: &TcpStream, waker: &std::task::Waker) {
    let mut recv = std::pin::pin!(stream.recv());
    let mut cx = std::task::Context::from_waker(waker);
    assert!(recv.as_mut().poll(&mut cx).is_pending());
}

#[test]
fn a_stream_dropped_inside_the_driver_still_closes() {
    // The stream's multishot receive used to stay armed: its key was lost
    // when the driver was borrowed, so the kernel kept the socket open (the
    // peer never saw it close) and the driver kept the entry.
    giong::block_on(async {
        let (c, s) = pair().await;
        let (c2, s2) = pair().await;
        // `s` has its multishot receive armed.
        arm_recv(&s, &waker_running(|| {}));
        let s = Rc::new(std::cell::RefCell::new(Some(s)));
        let dropper = {
            let s = s.clone();
            waker_running(move || drop(s.borrow_mut().take()))
        };
        // Data for `s2` makes the driver wake `dropper`, which drops `s`.
        arm_recv(&s2, &dropper);
        drop(dropper);
        c2.write_all(&b"go"[..]).await.0.unwrap();
        for _ in 0..100 {
            if s.borrow().is_none() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(s.borrow().is_none(), "the waker did not run");
        let (res, _) = timeout(Duration::from_secs(2), c.read(Vec::with_capacity(8)))
            .await
            .expect("the peer never saw the stream close");
        assert_eq!(res.unwrap(), 0);
        drop((c, c2, s2));
        sleep(Duration::from_millis(20)).await;
        assert_eq!(
            giong::observe::worker_stats().tracked_ops,
            0,
            "entries left"
        );
    });
}

#[test]
fn an_operation_dropped_inside_the_driver_is_still_cancelled() {
    // The future of an in-flight read, dropped by code the driver runs: its
    // buffer used to be leaked and its entry kept for good.
    giong::block_on(async {
        let (_c, s) = pair().await;
        let (c2, s2) = pair().await;
        let freed = Rc::new(std::cell::Cell::new(false));
        let buf = Tracked {
            data: Vec::with_capacity(64),
            freed: freed.clone(),
        };
        let s = Rc::new(s);
        let mut read: std::pin::Pin<Box<dyn Future<Output = _>>> = {
            let s = s.clone();
            Box::pin(async move { s.read(buf).await })
        };
        let noop = waker_running(|| {});
        let mut cx = std::task::Context::from_waker(&noop);
        assert!(read.as_mut().poll(&mut cx).is_pending());
        let read = Rc::new(std::cell::RefCell::new(Some(read)));
        let dropper = {
            let read = read.clone();
            waker_running(move || drop(read.borrow_mut().take()))
        };
        arm_recv(&s2, &dropper);
        drop(dropper);
        c2.write_all(&b"go"[..]).await.0.unwrap();
        for _ in 0..200 {
            if freed.get() {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        assert!(read.borrow().is_none(), "the waker did not run");
        assert!(freed.get(), "the read's buffer was never released");
        drop((c2, s2, s));
        sleep(Duration::from_millis(20)).await;
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
    });
}

#[test]
fn connections_accepted_for_a_listener_that_outlives_its_runtime_are_closed() {
    // io_uring accepts ahead of `accept` calls. A listener that outlived its
    // runtime left those connections open and unanswered: their descriptors
    // were never closed, so the clients waited for ever.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut clients = Vec::new();
    let uring = giong::block_on(async {
        let mut accept = Box::pin(listener.accept());
        let noop = waker_running(|| {});
        assert!(accept
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&noop))
            .is_pending());
        drop(accept);
        for _ in 0..3 {
            clients.push(std::net::TcpStream::connect(addr).unwrap());
        }
        // Let the driver take the accepted connections in.
        sleep(Duration::from_millis(50)).await;
        giong::driver_kind() == giong::DriverKind::IoUring
    });
    if !uring {
        // The epoll driver accepts only when asked: the connections wait in
        // the kernel's backlog, which closing the listener ends.
        return;
    }
    for mut client in clients {
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0u8; 1];
        let n = loop {
            match std::io::Read::read(&mut client, &mut byte) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                res => break res.expect("the connection was left open"),
            }
        };
        assert_eq!(n, 0);
    }
    drop(listener);
}
