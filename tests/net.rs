//! TCP: accept, recv (multishot + provided buffers), read, write, close.

// Some tests and their helpers are Unix-only for now.
#![cfg_attr(windows, allow(dead_code, unused_imports))]

use std::borrow::Cow;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong::time::{sleep, timeout};

mod common;

/// Lets this process open at least `needed` descriptors, by raising the soft
/// limit up to the hard one. Shells start with a soft limit of 1024 (Ubuntu's
/// included), which the tests below need more than; the tests running
/// alongside open some, too.
#[cfg(unix)]
fn allow_open_files(needed: u64) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain system calls with a valid `rlimit`.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
        if lim.rlim_cur >= needed {
            return;
        }
        assert!(
            lim.rlim_max >= needed,
            "this test needs {needed} open files, and the hard limit is {} (ulimit -Hn)",
            lim.rlim_max
        );
        // As much as allowed, so that the other tests have room too; the
        // kernel caps it at fs.nr_open, so fall back to what is needed.
        let hard = lim.rlim_max;
        lim.rlim_cur = hard;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
            lim.rlim_cur = needed;
            assert_eq!(
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }
    }
}

fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
}

/// Deterministic pseudo-random payload.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn recv_exact(stream: &TcpStream, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let chunk = stream.recv().await.unwrap();
        assert!(
            !chunk.is_empty(),
            "unexpected EOF after {} of {len} bytes",
            out.len()
        );
        out.extend_from_slice(&chunk);
    }
    out
}

#[test]
fn connections_reset_before_they_are_accepted_do_not_end_the_accept_loop() {
    // A port scan or a health check: connect, then reset at once, before
    // the server takes the connection. The accept loop must carry on and
    // serve the next client, as on Unix.
    giong::block_on(async {
        let (l, addr) = listener();
        let resets = std::thread::spawn(move || {
            for _ in 0..20 {
                let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
                    .unwrap();
                s.connect(&addr.into()).unwrap();
                // Closing with a zero linger sends RST, not FIN.
                s.set_linger(Some(Duration::ZERO)).unwrap();
                drop(s);
            }
            // Let the resets reach the server first.
            std::thread::sleep(Duration::from_millis(100));
            let mut good = std::net::TcpStream::connect(addr).unwrap();
            good.write_all(b"hello").unwrap();
            good
        });
        sleep(Duration::from_millis(50)).await;
        let served = timeout(Duration::from_secs(10), async {
            loop {
                // Never an error: a reset connection is skipped, or
                // accepted and then fails to read.
                let s = l.accept().await.expect("accept goes on after resets");
                let (res, buf) = s.read(vec![0u8; 16]).await;
                if let Ok(5) = res {
                    assert_eq!(&buf[..5], b"hello");
                    return;
                }
            }
        })
        .await;
        assert!(served.is_ok(), "the good client was not served");
        drop(resets.join().unwrap());
    });
}

#[test]
fn no_other_socket_can_bind_over_a_live_listener_or_udp_socket() {
    // Not even one that sets `SO_REUSEADDR`: on Windows that option lets a
    // socket bind a port another socket holds, unless that one is bound
    // exclusively.
    giong::block_on(async {
        let (_l, addr) = listener();
        let udp = giong::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_addr = udp.local_addr().unwrap();
        for (kind, addr) in [
            (socket2::Type::STREAM, addr),
            (socket2::Type::DGRAM, udp_addr),
        ] {
            let other = socket2::Socket::new(socket2::Domain::IPV4, kind, None).unwrap();
            other.set_reuse_address(true).unwrap();
            let err = match other.bind(&addr.into()) {
                Err(err) => err,
                // Linux 6.0.16 to 6.0.19 report success for a TCP bind to a
                // busy address and leave the socket unbound (see
                // `binding_a_busy_address_fails_and_says_so`): not bound
                // over the live socket either.
                Ok(()) => {
                    let at = other.local_addr().unwrap().as_socket();
                    assert!(
                        at.map_or(true, |a| a.port() == 0),
                        "{kind:?}: bound over a live socket, at {at:?}"
                    );
                    continue;
                }
            };
            // Windows says "access denied" to a socket that asked to share.
            let refused = [std::io::ErrorKind::AddrInUse]
                .into_iter()
                .chain(cfg!(windows).then_some(std::io::ErrorKind::PermissionDenied));
            assert!(
                refused.into_iter().any(|k| k == err.kind()),
                "{kind:?}: {err:?}"
            );
        }
    });
}

/// Connections a shared address hands round are not inherited by child
/// processes, like every other socket: a child holding a copy would keep
/// the connection open after this process closed it. (One name on every
/// system, for the simulator's index; the body is Windows', where a thread
/// accepts for listeners that share an address.)
#[test]
fn connections_handed_round_a_shared_address_are_not_inherited() {
    #[cfg(windows)]
    giong::block_on(async {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = first.local_addr().unwrap();
        let second = TcpListener::bind(addr).unwrap();
        // Whichever listener the connections go to: each accepts them.
        let got: Rc<std::cell::RefCell<Vec<(usize, TcpStream)>>> = Rc::default();
        let tasks: Vec<_> = [first, second]
            .into_iter()
            .enumerate()
            .map(|(which, listener)| {
                let got = got.clone();
                giong::spawn(async move {
                    loop {
                        let stream = listener.accept().await.unwrap();
                        got.borrow_mut().push((which, stream));
                    }
                })
            })
            .collect();
        let _clients: Vec<_> = (0..2)
            .map(|_| std::net::TcpStream::connect(addr).unwrap())
            .collect();
        let start = std::time::Instant::now();
        while got.borrow().len() < 2 && start.elapsed() < Duration::from_secs(5) {
            sleep(Duration::from_millis(10)).await;
        }
        tasks.iter().for_each(|t| t.abort());
        let got = got.borrow();
        assert_eq!(got.len(), 2, "connections handed over");
        // Which listener took each: the first one's own accept may take
        // one, and the thread's turn may give it the other as well.
        let takers: Vec<usize> = got.iter().map(|(which, _)| *which).collect();
        eprintln!("taken by listeners {takers:?}");
        for (_, stream) in got.iter() {
            let mut flags = 0u32;
            // SAFETY: reads the flags of an open socket into a local.
            let ok = unsafe { GetHandleInformation(stream.as_raw_socket() as _, &mut flags) };
            assert_ne!(ok, 0);
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0, "an inheritable connection");
        }
    });
}

/// A listener dropped after its own accept took a connection, while
/// another listener shares the address, gives that connection to the other
/// one instead of closing it. (One name on every system, for the
/// simulator's index; the body is Windows', where the first listener of an
/// address accepts through its own driver.)
#[test]
fn a_dropped_listener_hands_its_accepted_connection_to_another_sharing_the_address() {
    #[cfg(windows)]
    giong::block_on(async {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = first.local_addr().unwrap();
        // An accept of the first listener left waiting in the driver.
        assert!(timeout(Duration::from_millis(20), first.accept())
            .await
            .is_err());
        let second = TcpListener::bind(addr).unwrap();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.write_all(b"x").unwrap();
        // Time for the first listener's accept to complete.
        sleep(Duration::from_millis(100)).await;
        drop(first);
        let stream = timeout(Duration::from_secs(5), second.accept())
            .await
            .expect("the connection was lost with the first listener")
            .unwrap();
        let data = stream.recv().await.unwrap();
        assert_eq!(&data[..], b"x");
    });
}

#[test]
fn echo_roundtrip() {
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let s = l.accept().await.unwrap();
            loop {
                let data = s.recv().await.unwrap();
                if data.is_empty() {
                    break;
                }
                let copy = data.to_vec();
                drop(data);
                s.write_all(copy).await.0.unwrap();
            }
        });

        let c = TcpStream::connect(addr).await.unwrap();
        for i in 0..50 {
            let msg = format!("hello {i}").into_bytes();
            let (res, msg) = c.write_all(msg).await;
            res.unwrap();
            assert_eq!(recv_exact(&c, msg.len()).await, msg);
        }
        drop(c);
        server.await.unwrap();

        let stats = giong::observe::worker_stats();
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

#[test]
fn large_transfer_is_intact_in_both_directions() {
    const LEN: usize = 8 * 1024 * 1024;
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let s = l.accept().await.unwrap();
            let got = recv_exact(&s, LEN).await;
            assert_eq!(got, payload(LEN, 1));
            s.write_all(payload(LEN, 2)).await.0.unwrap();
        });

        let c = Rc::new(TcpStream::connect(addr).await.unwrap());
        // Write and read concurrently so neither side stalls on full buffers.
        let writer = {
            let c = c.clone();
            giong::spawn(async move { c.write_all(payload(LEN, 1)).await.0.unwrap() })
        };
        let got = recv_exact(&c, LEN).await;
        assert_eq!(got, payload(LEN, 2));
        writer.await.unwrap();
        server.await.unwrap();

        let stats = giong::observe::worker_stats();
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

#[test]
fn a_reader_that_keeps_up_gets_a_bulk_transfer_without_copies() {
    // One stream, written 256 KiB at a time: dozens of receives complete
    // between two turns of the event loop. None of it may be moved out of
    // the pool while the pool has room (a backlog limit of 8 per stream
    // once copied most of such a transfer).
    const LEN: usize = 64 * 1024 * 1024;
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let writer = std::thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        let chunk = vec![5u8; 256 * 1024];
        for _ in 0..LEN / chunk.len() {
            s.write_all(&chunk).unwrap();
        }
    });
    giong::block_on(async move {
        let l = TcpListener::from_std(std_listener);
        let s = l.accept().await.unwrap();
        let mut got = 0;
        while got < LEN {
            let data = s.recv().await.unwrap();
            assert!(!data.is_empty(), "EOF after {got} bytes");
            assert!(data.iter().all(|&b| b == 5));
            got += data.len();
        }
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.recv_copied, 0, "{stats:?}");
    });
    writer.join().unwrap();
}

#[test]
#[cfg(unix)]
fn readers_that_stop_reading_hold_a_bounded_amount_of_memory() {
    // Peers that keep sending to tasks that read once and then stop (an
    // HTTP client that pipelines requests and never reads the answers): what
    // waits for those readers must stay within a bound for the whole worker,
    // not grow with every connection; the rest waits in the socket, under
    // TCP flow control. Once they read again, every byte arrives in order.
    const CONNS: usize = 128;
    const LEN: usize = 1024 * 1024;
    allow_open_files(CONNS as u64 * 2 + 64);
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let clients: Vec<_> = (0..CONNS)
        .map(|_| {
            std::thread::spawn(move || {
                let mut s = std::net::TcpStream::connect(addr).unwrap();
                let data: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
                let mut sent = 0;
                while sent < LEN {
                    match s.write(&data[sent..]) {
                        Ok(n) => sent += n,
                        // macOS refuses a send with ENOBUFS, where Linux
                        // makes it wait, once loopback runs out of buffers
                        // (128 peers with 1 MiB each in flight did, on a
                        // macos-26 runner). It sends nothing then; Gióng's
                        // own sends wait it out the same way.
                        Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => panic!("send after {sent} bytes: {e}"),
                    }
                }
                s.shutdown(Shutdown::Write).unwrap();
            })
        })
        .collect();
    giong::block_on(async move {
        let l = TcpListener::from_std(std_listener);
        let go = Rc::new(std::cell::Cell::new(false));
        let mut readers = Vec::new();
        for _ in 0..CONNS {
            let s = l.accept().await.unwrap();
            let go = go.clone();
            readers.push(giong::spawn(async move {
                let mut got = s.recv().await.unwrap().to_vec();
                while !go.get() {
                    sleep(Duration::from_millis(10)).await;
                }
                loop {
                    let data = s.recv().await.unwrap();
                    if data.is_empty() {
                        break;
                    }
                    got.extend_from_slice(&data);
                }
                assert_eq!(got.len(), LEN);
                assert!(got.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8));
            }));
        }
        sleep(Duration::from_secs(1)).await;
        let stats = giong::observe::worker_stats();
        // The pool's size (16 KiB buffers) on the heap, plus the pool itself
        // copied out, plus slack; without a bound this is about 0.5 MiB per
        // connection (64 MiB here).
        let bound = 3 * stats.recv_buffers as u64 * 16 * 1024;
        assert!(stats.recv_copied <= bound, "{stats:?}");
        go.set(true);
        for r in readers {
            r.await.unwrap();
        }
    });
    for c in clients {
        c.join().unwrap();
    }
}

#[test]
fn large_writes_arrive_intact_with_or_without_zero_copy() {
    // Sizes around the zero-copy threshold (16 KiB), twice on each fresh
    // connection, from buffers that can change (sent with a copy) and from
    // ones that cannot (`Rc<[u8]>`, `'static` bytes, also behind a head that
    // can change: without a copy where the kernel offers it). Such a write
    // returns before the kernel is done with the bytes; the head it gives
    // back is overwritten at once, which must not change what was sent. On
    // loopback the kernel copies after all, and once it has said so the
    // socket sends with a copy.
    giong::block_on(async {
        let (l, addr) = listener();
        let sizes = [16 * 1024 - 1, 16 * 1024, 64 * 1024 + 7, 1024 * 1024 + 3];
        for (i, &size) in sizes.iter().enumerate() {
            let data: &'static [u8] = Box::leak(payload(size, i as u64).into_boxed_slice());
            for way in ["vec", "rc", "static", "vectored vec", "vectored head"] {
                let c = TcpStream::connect(addr).await.unwrap();
                let s = l.accept().await.unwrap();
                let reader = giong::spawn(async move { recv_exact(&s, 2 * size).await });
                let (head, body) = data.split_at(100);
                for _ in 0..2 {
                    match way {
                        "vec" => c.write_all(data.to_vec()).await.0.unwrap(),
                        "rc" => c.write_all(Rc::<[u8]>::from(data)).await.0.unwrap(),
                        "static" => c.write_all(data).await.0.unwrap(),
                        "vectored vec" => {
                            let pieces = vec![head.to_vec(), body.to_vec()];
                            c.write_all_vectored(pieces).await.0.unwrap();
                        }
                        _ => {
                            let pieces = vec![Cow::Owned(head.to_vec()), Cow::Borrowed(body)];
                            let (res, mut pieces) = c.write_all_vectored(pieces).await;
                            res.unwrap();
                            pieces[0].to_mut().fill(0xEE);
                        }
                    }
                }
                let got = reader.await.unwrap();
                assert!(
                    got[..size] == data[..] && got[size..] == data[..],
                    "{size} bytes, {way}"
                );
            }
        }
        drop(l);
        sleep(Duration::from_millis(20)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn recv_reports_eof_and_keeps_reporting_it() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(&b"bye"[..]).await.0.unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        assert_eq!(&*s.recv().await.unwrap(), b"bye");
        assert!(s.recv().await.unwrap().is_empty());
        assert!(s.recv().await.unwrap().is_empty());
    });
}

#[test]
fn read_with_an_owned_buffer() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(String::from("owned buffer")).await.0.unwrap();
        let (res, buf) = s.read(Vec::with_capacity(64)).await;
        assert_eq!(res.unwrap(), 12);
        assert_eq!(buf, b"owned buffer");
        drop(c);
        let (res, buf) = s.read(buf).await;
        assert_eq!(res.unwrap(), 0);
        assert!(buf.is_empty());
    });
}

#[test]
fn read_and_recv_cannot_be_mixed() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(&b"x"[..]).await.0.unwrap();
        assert_eq!(&*s.recv().await.unwrap(), b"x");
        let (res, _) = s.read(Vec::with_capacity(8)).await;
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    });
}

/// One control message: `level`, `kind`, then `data`, laid out the way
/// `CMSG_SPACE` says.
#[cfg(target_os = "linux")]
fn control_message(level: i32, kind: i32, data: &[u8]) -> Vec<u8> {
    // SAFETY: CMSG_SPACE and CMSG_LEN only compute sizes.
    let (space, len) = unsafe {
        (
            libc::CMSG_SPACE(data.len() as u32) as usize,
            libc::CMSG_LEN(data.len() as u32) as usize,
        )
    };
    let header = std::mem::size_of::<libc::cmsghdr>();
    let mut buf = vec![0u8; space];
    // `cmsghdr` is { cmsg_len: usize, cmsg_level: i32, cmsg_type: i32 } on
    // 64-bit Linux, the data follows it.
    buf[..8].copy_from_slice(&(len as u64).to_ne_bytes());
    buf[8..12].copy_from_slice(&level.to_ne_bytes());
    buf[12..16].copy_from_slice(&kind.to_ne_bytes());
    buf[header..header + data.len()].copy_from_slice(data);
    buf
}

// Linux cmsghdr layout and SO_TIMESTAMPING.
#[cfg(target_os = "linux")]
#[test]
fn send_with_control_hands_the_control_message_to_the_kernel() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        // SO_TIMESTAMPING with no flags: valid on any TCP socket.
        let fine = control_message(libc::SOL_SOCKET, libc::SO_TIMESTAMPING, &0u32.to_ne_bytes());
        let (res, buf, control) = c.send_with_control(b"with control".to_vec(), fine).await;
        assert_eq!(res.unwrap(), 12);
        assert_eq!(buf, b"with control");
        assert!(!control.is_empty());
        // An empty control buffer sends none.
        let (res, _, _) = c.send_with_control(b"!".to_vec(), Vec::new()).await;
        assert_eq!(res.unwrap(), 1);
        let mut got = Vec::new();
        while got.len() < 13 {
            got.extend_from_slice(&s.recv().await.unwrap());
        }
        assert_eq!(got, b"with control!");
        // The kernel reads the control message: one it does not know is
        // refused, and nothing is sent.
        let unknown = control_message(libc::SOL_SOCKET, 9999, &[0; 4]);
        let (res, _, _) = c.send_with_control(b"never".to_vec(), unknown).await;
        assert_eq!(res.unwrap_err().raw_os_error(), Some(libc::EINVAL));
        drop(c);
        assert!(s.recv().await.unwrap().is_empty());
    });
}

/// A file under the temporary directory holding `data`, removed on drop.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(name: &str, data: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!("giong-net-{}-{name}", std::process::id()));
        std::fs::write(&path, data).unwrap();
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn send_file_sends_the_file_and_leaves_the_stream_usable() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = Rc::new(l.accept().await.unwrap());
        // Larger than the socket buffers, so sends stop part-way and wait.
        let data = payload((9 << 20) + 123, 7);
        let tmp = TempFile::new("send-file", &data);
        let file = giong::fs::File::open(&tmp.0).await.unwrap();

        let reader = {
            let s = s.clone();
            let len = data.len();
            giong::spawn(async move { recv_exact(&s, len).await })
        };
        c.send_file(&file, 0, data.len() as u64).await.unwrap();
        assert!(reader.await.unwrap() == data);

        // A range from the middle, then nothing at all.
        c.send_file(&file, 1000, 5000).await.unwrap();
        assert!(recv_exact(&s, 5000).await == data[1000..6000]);
        c.send_file(&file, 0, 0).await.unwrap();
        // The file's own position did not move.
        let (n, buf) = file.read_at(Vec::with_capacity(16), 0).await;
        assert_eq!(n.unwrap(), 16);
        assert_eq!(buf, data[..16]);

        // The stream now has O_NONBLOCK set; every other operation works as
        // before, in both directions, with full socket buffers.
        let big = payload(6 << 20, 8);
        let reader = {
            let s = s.clone();
            let len = big.len();
            giong::spawn(async move { recv_exact(&s, len).await })
        };
        let (res, _) = c.write_all(big.clone()).await;
        res.unwrap();
        assert!(reader.await.unwrap() == big);
        let reader = giong::spawn(async move {
            let mut out = Vec::new();
            loop {
                let chunk = c.recv().await.unwrap();
                if chunk.is_empty() {
                    break out;
                }
                out.extend_from_slice(&chunk);
            }
        });
        let (res, _) = s
            .write_all_vectored(vec![big.clone(), b"end".to_vec()])
            .await;
        res.unwrap();
        s.shutdown(Shutdown::Write).unwrap();
        let back = reader.await.unwrap();
        assert!(back.len() == big.len() + 3 && back[..big.len()] == big[..]);
    });
}

#[test]
fn a_dropped_send_file_whose_file_is_closed_leaves_the_stream_usable() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = Rc::new(l.accept().await.unwrap());
        // Larger than the socket buffers, so the first send is in flight
        // when its future is dropped.
        let first = TempFile::new("dropped-send-file-1", &[0x11; 8 << 20]);
        let second = TempFile::new("dropped-send-file-2", &[0x22; 4096]);
        let reader = {
            let s = s.clone();
            giong::spawn(async move {
                let mut out = Vec::new();
                loop {
                    let chunk = s.recv().await.unwrap();
                    if chunk.is_empty() {
                        break out;
                    }
                    out.extend_from_slice(&chunk);
                }
            })
        };
        let file = giong::fs::File::open(&first.0).await.unwrap();
        {
            // Started (one poll hands it to the kernel), then dropped, and
            // the file closed with it, as when a handler is cancelled.
            let mut send = std::pin::pin!(c.send_file(&file, 0, 8 << 20));
            std::future::poll_fn(|cx| {
                assert!(
                    std::future::Future::poll(send.as_mut(), cx).is_pending(),
                    "the send ended at once"
                );
                std::task::Poll::Ready(())
            })
            .await;
        }
        drop(file);
        let file = giong::fs::File::open(&second.0).await.unwrap();
        c.send_file(&file, 0, 4096).await.unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let got = reader.await.unwrap();
        let ones = got.iter().take_while(|&&b| b == 0x11).count();
        assert!(
            got[ones..] == [0x22; 4096][..],
            "{} bytes after the first file's",
            got.len() - ones
        );
        // Windows sends the piece in flight to its end.
        #[cfg(windows)]
        assert_eq!(ones, 8 << 20);
    });
}

#[test]
fn send_file_past_the_end_sends_what_there_is_and_says_so() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        let data = payload(1000, 9);
        let tmp = TempFile::new("send-file-short", &data);
        let file = giong::fs::File::open(&tmp.0).await.unwrap();

        let err = c.send_file(&file, 200, 2000).await.unwrap_err();
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::FileTooShort)
        );
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("1200 bytes short"), "{err}");
        assert!(recv_exact(&s, 800).await == data[200..]);

        // A range no file can have fails before sending anything.
        for (offset, len) in [(u64::MAX, 1), (1, u64::MAX), (i64::MAX as u64, 1)] {
            let err = c.send_file(&file, offset, len).await.unwrap_err();
            assert_eq!(
                giong::error::code(&err),
                Some(giong::error::Code::FileTooShort)
            );
        }
        c.write_all(&b"!"[..]).await.0.unwrap();
        assert_eq!(recv_exact(&s, 1).await, b"!");
    });
}

#[test]
fn send_file_to_a_closed_peer_is_an_error_not_a_signal() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        drop(l.accept().await.unwrap());
        sleep(Duration::from_millis(20)).await;
        let tmp = TempFile::new("send-file-closed", &payload(256 * 1024, 10));
        let file = giong::fs::File::open(&tmp.0).await.unwrap();
        let mut failed = false;
        for _ in 0..50 {
            if let Err(e) = c.send_file(&file, 0, 256 * 1024).await {
                assert!(
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ),
                    "{e}"
                );
                failed = true;
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert!(failed, "sends to a closed peer kept succeeding");
    });
}

#[cfg(unix)]
#[test]
fn a_non_blocking_stream_never_reports_would_block() {
    // io_uring stops waiting on a non-blocking socket in some cases (Linux
    // 6.0 and 6.1: when a woken sendmsg still finds the buffer full, it is
    // retried on a kernel thread, which then sees O_NONBLOCK) and completes
    // with EAGAIN; giong submits again. Four writers on one small buffer
    // keep waking each other into exactly that case. (Several writers at
    // once interleave their bytes, which is why streams allow one; it is
    // safe, and the byte count still adds up.)
    giong::block_on(async {
        let (l, addr) = listener();
        let c = Rc::new(TcpStream::connect(addr).await.unwrap());
        let s = Rc::new(l.accept().await.unwrap());
        let tmp = TempFile::new("send-file-nonblocking", b"x");
        let file = giong::fs::File::open(&tmp.0).await.unwrap();
        c.send_file(&file, 0, 0).await.unwrap();
        s.send_file(&file, 0, 0).await.unwrap();
        // A small send buffer, so the writers keep waiting for room.
        let small: libc::c_int = 4096;
        // SAFETY: a valid descriptor and option value.
        let res = unsafe {
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&*c),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&small as *const libc::c_int).cast(),
                std::mem::size_of_val(&small) as libc::socklen_t,
            )
        };
        assert_eq!(res, 0);
        const EACH: usize = 2 << 20;
        let writers: Vec<_> = (0..4u8)
            .map(|w| {
                let c = c.clone();
                giong::spawn(async move {
                    for _ in 0..EACH / 16384 {
                        let pieces = vec![vec![w; 8192], vec![w; 8192]];
                        c.write_all_vectored(pieces).await.0.unwrap();
                    }
                })
            })
            .collect();
        let reader = giong::spawn(async move {
            let mut total = 0;
            while total < 4 * EACH {
                let (res, _) = s.read(Vec::with_capacity(64 * 1024)).await;
                total += res.unwrap();
            }
            total
        });
        for w in writers {
            w.await.unwrap();
        }
        assert_eq!(reader.await.unwrap(), 4 * EACH);
    });
}

#[test]
fn connect_refused() {
    giong::block_on(async {
        let (_refusing, addr) = common::refusing_addr();
        let err = TcpStream::connect(addr).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
    });
}

#[test]
fn vectored_writes_send_every_piece_in_order() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = Rc::new(l.accept().await.unwrap());

        // 3,001 pieces (one sendmsg takes 1,024), static, owned and empty,
        // and 6 MiB in one of them, so the socket buffer fills and sends end
        // part-way through a piece.
        let mut pieces: Vec<Cow<'static, [u8]>> = Vec::new();
        for i in 0..3000u64 {
            pieces.push(match i % 3 {
                0 => Cow::Borrowed(b"static;"),
                1 => Cow::Owned(payload((i % 700) as usize, i)),
                _ => Cow::Borrowed(b""),
            });
            if i == 1500 {
                pieces.push(Cow::Owned(payload(6 << 20, 99)));
            }
        }
        let expected = pieces.concat();
        let reader = {
            let s = s.clone();
            let len = expected.len();
            giong::spawn(async move { recv_exact(&s, len).await })
        };
        let (res, back) = c.write_all_vectored(pieces).await;
        res.unwrap();
        assert_eq!(back.len(), 3001);
        assert!(reader.await.unwrap() == expected);

        // Nothing to send is not an error and sends nothing.
        let (res, _) = c.write_vectored(Vec::<Vec<u8>>::new()).await;
        assert_eq!(res.unwrap(), 0);
        let (res, _) = c.write_all_vectored(vec![Vec::new(), Vec::new()]).await;
        res.unwrap();
        // A short one goes out whole.
        let (res, back) = c.write_vectored(vec![b"ab".to_vec(), b"cd".to_vec()]).await;
        assert_eq!(res.unwrap(), 4);
        assert_eq!(back, [b"ab".to_vec(), b"cd".to_vec()]);
        assert_eq!(recv_exact(&s, 4).await, b"abcd");
    });
}

#[test]
fn vectored_writes_to_a_closed_peer_are_an_error_not_a_signal() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        drop(l.accept().await.unwrap());
        sleep(Duration::from_millis(20)).await;
        let mut failed = false;
        for _ in 0..50 {
            let pieces = vec![vec![0u8; 32 * 1024], vec![1u8; 32 * 1024]];
            if c.write_all_vectored(pieces).await.0.is_err() {
                failed = true;
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert!(failed, "writes to a closed peer kept succeeding");
    });
}

#[test]
fn writing_to_a_closed_peer_is_an_error_not_a_signal() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        drop(s);
        sleep(Duration::from_millis(20)).await;
        let mut failed = false;
        for _ in 0..50 {
            if c.write_all(vec![0u8; 64 * 1024]).await.0.is_err() {
                failed = true;
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert!(failed, "writes to a closed peer kept succeeding");
    });
}

#[test]
fn dropping_a_stream_closes_the_connection() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        // Arm the multishot receive, then drop the stream while it is in flight.
        assert!(timeout(Duration::from_millis(10), s.recv()).await.is_err());
        drop(s);
        let eof = timeout(Duration::from_secs(5), c.recv())
            .await
            .expect("peer never saw the close");
        assert!(eof.unwrap().is_empty());
        drop(c);
        drop(l); // its multishot accept is the only other tracked operation
        sleep(Duration::from_millis(20)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        // On io_uring only the wake-up read may remain; epoll has nothing
        // in flight.
        let expected = match giong::driver_kind() {
            giong::DriverKind::IoUring => 1,
            _ => 0,
        };
        assert_eq!(stats.inflight, expected, "{stats:?}");
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

#[cfg(unix)]
#[test]
fn accept_many_connections() {
    const N: usize = 500;
    // Both ends of every connection, and room for the other tests.
    allow_open_files(2 * N as u64 + 1024);
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let mut conns = Vec::new();
            for _ in 0..N {
                conns.push(l.accept().await.unwrap());
            }
            for (i, s) in conns.iter().enumerate() {
                s.write_all((i as u32).to_le_bytes().to_vec())
                    .await
                    .0
                    .unwrap();
            }
            conns.len()
        });
        let mut clients = Vec::new();
        for _ in 0..N {
            clients.push(TcpStream::connect(addr).await.unwrap());
        }
        assert_eq!(server.await.unwrap(), N);
        let mut seen = [false; N];
        for c in &clients {
            let id = u32::from_le_bytes(recv_exact(c, 4).await.try_into().unwrap()) as usize;
            assert!(!seen[id]);
            seen[id] = true;
        }
        assert!(seen.iter().all(|&s| s));
    });
}

#[test]
fn dropping_a_listener_with_pending_connections_closes_them() {
    giong::block_on(async {
        let (l, addr) = listener();
        // Arm the multishot accept.
        assert!(timeout(Duration::from_millis(10), l.accept())
            .await
            .is_err());
        let c1 = TcpStream::connect(addr).await.unwrap();
        let c2 = TcpStream::connect(addr).await.unwrap();
        sleep(Duration::from_millis(20)).await;
        // Two accepted sockets are queued inside the listener; dropping it must close them.
        drop(l);
        for c in [&c1, &c2] {
            let res = timeout(Duration::from_secs(5), c.recv())
                .await
                .expect("connection left open");
            assert!(matches!(res, Ok(ref b) if b.is_empty()) || res.is_err());
        }
        sleep(Duration::from_millis(20)).await;
        drop((c1, c2));
        sleep(Duration::from_millis(20)).await;
        assert_eq!(giong::observe::worker_stats().tracked_ops, 0);
    });
}

#[test]
fn interoperates_with_blocking_std_clients() {
    let (tx, rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        giong::block_on(async move {
            let (l, addr) = listener();
            tx.send(addr).unwrap();
            for _ in 0..3 {
                let s = l.accept().await.unwrap();
                giong::spawn(async move {
                    loop {
                        let data = s.recv().await.unwrap();
                        if data.is_empty() {
                            return;
                        }
                        let mut reply = data.to_vec();
                        drop(data);
                        reply.reverse();
                        s.write_all(reply).await.0.unwrap();
                    }
                });
            }
            sleep(Duration::from_millis(300)).await;
        });
    });
    let addr = rx.recv().unwrap();
    for _ in 0..3 {
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.write_all(b"abcdef").unwrap();
        let mut buf = [0u8; 6];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"fedcba");
    }
    server.join().unwrap();
}

/// A port held by a listener that shares it with nobody. Linux 6.0.16 to
/// 6.0.19 answer 1, not -1, to the bind(2) of a second TCP socket there,
/// which reads as success and would leave the listener on some other port.
#[test]
fn binding_a_busy_address_fails_and_says_so() {
    giong::block_on(async {
        for any in ["127.0.0.1:0", "[::1]:0"] {
            let Ok(holder) = std::net::TcpListener::bind(any) else {
                assert!(any.starts_with('['), "cannot bind {any}");
                continue; // no IPv6 here
            };
            let addr = holder.local_addr().unwrap();
            match TcpListener::bind(addr) {
                Ok(l) => panic!(
                    "bound {addr}, which is in use, and listens on {:?}",
                    l.local_addr()
                ),
                Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{addr}: {e}"),
            }
            // Listeners of this runtime still share a port with each other.
            let first = TcpListener::bind(any).unwrap();
            let shared = first.local_addr().unwrap();
            let second = TcpListener::bind(shared).unwrap();
            assert_eq!(second.local_addr().unwrap(), shared);
        }
    });
}

#[test]
fn from_std_listener_and_stream() {
    giong::block_on(async {
        let std_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = std_l.local_addr().unwrap();
        let l = TcpListener::from_std(std_l);
        let std_c = std::net::TcpStream::connect(addr).unwrap();
        let c = TcpStream::from_std(std_c);
        let s = l.accept().await.unwrap();
        assert_eq!(s.peer_addr().unwrap(), c.local_addr().unwrap());
        s.set_nodelay(true).unwrap();
        s.write_all(&b"ping"[..]).await.0.unwrap();
        assert_eq!(recv_exact(&c, 4).await, b"ping");
    });
}

#[cfg(unix)]
#[test]
fn accept_backlog_is_bounded_and_no_connection_is_lost() {
    // More pending connections than the driver is willing to queue (1024):
    // the multishot accept must pause, leave the rest in the kernel's listen
    // backlog, and resume once the application catches up.
    // The kqueue driver accepts only when asked, so on macOS all of them
    // wait in the kernel, which keeps at most 128 (`kern.ipc.somaxconn`)
    // and drops the rest: fewer there, and none may be lost.
    const N: usize = if cfg!(target_os = "macos") { 100 } else { 1300 };
    // Every client, up to 1024 accepted sockets queued in the driver, and
    // room for the other tests.
    allow_open_files(N as u64 + 1024 + 1024);
    giong::block_on(async {
        let (l, addr) = listener();
        // Arm the multishot accept, then stop accepting for a while.
        assert!(timeout(Duration::from_millis(5), l.accept()).await.is_err());

        let mut clients = Vec::with_capacity(N);
        for i in 0..N {
            let c = TcpStream::connect(addr).await.unwrap();
            c.write_all((i as u32).to_le_bytes().to_vec())
                .await
                .0
                .unwrap();
            clients.push(c);
        }
        sleep(Duration::from_millis(50)).await;

        let mut seen = [false; N];
        for _ in 0..N {
            let s = timeout(Duration::from_secs(10), l.accept())
                .await
                .expect("accept stalled after the backlog pause")
                .unwrap();
            let id = u32::from_le_bytes(recv_exact(&s, 4).await.try_into().unwrap()) as usize;
            assert!(!seen[id], "connection {id} accepted twice");
            seen[id] = true;
        }
        assert!(seen.iter().all(|&s| s));
    });
}

/// Many connections writing small pieces at once through a small pool: with
/// the packed buffer ring (Linux 6.12+) the pieces of different connections
/// share buffers and are cut where a buffer ends. Every stream must still
/// arrive byte for byte, and every buffer must come back.
#[test]
fn small_writes_from_many_connections_share_buffers_and_arrive_intact() {
    const CONNS: usize = 16;
    const MESSAGES: usize = 300;
    fn stream_of(conn: usize) -> Vec<Vec<u8>> {
        (0..MESSAGES)
            .map(|i| {
                let len = 1 + (conn * 37 + i * 101) % 300;
                (0..len)
                    .map(|j| ((conn * 31 + i * 7 + j) % 251) as u8)
                    .collect()
            })
            .collect()
    }
    giong::Builder::new()
        .recv_buffers(8, 512)
        .block_on(async {
            let (l, addr) = listener();
            let mut writers = Vec::new();
            for conn in 0..CONNS {
                writers.push(giong::spawn(async move {
                    let c = TcpStream::connect(addr).await.unwrap();
                    c.write_all(vec![conn as u8]).await.0.unwrap();
                    for msg in stream_of(conn) {
                        c.write_all(msg).await.0.unwrap();
                        if conn % 3 == 0 {
                            giong::yield_now().await;
                        }
                    }
                }));
            }
            let mut readers = Vec::new();
            for _ in 0..CONNS {
                let s = l.accept().await.unwrap();
                readers.push(giong::spawn(async move {
                    let mut got = Vec::new();
                    loop {
                        let data = s.recv().await.unwrap();
                        if data.is_empty() {
                            break;
                        }
                        got.extend_from_slice(&data);
                        // Hold no piece across an await, but yield now and
                        // then so pieces of other streams pile up.
                        drop(data);
                        if got.len() % 5 == 0 {
                            giong::yield_now().await;
                        }
                    }
                    got
                }));
            }
            for w in writers {
                w.await.unwrap();
            }
            for r in readers {
                let got = timeout(Duration::from_secs(30), r).await.unwrap().unwrap();
                let conn = got[0] as usize;
                assert_eq!(&got[1..], stream_of(conn).concat(), "connection {conn}");
            }
            sleep(Duration::from_millis(20)).await;
            let stats = giong::observe::worker_stats();
            assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
        })
        .unwrap();
}

/// A listener of another process that shares its port (`SO_REUSEPORT`, as
/// giong's own listeners set) must not get a giong listener as a partner:
/// the kernel would split the connections between the two processes.
/// A socket made here with socket2 stands for it, since giong does not know
/// it.
#[cfg(unix)]
#[test]
fn a_port_shared_by_another_process_is_refused() {
    giong::block_on(async {
        let other =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        other.set_reuse_address(true).unwrap();
        other.set_reuse_port(true).unwrap();
        other
            .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
            .unwrap();
        other.listen(16).unwrap();
        let addr = other.local_addr().unwrap().as_socket().unwrap();
        let err = TcpListener::bind(addr).expect_err("joined another process's listener");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
        // Nor through bind_balanced.
        let err = TcpListener::bind_balanced(addr).expect_err("joined it through bind_balanced");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
    });
}

/// Dropping a listener and binding the same address again at once works,
/// although the socket is closed through the ring a moment later; and every
/// listener bound meanwhile shares the port.
#[test]
fn a_dropped_listener_s_port_can_be_bound_again_at_once() {
    giong::block_on(async {
        let (first, addr) = listener();
        let second = TcpListener::bind(addr).unwrap();
        drop(first);
        drop(second);
        for _ in 0..20 {
            let again = TcpListener::bind(addr).unwrap();
            let c = std::net::TcpStream::connect(addr).unwrap();
            let s = timeout(Duration::from_secs(5), again.accept())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(s.peer_addr().unwrap(), c.local_addr().unwrap());
        }
    });
}

/// `bind` cannot wait for a name to be resolved, so it takes IP addresses
/// only and says what to write instead.
#[cfg(unix)]
#[test]
fn bind_refuses_a_host_name_and_says_what_to_write() {
    giong::block_on(async {
        for f in [TcpListener::bind, TcpListener::bind_balanced] {
            let err = f("localhost:8080").expect_err("bound a host name");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            let code = giong::error::code(&err);
            assert_eq!(code, Some(giong::error::Code::InvalidAddress), "{err}");
            let msg = err.to_string();
            assert!(
                msg.contains("127.0.0.1:8080") && msg.contains("lookup_host"),
                "{msg}"
            );
        }
        // The address forms of `connect` work.
        // On a port held for it: one taken from a listener closed at once
        // can be taken by a connect of a test running at the same time.
        let (_held, port) = common::shared_port();
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    });
}

/// A write dropped while its send waits in the driver may still send its
/// bytes later; a write after it must not overtake them. The server sees
/// the dropped write's bytes (all, some or none) strictly before the next
/// write's.
#[test]
fn a_write_after_a_dropped_write_is_not_reordered() {
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let s = l.accept().await.unwrap();
            // Let the client fill the socket before reading.
            sleep(Duration::from_millis(50)).await;
            let mut all = Vec::new();
            loop {
                let (res, buf) = s.read(Vec::with_capacity(64 * 1024)).await;
                if res.unwrap() == 0 {
                    break;
                }
                all.extend_from_slice(&buf);
            }
            all
        });
        let c = TcpStream::connect(addr).await.unwrap();
        // Far more than the socket buffers hold: the send stays in the driver.
        // Windows would take a send into the kernel whole, whatever its
        // size, while the socket's buffer has room; giong hands it a
        // megabyte at a time, so there too the write waits once the buffer
        // is full.
        let mut dropped = false;
        for _ in 0..4 {
            let big = vec![b'a'; 32 << 20];
            if timeout(Duration::from_millis(5), c.write_all(big))
                .await
                .is_err()
            {
                dropped = true;
                break;
            }
        }
        assert!(dropped, "32 MiB went out in 5 ms, four times over");
        let (res, _) = c.write_all(b"END".to_vec()).await;
        res.unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let all = server.await.unwrap();
        assert!(all.ends_with(b"END"), "the last write was overtaken");
        assert!(all[..all.len() - 3].iter().all(|&b| b == b'a'));
    });
}

/// A write to a peer that has stopped reading waits for it, however large:
/// the kernel holds about a socket buffer's worth, not the whole write. On
/// Windows an overlapped send of 32 MiB, or 512 MiB, was taken whole while
/// the socket's buffer had room, so `write_all` ended at once, the kernel
/// held every byte for the stalled peer, and a write timeout counting from
/// the last progress could never fire. Plain and vectored writes, to a peer
/// that reads once and then nothing until told to; every byte then arrives,
/// in order.
#[test]
fn a_write_to_a_peer_that_stopped_reading_waits_for_it() {
    const BIG: usize = 64 << 20;
    for vectored in [false, true] {
        let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = peer.local_addr().unwrap();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let reader = std::thread::spawn(move || {
            let (mut s, _) = peer.accept().unwrap();
            let mut all = Vec::with_capacity(BIG);
            let mut buf = vec![0u8; 64 * 1024];
            let n = s.read(&mut buf).unwrap();
            all.extend_from_slice(&buf[..n]);
            wait.recv().unwrap();
            loop {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
            }
            all
        });
        giong::block_on(async move {
            let c = TcpStream::connect(addr).await.unwrap();
            let data: Vec<u8> = (0..BIG).map(pattern).collect();
            let started = std::time::Instant::now();
            let res = if vectored {
                let half = data[BIG / 2..].to_vec();
                let bufs = vec![data[..BIG / 2].to_vec(), half];
                let mut write = std::pin::pin!(c.write_all_vectored(bufs));
                let early = timeout(Duration::from_secs(1), write.as_mut()).await;
                assert!(
                    early.is_err(),
                    "a vectored write of {BIG} bytes ended in {:?}, the peer reading nothing",
                    started.elapsed()
                );
                go.send(()).unwrap();
                write.await.0
            } else {
                let mut write = std::pin::pin!(c.write_all(data));
                let early = timeout(Duration::from_secs(1), write.as_mut()).await;
                assert!(
                    early.is_err(),
                    "a write of {BIG} bytes ended in {:?}, the peer reading nothing",
                    started.elapsed()
                );
                go.send(()).unwrap();
                write.await.0
            };
            res.unwrap();
            c.shutdown(Shutdown::Write).unwrap();
        });
        let all = reader.join().unwrap();
        assert_eq!(all.len(), BIG, "vectored: {vectored}");
        assert!(
            all.iter().enumerate().all(|(i, &b)| b == pattern(i)),
            "vectored: {vectored}: a byte out of place"
        );
    }
}

/// Byte `i` of the test stream: a pattern that shows any byte out of place.
fn pattern(i: usize) -> u8 {
    (i % 251) as u8
}

/// `recv_into` (hidden, used by giong-http for large bodies) after `recv`,
/// with data already taken into pool buffers, then `recv` again: every byte
/// arrives once and in order, under either driver.
#[test]
fn recv_into_follows_recv_and_keeps_the_order() {
    const TOTAL: usize = 6 << 20;
    giong::block_on(async {
        let (l, addr) = listener();
        let writer = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let data: Vec<u8> = (0..TOTAL).map(pattern).collect();
            // In rounds, so that each round starts with `recv` and the pooled
            // receive has taken part of it by the time `recv_into` runs.
            for round in data.chunks(TOTAL / 6) {
                c.write_all(round).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let s = l.accept().await.unwrap();
        let mut got: Vec<u8> = Vec::new();
        let mut round = 0;
        while got.len() < TOTAL {
            // A round: some through `recv`, the rest straight into `got`.
            let chunk = s.recv().await.unwrap();
            assert!(!chunk.is_empty(), "the stream ended at {}", got.len());
            got.extend_from_slice(&chunk);
            drop(chunk);
            let end = ((round + 1) * (TOTAL / 6)).min(TOTAL);
            got.reserve(end.saturating_sub(got.len()));
            while got.len() < end {
                let (res, back) = s.recv_into(got).await;
                got = back;
                assert!(res.unwrap() > 0);
            }
            round += 1;
        }
        writer.join().unwrap();
        assert_eq!(got.len(), TOTAL);
        if let Some(i) = (0..TOTAL).find(|&i| got[i] != pattern(i)) {
            panic!("byte {i} is {}, not {}", got[i], pattern(i));
        }
        // The stream ends: `recv_into` says so, and so does `recv` after it.
        let (res, _) = s.recv_into(Vec::new()).await;
        assert_eq!(res.unwrap(), 0);
        assert!(s.recv().await.unwrap().is_empty());
    });
}

/// `recv_into` on a stream that never used `recv`, and with no free
/// capacity in the buffer it is given: it makes room and appends.
#[test]
fn recv_into_appends_without_recv_first() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(&b"world"[..]).await.0.unwrap();
        let mut buf = b"hello ".to_vec();
        buf.shrink_to_fit();
        let (res, buf) = s.recv_into(buf).await;
        assert_eq!(res.unwrap(), 5);
        assert_eq!(buf, b"hello world");
        // `recv` works after it.
        c.write_all(&b"!"[..]).await.0.unwrap();
        assert_eq!(&*s.recv().await.unwrap(), b"!");
    });
}

/// macOS only, in a module of its own: the simulator's copy of this file
/// wraps top-level tests for every platform.
#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    /// macOS has no per-send flag against `SIGPIPE`, so every stream must carry
    /// `SO_NOSIGPIPE`: a process that does not ignore the signal (a library in
    /// a C or Swift program, a Rust program built to keep it) would otherwise
    /// die when a client goes away while the server writes. Connections a
    /// listener accepts included.
    #[test]
    fn accepted_and_connected_streams_never_raise_sigpipe() {
        use std::os::fd::AsRawFd;

        fn no_sigpipe(fd: std::os::fd::RawFd) -> bool {
            let mut on: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: reads an int option into a local of the right size.
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&mut on as *mut libc::c_int).cast(),
                    &mut len,
                )
            };
            assert_eq!(rc, 0, "getsockopt: {}", std::io::Error::last_os_error());
            on != 0
        }

        giong::block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client = giong::spawn(async move { TcpStream::connect(addr).await.unwrap() });
            let server = listener.accept().await.unwrap();
            let client = client.await.unwrap();
            assert!(no_sigpipe(client.as_raw_fd()), "a connected stream");
            assert!(no_sigpipe(server.as_raw_fd()), "an accepted stream");

            let dir = std::env::temp_dir().join(format!("giong-nosigpipe-{}", std::process::id()));
            drop(std::fs::remove_file(&dir));
            let listener = giong::net::UnixListener::bind(&dir).unwrap();
            let path = dir.clone();
            let client =
                giong::spawn(async move { giong::net::UnixStream::connect(&path).await.unwrap() });
            let server = listener.accept().await.unwrap();
            let client = client.await.unwrap();
            assert!(no_sigpipe(client.as_raw_fd()), "a connected Unix stream");
            assert!(no_sigpipe(server.as_raw_fd()), "an accepted Unix stream");
            drop(std::fs::remove_file(&dir));
        });
    }
}

/// Shutting down the read half of a stream ends a `recv` that waits on it
/// with the end of the stream, while the peer keeps its side open: on Unix
/// the kernel does it, on Windows the driver (which Windows leaves waiting).
#[test]
fn shutting_down_the_read_half_ends_a_waiting_recv() {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Rc::new(TcpStream::connect(addr).await.unwrap());
        let server = listener.accept().await.unwrap();
        let reader = {
            let client = client.clone();
            giong::spawn(async move { client.recv().await })
        };
        sleep(Duration::from_millis(50)).await;
        client.shutdown(Shutdown::Read).unwrap();
        let got = timeout(Duration::from_secs(5), reader)
            .await
            .expect("the recv ended")
            .unwrap()
            .unwrap();
        assert!(got.is_empty(), "the end of the stream");
        drop((client, server, listener));
        sleep(Duration::from_millis(50)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

/// The same for a one-off `read`: it returns 0 bytes, the end of the stream.
#[test]
fn shutting_down_the_read_half_ends_a_waiting_read() {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Rc::new(TcpStream::connect(addr).await.unwrap());
        let server = listener.accept().await.unwrap();
        let reader = {
            let client = client.clone();
            giong::spawn(async move { client.read(Vec::with_capacity(64)).await.0 })
        };
        sleep(Duration::from_millis(50)).await;
        client.shutdown(Shutdown::Read).unwrap();
        let got = timeout(Duration::from_secs(5), reader)
            .await
            .expect("the read ended")
            .unwrap()
            .unwrap();
        assert_eq!(got, 0, "the end of the stream");
        drop((client, server, listener));
        sleep(Duration::from_millis(50)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn a_read_or_recv_after_the_read_half_is_shut_gives_the_end_of_the_stream() {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // `read` on one stream, `recv` on another: a stream takes one kind.
        let reading = TcpStream::connect(addr).await.unwrap();
        let reading_peer = listener.accept().await.unwrap();
        let receiving = TcpStream::connect(addr).await.unwrap();
        let receiving_peer = listener.accept().await.unwrap();
        reading.shutdown(Shutdown::Read).unwrap();
        receiving.shutdown(Shutdown::Read).unwrap();
        for _ in 0..2 {
            let (res, _) = timeout(Duration::from_secs(5), reading.read(Vec::with_capacity(64)))
                .await
                .expect("the read ended");
            assert_eq!(res.unwrap(), 0, "the end of the stream");
            let got = timeout(Duration::from_secs(5), receiving.recv())
                .await
                .expect("the recv ended")
                .unwrap();
            assert!(got.is_empty(), "the end of the stream");
        }
        // The write half still works.
        let (res, _) = reading.write_all(b"after".to_vec()).await;
        res.unwrap();
        let got = recv_exact(&reading_peer, 5).await;
        assert_eq!(got, b"after");
        drop((reading, reading_peer, receiving, receiving_peer, listener));
        sleep(Duration::from_millis(50)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}
