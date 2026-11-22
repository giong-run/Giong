//! Socket activation, as systemd does it: the test starts a copy of itself
//! with sockets on descriptors 3 and 4 and `LISTEN_*` set, through a shell
//! that sets `LISTEN_PID` to its own process ID before it `exec`s.
//!
//! Linux only: socket activation is systemd's, Linux only.

#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use giong::error::{self, Code};

#[test]
fn activated_child() {
    if std::env::var_os("GIONG_ACTIVATION_CHILD").is_none() {
        return;
    }
    giong::block_on(async {
        let sockets = giong::net::activated_sockets().unwrap();
        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0].name(), Some("web"));
        assert_eq!(sockets[1].name(), Some("dns"));
        // The wrong kind is refused, with what the socket is.
        let err = sockets[1].tcp_listener().unwrap_err();
        assert_eq!(error::code(&err), Some(Code::ActivatedSocket));
        assert!(err.to_string().contains("IPv4 datagram"), "{err}");
        let err = giong::net::activated_sockets().unwrap_err();
        assert_eq!(error::code(&err), Some(Code::ActivatedSocket));
        let _udp = sockets[1].udp_socket().unwrap();

        // Two listeners on the one inherited socket.
        let a = sockets[0].tcp_listener().unwrap();
        let b = sockets[0].tcp_listener().unwrap();
        drop(b);
        let conn = a.accept().await.unwrap();
        conn.write_all(&b"activated"[..]).await.0.unwrap();
    });
}

#[test]
fn listeners_come_from_the_service_manager() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    // Copies far from 3 and 4, so that moving them there cannot clobber one
    // another (and `dup2` onto another number clears close-on-exec).
    // SAFETY: fcntl on descriptors we own; the copies close on exec.
    let (l, u) = unsafe {
        (
            libc::fcntl(listener.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100),
            libc::fcntl(udp.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 100),
        )
    };
    assert!(l >= 100 && u >= 100);
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "LISTEN_PID=$$ exec \"$0\" \"$@\""])
        .arg(exe)
        .args(["--exact", "activated_child", "--quiet", "--test-threads=1"])
        .env("GIONG_ACTIVATION_CHILD", "1")
        .env("LISTEN_FDS", "2")
        .env("LISTEN_FDNAMES", "web:dns")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: only dup2 between fork and exec, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(l, 3) < 0 || libc::dup2(u, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    // Only the child accepts from here on: if it fails before accepting,
    // the connect is refused instead of waiting forever.
    drop(listener);
    drop(udp);
    // SAFETY: our copies, unused from here on.
    unsafe {
        libc::close(l);
        libc::close(u);
    }
    let mut got = String::new();
    if let Ok(mut conn) = std::net::TcpStream::connect(addr) {
        let _ = conn.read_to_string(&mut got);
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(got, "activated");
}

#[test]
fn without_a_service_manager_there_are_none() {
    if std::env::var_os("GIONG_ACTIVATION_CHILD").is_some() {
        return;
    }
    // LISTEN_PID is not this process.
    assert!(giong::net::activated_sockets().unwrap().is_empty());
}
