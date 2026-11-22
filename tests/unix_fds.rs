//! Unix datagram sockets, abstract names, peer credentials, and passing
//! descriptors between processes (`SCM_RIGHTS`).
//!
//! Linux only: descriptor passing, peer credentials, abstract names and
//! `UnixDatagram` are Linux only for now.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::future::{poll_fn, Future};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::SocketAddr;
use std::path::PathBuf;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use giong::error::{self, Code};
use giong::net::{TcpListener, UnixDatagram, UnixListener, UnixStream, MAX_FDS};
use giong::time::sleep;

struct SocketPath(PathBuf);

impl SocketPath {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("giong-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A pipe as two blocking `File`s (`std::io::pipe` is newer than the
/// minimum Rust version).
fn os_pipe() -> std::io::Result<(File, File)> {
    let mut fds = [0; 2];
    // SAFETY: a valid pointer to two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: two new descriptors we own.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

fn abstract_name(name: &str) -> SocketAddr {
    SocketAddr::from_abstract_name(format!("giong-test-{}-{name}", std::process::id())).unwrap()
}

/// `true` if every write end of the pipe whose read end is `reader` is
/// closed: a non-blocking read sees end of file instead of "would block".
fn all_writers_closed(reader: &mut File) -> bool {
    let fd = reader.as_raw_fd();
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) => panic!("read failed: {e}"),
        }
    }
}

#[test]
fn datagrams_keep_their_boundaries_and_senders() {
    giong::block_on(async {
        let server_path = SocketPath::new("dgram-server");
        let client_path = SocketPath::new("dgram-client");
        let server = UnixDatagram::bind(&server_path.0).unwrap();
        let client = UnixDatagram::bind(&client_path.0).unwrap();
        for msg in [&b"one"[..], b"", b"three three"] {
            let (n, _) = client.send_to(msg, &server_path.0).await;
            assert_eq!(n.unwrap(), msg.len());
        }
        for msg in [&b"one"[..], b"", b"three three"] {
            let (res, buf) = server.recv_from(Vec::with_capacity(64)).await;
            let (n, from) = res.unwrap();
            assert_eq!(n, msg.len());
            assert_eq!(buf, msg);
            assert_eq!(from.as_pathname(), Some(client_path.0.as_path()));
            // Answer the sender at the address it came from.
            let (res, _) = server.send_to_addr(&b"ack"[..], &from).await;
            res.unwrap();
        }
        for _ in 0..3 {
            let (res, buf) = client.recv(Vec::with_capacity(8)).await;
            assert_eq!(res.unwrap(), 3);
            assert_eq!(buf, b"ack");
        }
        assert_eq!(
            server.local_addr().unwrap().as_pathname(),
            Some(server_path.0.as_path())
        );
    });
}

#[test]
fn a_long_datagram_is_cut_to_the_buffer() {
    giong::block_on(async {
        let (a, b) = UnixDatagram::pair().unwrap();
        a.send(vec![7u8; 1000]).await.0.unwrap();
        let (res, buf) = b.recv(Vec::with_capacity(100)).await;
        assert_eq!(res.unwrap(), 100);
        assert_eq!(buf, vec![7u8; 100]);
        // The rest is discarded, not delivered as another message.
        a.send(&b"next"[..]).await.0.unwrap();
        let (res, buf) = b.recv(Vec::with_capacity(100)).await;
        assert_eq!(res.unwrap(), 4);
        assert_eq!(buf, b"next");
    });
}

#[test]
fn unbound_senders_show_up_unnamed_and_connected_sockets_talk() {
    giong::block_on(async {
        let path = SocketPath::new("dgram-unbound");
        let server = UnixDatagram::bind(&path.0).unwrap();
        let client = UnixDatagram::unbound().unwrap();
        client.send_to(&b"hi"[..], &path.0).await.0.unwrap();
        let (res, _) = server.recv_from(Vec::with_capacity(8)).await;
        let (_, from) = res.unwrap();
        assert!(from.is_unnamed());
        assert!(server.send_to_addr(&b"x"[..], &from).await.0.is_err());

        client.connect(&path.0).unwrap();
        assert_eq!(
            client.peer_addr().unwrap().as_pathname(),
            Some(path.0.as_path())
        );
        client.send(&b"connected"[..]).await.0.unwrap();
        let (res, buf) = server.recv(Vec::with_capacity(16)).await;
        assert_eq!(res.unwrap(), 9);
        assert_eq!(buf, b"connected");
    });
}

#[test]
fn abstract_names_work_for_streams_and_datagrams() {
    giong::block_on(async {
        let addr = abstract_name("stream");
        let listener = UnixListener::bind_addr(&addr).unwrap();
        assert_eq!(
            listener.local_addr().unwrap().as_abstract_name(),
            addr.as_abstract_name()
        );
        let accept = giong::spawn(async move { listener.accept().await.unwrap() });
        let client = UnixStream::connect_addr(&addr).await.unwrap();
        let server = accept.await.unwrap();
        client.write_all(&b"abstract"[..]).await.0.unwrap();
        let (res, buf) = server.read(Vec::with_capacity(16)).await;
        assert_eq!(res.unwrap(), 8);
        assert_eq!(buf, b"abstract");

        let daddr = abstract_name("dgram");
        let d = UnixDatagram::bind_addr(&daddr).unwrap();
        let sender = UnixDatagram::unbound().unwrap();
        sender.send_to_addr(&b"abs"[..], &daddr).await.0.unwrap();
        let (res, buf) = d.recv(Vec::with_capacity(8)).await;
        assert_eq!(res.unwrap(), 3);
        assert_eq!(buf, b"abs");
    });
}

#[test]
fn peer_credentials_name_this_process() {
    giong::block_on(async {
        let (a, _b) = UnixStream::pair().unwrap();
        let cred = a.peer_cred().unwrap();
        assert_eq!(cred.pid, std::process::id() as i32);
        // SAFETY: plain getters.
        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        assert_eq!(cred.gid, unsafe { libc::getegid() });
        let (d, _e) = UnixDatagram::pair().unwrap();
        assert_eq!(d.peer_cred().unwrap().pid, std::process::id() as i32);
    });
}

#[test]
fn a_pipe_passed_over_a_stream_reaches_the_same_pipe() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut reader, writer) = os_pipe().unwrap();
        let (res, _) = a.send_with_fds(&b"pipe"[..], &[writer.as_fd()]).await;
        assert_eq!(res.unwrap(), 4);
        drop(writer); // ours; the peer has its own copy now

        let (res, buf) = b.recv_with_fds(Vec::with_capacity(16)).await;
        let (n, fds) = res.unwrap();
        assert_eq!(n, 4);
        assert_eq!(buf, b"pipe");
        assert_eq!(fds.len(), 1);
        // Closed on exec, as std's own descriptors are.
        // SAFETY: fcntl on a descriptor we own.
        let flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);

        let mut writer = File::from(fds.into_iter().next().unwrap());
        writer.write_all(b"through").unwrap();
        drop(writer);
        let mut got = String::new();
        reader.read_to_string(&mut got).unwrap();
        assert_eq!(got, "through");
    });
}

#[test]
fn a_listener_passed_over_a_datagram_socket_accepts_connections() {
    giong::block_on(async {
        let (a, b) = UnixDatagram::pair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        a.send_with_fds(&b"L"[..], &[listener.as_fd()])
            .await
            .0
            .unwrap();
        drop(listener);

        let (res, _) = b.recv_with_fds(Vec::with_capacity(4)).await;
        let (_, mut fds) = res.unwrap();
        let fd: OwnedFd = fds.pop().unwrap();
        let handed = TcpListener::from_std(std::net::TcpListener::from(fd));
        assert_eq!(handed.local_addr().unwrap(), addr);
        let client = giong::spawn(async move {
            let s = giong::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(&b"ok"[..]).await.0.unwrap();
        });
        let conn = handed.accept().await.unwrap();
        let (res, buf) = conn.read(Vec::with_capacity(4)).await;
        assert_eq!(res.unwrap(), 2);
        assert_eq!(buf, b"ok");
        client.await.unwrap();
    });
}

#[test]
fn many_descriptors_in_one_message() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let pipes: Vec<_> = (0..MAX_FDS).map(|_| os_pipe().unwrap()).collect();
        let fds: Vec<_> = pipes.iter().map(|(_, w)| w.as_fd()).collect();
        let (res, _) = a.send_with_fds(&b"many"[..], &fds).await;
        res.unwrap();
        let (res, _) = b.recv_with_fds(Vec::with_capacity(8)).await;
        assert_eq!(res.unwrap().1.len(), MAX_FDS);
    });
}

#[test]
fn passing_mistakes_have_codes_and_send_nothing() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let (_r, w) = os_pipe().unwrap();
        let too_many = vec![w.as_fd(); MAX_FDS + 1];
        let (res, _) = a.send_with_fds(&b"x"[..], &too_many).await;
        assert_eq!(error::code(&res.unwrap_err()), Some(Code::FdPassing));
        let (res, _) = a.send_with_fds(&b""[..], &[w.as_fd()]).await;
        assert_eq!(error::code(&res.unwrap_err()), Some(Code::FdPassing));

        // Nothing went out: the next read gets the next message.
        a.send_with_fds(&b"real"[..], &[]).await.0.unwrap();
        let (res, buf) = b.recv_with_fds(Vec::with_capacity(8)).await;
        let (n, fds) = res.unwrap();
        assert_eq!((n, fds.len()), (4, 0));
        assert_eq!(buf, b"real");
    });
}

#[test]
fn recv_and_recv_with_fds_do_not_mix() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        a.write_all(&b"x"[..]).await.0.unwrap();
        b.recv().await.unwrap();
        let (res, _) = b.recv_with_fds(Vec::with_capacity(8)).await;
        assert_eq!(error::code(&res.unwrap_err()), Some(Code::MixedReads));

        let (c, d) = UnixStream::pair().unwrap();
        c.write_all(&b"y"[..]).await.0.unwrap();
        d.recv_with_fds(Vec::with_capacity(8)).await.0.unwrap();
        let err = d.recv().await.unwrap_err();
        assert_eq!(error::code(&err), Some(Code::MixedReads));
    });
}

/// A receive whose future is dropped after the kernel delivered descriptors
/// closes them; one dropped before leaves the message queued, and closing
/// the socket closes them. Either way nothing leaks.
#[test]
fn a_dropped_receive_never_leaks_descriptors() {
    giong::block_on(async {
        for _ in 0..20 {
            let (a, b) = UnixStream::pair().unwrap();
            let (mut reader, writer) = os_pipe().unwrap();
            {
                let mut recv = pin!(b.recv_with_fds(Vec::with_capacity(8)));
                // Start the receive, then let the send complete it without
                // polling it again.
                let started = poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx).is_pending())).await;
                assert!(started);
                a.send_with_fds(&b"f"[..], &[writer.as_fd()])
                    .await
                    .0
                    .unwrap();
                sleep(Duration::from_millis(1)).await;
            }
            drop(writer);
            drop(b); // closes anything still queued
            sleep(Duration::from_millis(5)).await;
            assert!(
                all_writers_closed(&mut reader),
                "a received descriptor leaked"
            );
        }
        sleep(Duration::from_millis(20)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

/// The send keeps its own copy of each descriptor: closing the caller's
/// while a send waits for room cannot send another file.
#[test]
fn a_dropped_send_leaves_nothing_behind() {
    giong::block_on(async {
        let (a, b) = UnixDatagram::pair().unwrap();
        // Fill the socket until a send has to wait.
        loop {
            let res =
                giong::time::timeout(Duration::from_millis(20), a.send(vec![0u8; 4096])).await;
            if res.is_err() {
                break;
            }
        }
        let (mut reader, writer) = os_pipe().unwrap();
        let res = giong::time::timeout(
            Duration::from_millis(20),
            a.send_with_fds(&b"late"[..], &[writer.as_fd()]),
        )
        .await;
        assert!(res.is_err(), "the socket should have been full");
        drop(writer);
        drop(a);
        drop(b);
        sleep(Duration::from_millis(20)).await;
        assert!(all_writers_closed(&mut reader));
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}
