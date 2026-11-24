//! Messages between processes over shared memory (`giong::ipc`).

#![cfg(target_os = "linux")]

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use giong::error::{self, Code};
use giong::ipc::{self, IpcListener};
use giong::time::{sleep, timeout};

/// A fresh socket path, removed when dropped.
struct SockPath(PathBuf);

impl SockPath {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("giong-ipc-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Self(p)
    }
}

impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn code(r: &io::Error) -> Option<Code> {
    error::code(r)
}

fn msg_for(i: u32, max: usize) -> Vec<u8> {
    let len = (i as usize * 131) % (max + 1);
    (0..len)
        .map(|b| (b as u32).wrapping_mul(31).wrapping_add(i) as u8)
        .collect()
}

/// The descriptors a listener receives in a handshake (the memfd and the
/// eventfds) are close-on-exec, like every other descriptor here, so a
/// program this process starts does not inherit them.
#[test]
fn an_accepted_channel_s_descriptors_are_close_on_exec() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 1 << 16).unwrap();
        let server = giong::spawn(async move { listener.accept().await.unwrap() });
        let client = ipc::connect(&path.0, 4096).await.unwrap();
        let channel = server.await.unwrap();
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            let fd: RawFd = entry
                .unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            if fd <= 2 {
                continue;
            }
            // SAFETY: only reads the flags of a descriptor number.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            // read_dir's own descriptor may be closed by now.
            if flags >= 0 {
                assert_ne!(
                    flags & libc::FD_CLOEXEC,
                    0,
                    "descriptor {fd} is inherited by exec"
                );
            }
        }
        drop((client, channel));
    });
}

#[test]
fn messages_of_every_size_arrive_in_order_both_ways() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 1 << 16).unwrap();
        let server = giong::spawn(async move {
            let (mut tx, mut rx) = listener.accept().await.unwrap();
            while let Some(m) = rx.recv().await.unwrap() {
                let copy = m.to_vec();
                drop(m);
                tx.send(&copy).await.unwrap();
            }
        });
        // A small ring, so that both directions fill and wrap many times.
        let (mut tx, mut rx) = ipc::connect(&path.0, 4096).await.unwrap();
        let max = tx.max_message();
        assert_eq!(max, 4096 / 4 - 8);
        let reader = giong::spawn(async move {
            for i in 0..20_000u32 {
                let m = rx.recv().await.unwrap().expect("open");
                assert_eq!(&*m, msg_for(i, max).as_slice(), "message {i}");
            }
            assert!(rx.recv().await.unwrap().is_none());
        });
        for i in 0..20_000u32 {
            tx.send(&msg_for(i, max)).await.unwrap();
        }
        drop(tx);
        reader.await.unwrap();
        server.await.unwrap();
    });
}

#[test]
fn a_full_channel_makes_send_wait_and_a_slow_reader_gets_everything() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 4096).unwrap();
        let server = giong::spawn(async move {
            let (_tx, mut rx) = listener.accept().await.unwrap();
            let mut n = 0u32;
            while let Some(m) = rx.recv().await.unwrap() {
                assert_eq!(&m[..4], &n.to_le_bytes());
                n += 1;
                drop(m);
                if n % 50 == 0 {
                    sleep(Duration::from_millis(1)).await;
                }
            }
            n
        });
        let (mut tx, _rx) = ipc::connect(&path.0, 4096).await.unwrap();
        let mut msg = vec![0u8; 500];
        for i in 0..2_000u32 {
            msg[..4].copy_from_slice(&i.to_le_bytes());
            tx.send(&msg).await.unwrap();
        }
        drop(tx);
        assert_eq!(server.await.unwrap(), 2_000);
    });
}

#[test]
fn dropping_the_receiver_fails_later_sends_with_its_code() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 4096).unwrap();
        let server = giong::spawn(async move { listener.accept().await.unwrap() });
        let (mut tx, rx) = ipc::connect(&path.0, 4096).await.unwrap();
        let (_peer_tx, peer_rx) = server.await.unwrap();
        drop(peer_rx);
        let err = tx.send(b"x").await.unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcReceiverClosed), "{err}");
        drop(rx);
    });
}

#[test]
fn a_sender_blocked_on_a_full_channel_wakes_when_the_receiver_is_dropped() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 4096).unwrap();
        let server = giong::spawn(async move { listener.accept().await.unwrap() });
        let (mut tx, _rx) = ipc::connect(&path.0, 4096).await.unwrap();
        let (_peer_tx, peer_rx) = server.await.unwrap();
        let msg = vec![7u8; tx.max_message()];
        let blocked = giong::spawn(async move {
            loop {
                if let Err(e) = tx.send(&msg).await {
                    return e;
                }
            }
        });
        sleep(Duration::from_millis(20)).await;
        drop(peer_rx);
        let err = timeout(Duration::from_secs(5), blocked)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(code(&err), Some(Code::IpcReceiverClosed), "{err}");
    });
}

#[test]
fn limits_are_refused_with_their_codes_and_the_listener_keeps_working() {
    let path = SockPath::new();
    giong::block_on(async {
        let err = IpcListener::bind(&path.0, 1000).unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcCapacity));
        let err = ipc::connect(&path.0, 3 << 12).await.unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcCapacity));

        let listener = IpcListener::bind(&path.0, 8192).unwrap();
        let server = giong::spawn(async move {
            let first = listener.accept().await.unwrap_err();
            assert_eq!(code(&first), Some(Code::IpcCapacity), "{first}");
            listener.accept().await.unwrap()
        });
        let err = ipc::connect(&path.0, 1 << 20).await.unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcCapacity), "{err}");
        let (mut tx, _rx) = ipc::connect(&path.0, 8192).await.unwrap();
        let err = tx.send(&vec![0u8; tx.max_message() + 1]).await.unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcMessageTooLarge), "{err}");
        tx.send(&vec![0u8; tx.max_message()]).await.unwrap();
        let (_ptx, mut prx) = server.await.unwrap();
        assert_eq!(prx.recv().await.unwrap().unwrap().len(), 8192 / 4 - 8);
    });
}

#[test]
fn a_socket_that_is_not_an_ipc_endpoint_fails_the_handshake() {
    let path = SockPath::new();
    giong::block_on(async {
        // A plain Unix server that answers garbage.
        let plain = giong::net::UnixListener::bind(&path.0).unwrap();
        let server = giong::spawn(async move {
            let s = plain.accept().await.unwrap();
            let _ = s.recv().await;
            s.write_all(b"HTTP/1.1 400\r\n\r\n".to_vec())
                .await
                .0
                .unwrap();
            sleep(Duration::from_millis(50)).await;
        });
        let err = ipc::connect(&path.0, 4096).await.unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcHandshake), "{err}");
        server.await.unwrap();
    });
    let path = SockPath::new();
    giong::block_on(async {
        // A plain client that sends garbage to an IPC listener.
        let listener = IpcListener::bind(&path.0, 4096).unwrap();
        let p = path.0.clone();
        let client = std::thread::spawn(move || {
            use std::io::Write;
            let mut s = StdUnixStream::connect(p).unwrap();
            s.write_all(b"GET / HTTP/1.1\r\n").unwrap();
            let mut b = [0u8; 1];
            let _ = std::io::Read::read(&mut s, &mut b);
            b[0]
        });
        let err = listener.accept().await.unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcHandshake), "{err}");
        assert_eq!(client.join().unwrap(), 2);
    });
}

#[test]
fn a_cancelled_recv_loses_no_message() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 4096).unwrap();
        let server = giong::spawn(async move { listener.accept().await.unwrap() });
        let (mut tx, _rx) = ipc::connect(&path.0, 4096).await.unwrap();
        let (_ptx, mut prx) = server.await.unwrap();
        // Times out while sleeping on the eventfd, then receives.
        assert!(timeout(Duration::from_millis(10), prx.recv())
            .await
            .is_err());
        tx.send(b"after").await.unwrap();
        assert_eq!(&*prx.recv().await.unwrap().unwrap(), b"after");
    });
}

#[test]
fn a_message_is_seen_as_soon_as_its_sender_awaits_something_else() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 1 << 16).unwrap();
        let server = giong::spawn(async move { listener.accept().await.unwrap() });
        let (mut tx, _rx) = ipc::connect(&path.0, 1 << 16).await.unwrap();
        let (_ptx, mut prx) = server.await.unwrap();
        let sender = giong::spawn(async move {
            // Far below the size that publishes at once: held back until
            // this task gives the worker back.
            tx.send(b"early").await.unwrap();
            sleep(Duration::from_secs(5)).await;
            drop(tx);
        });
        let got = timeout(Duration::from_secs(1), prx.recv())
            .await
            .expect("the message waited for its sender's sleep")
            .unwrap()
            .unwrap();
        assert_eq!(&*got, b"early");
        drop(got);
        sender.abort();
    });
}

// ------------------------------------------- workers that spin before parking

/// While the worker spins (`spin_before_park`), a waiting receiver is
/// watched from the spin loop instead of sleeping in the kernel.
fn spinning(spin: Duration) -> giong::Builder {
    giong::Builder::new().spin_before_park(spin)
}

#[test]
fn a_spinning_worker_streams_and_echoes_without_losing_a_wake_up() {
    let path = SockPath::new();
    spinning(Duration::from_micros(50))
        .block_on(async {
            let listener = IpcListener::bind(&path.0, 4096).unwrap();
            let server = giong::spawn(async move {
                let (mut tx, mut rx) = listener.accept().await.unwrap();
                while let Some(m) = rx.recv().await.unwrap() {
                    let copy = m.to_vec();
                    drop(m);
                    tx.send(&copy).await.unwrap();
                }
            });
            let (mut tx, mut rx) = ipc::connect(&path.0, 4096).await.unwrap();
            let max = tx.max_message();
            let reader = giong::spawn(async move {
                for i in 0..50_000u32 {
                    let m = rx.recv().await.unwrap().expect("open");
                    assert_eq!(&*m, msg_for(i, max).as_slice(), "message {i}");
                    // Now and then the reader falls behind long enough for
                    // the worker to stop spinning and block.
                    if i % 5_000 == 0 {
                        sleep(Duration::from_millis(2)).await;
                    }
                }
                assert!(rx.recv().await.unwrap().is_none());
            });
            for i in 0..50_000u32 {
                tx.send(&msg_for(i, max)).await.unwrap();
                if i % 7_000 == 0 {
                    sleep(Duration::from_millis(2)).await;
                }
            }
            drop(tx);
            reader.await.unwrap();
            server.await.unwrap();
        })
        .unwrap();
}

#[test]
fn a_recv_cancelled_while_the_worker_spins_loses_nothing() {
    let path = SockPath::new();
    // Spins far longer than the timeout: the timeout fires while the
    // receiver is watched from the spin loop.
    spinning(Duration::from_millis(50))
        .block_on(async {
            let listener = IpcListener::bind(&path.0, 4096).unwrap();
            let server = giong::spawn(async move { listener.accept().await.unwrap() });
            let (mut tx, _rx) = ipc::connect(&path.0, 4096).await.unwrap();
            let (_ptx, mut prx) = server.await.unwrap();
            for round in 0..20u32 {
                assert!(timeout(Duration::from_millis(1), prx.recv()).await.is_err());
                tx.send(&round.to_le_bytes()).await.unwrap();
                assert_eq!(&*prx.recv().await.unwrap().unwrap(), &round.to_le_bytes());
            }
            // And once the worker has blocked: the producer still wakes it.
            let late = giong::spawn(async move {
                sleep(Duration::from_millis(120)).await;
                tx.send(b"late").await.unwrap();
                tx
            });
            assert_eq!(&*prx.recv().await.unwrap().unwrap(), b"late");
            drop(late.await.unwrap());
        })
        .unwrap();
}

// ------------------------------------------------- a peer in another process

/// The child half of the cross-process tests: an echo server that exits
/// when told to, or never returns until killed.
#[test]
#[ignore = "run by the cross-process tests in a child process"]
fn child_echo() {
    let Ok(path) = std::env::var("GIONG_IPC_TEST_PATH") else {
        return;
    };
    giong::block_on(async move {
        let (mut tx, mut rx) = ipc::connect(&path, 1 << 16).await.unwrap();
        while let Some(m) = rx.recv().await.unwrap() {
            let v = m.to_vec();
            drop(m);
            if v == b"hang" {
                // Waits to be killed, with its halves alive.
                std::future::pending::<()>().await;
            }
            tx.send(&v).await.unwrap();
        }
    });
}

fn spawn_child(path: &SockPath) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_echo",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GIONG_IPC_TEST_PATH", &path.0)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn another_process_echoes_and_ends_the_channel_cleanly() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 1 << 16).unwrap();
        let mut child = spawn_child(&path);
        let (mut tx, mut rx) = listener.accept().await.unwrap();
        for i in 0..5_000u32 {
            let msg = msg_for(i, tx.max_message());
            tx.send(&msg).await.unwrap();
            assert_eq!(&*rx.recv().await.unwrap().unwrap(), msg.as_slice());
        }
        drop(tx);
        assert!(rx.recv().await.unwrap().is_none());
        assert!(child.wait().unwrap().success());
    });
}

#[test]
fn a_killed_process_is_reported_with_its_code() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 1 << 16).unwrap();
        let mut child = spawn_child(&path);
        let (mut tx, mut rx) = listener.accept().await.unwrap();
        tx.send(b"one").await.unwrap();
        assert_eq!(&*rx.recv().await.unwrap().unwrap(), b"one");
        tx.send(b"hang").await.unwrap();
        sleep(Duration::from_millis(50)).await;
        child.kill().unwrap();
        child.wait().unwrap();
        let err = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("noticed in time")
            .unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcPeerGone), "{err}");
    });
}

// ------------------------------------------------------- a hostile peer

/// Does the client side of the handshake by hand and returns the socket and
/// the mapping, so that a test can break the rules.
fn raw_client(path: &SockPath, capacity: usize) -> (StdUnixStream, *mut u8, [RawFd; 4]) {
    let sock = StdUnixStream::connect(&path.0).unwrap();
    let len = 4096 + 2 * capacity;
    // SAFETY: plain system calls with valid arguments.
    unsafe {
        let mem = libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
        assert!(mem >= 0);
        assert_eq!(libc::ftruncate(mem, len as libc::off_t), 0);
        assert_eq!(
            libc::fcntl(
                mem,
                libc::F_ADD_SEALS,
                libc::F_SEAL_SHRINK | libc::F_SEAL_GROW
            ),
            0
        );
        let base = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            mem,
            0,
        );
        assert_ne!(base, libc::MAP_FAILED);
        let base = base.cast::<u8>();
        let mut data = [0u8; 16];
        data[..8].copy_from_slice(b"GIONGIPC");
        data[8..12].copy_from_slice(&1u32.to_le_bytes());
        data[12..16].copy_from_slice(&(capacity as u32).to_le_bytes());
        std::ptr::copy_nonoverlapping(data.as_ptr(), base, 16);
        let efds = [0; 4].map(|_| libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK));
        let fds = [mem, efds[0], efds[1], efds[2], efds[3]];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: 16,
        };
        let mut cbuf = [0u64; 8];
        let mut hdr: libc::msghdr = std::mem::zeroed();
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cbuf.as_mut_ptr().cast();
        hdr.msg_controllen = libc::CMSG_SPACE(20) as _;
        let c = libc::CMSG_FIRSTHDR(&hdr);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(20) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), 5);
        assert_eq!(libc::sendmsg(sock.as_raw_fd(), &hdr, 0), 16);
        libc::close(mem);
        (sock, base, efds)
    }
}

#[test]
fn a_peer_that_breaks_the_ring_is_reported_not_followed() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 4096).unwrap();
        let p = SockPath(path.0.clone());
        let client = std::thread::spawn(move || {
            let (sock, base, efds) = raw_client(&p, 4096);
            std::mem::forget(p);
            let mut b = [0u8; 1];
            std::io::Read::read_exact(&mut &sock, &mut b).unwrap();
            assert_eq!(b[0], 0);
            // Dir 0 (client to listener) tail is the first word of the line
            // at offset 64: claim far more data than the ring holds.
            // SAFETY: inside the mapping; the listener reads it atomically.
            unsafe {
                (*base.add(64).cast::<AtomicU64>()).store(1 << 40, Ordering::Release);
                libc::write(efds[0], (&1u64 as *const u64).cast(), 8);
            }
            std::thread::sleep(Duration::from_millis(200));
            drop(sock);
        });
        let (_tx, mut rx) = listener.accept().await.unwrap();
        let err = rx.recv().await.unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcCorrupted), "{err}");
        client.join().unwrap();
    });
}

#[test]
fn a_peer_that_closes_its_socket_mid_channel_is_gone_after_its_messages() {
    let path = SockPath::new();
    giong::block_on(async {
        let listener = IpcListener::bind(&path.0, 4096).unwrap();
        let p = SockPath(path.0.clone());
        let client = std::thread::spawn(move || {
            let (sock, _base, _efds) = raw_client(&p, 4096);
            std::mem::forget(p);
            let mut b = [0u8; 1];
            std::io::Read::read_exact(&mut &sock, &mut b).unwrap();
            drop(sock);
        });
        let (_tx, mut rx) = listener.accept().await.unwrap();
        client.join().unwrap();
        let err = timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(code(&err), Some(Code::IpcPeerGone), "{err}");
    });
}
