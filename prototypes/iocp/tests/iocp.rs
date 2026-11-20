//! The completion-port driver against real sockets and files.
//!
//! Every test runs twice: with completions that finish inside the call
//! reported at once (the default), and with every completion going through
//! the port.

#![cfg(windows)]

use std::cell::Cell;
use std::io;
use std::mem;
use std::net::{SocketAddr, TcpListener as StdListener, TcpStream as StdStream, UdpSocket};
use std::os::windows::io::{AsRawHandle, AsRawSocket, IntoRawSocket};
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong_iocp_prototype::driver::{Driver, Io, Park, ACCEPT_ADDR_LEN};
use giong_iocp_prototype::op::{block_on, sleep, with_driver, Op};
use socket2::{Domain, SockAddr, Socket, Type};
use windows_sys::Win32::Foundation::{ERROR_OPERATION_ABORTED, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    SOCKET, WSAECONNREFUSED, WSAECONNRESET, WSAEMSGSIZE,
};

/// Run `test` under both completion modes.
fn both<F, Fut>(test: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for inline in [true, false] {
        let driver = Driver::with_inline_completions(inline).expect("driver");
        let ((), mut driver) = block_on(driver, test());
        assert!(
            driver.shutdown(Duration::from_secs(5)),
            "operations still in flight (inline = {inline})"
        );
        assert_eq!(driver.inflight(), 0);
        assert_eq!(driver.tracked_ops(), 0, "an operation leaked its key");
    }
}

fn err(res: i32) -> io::Error {
    io::Error::from_raw_os_error(-res)
}

/// A socket owned by a test, registered with this thread's driver.
struct Sock(Socket);

impl Sock {
    fn tcp() -> Sock {
        let s = Socket::new(Domain::IPV4, Type::STREAM, None).expect("socket");
        with_driver(|d| d.register_socket(s.as_raw_socket() as SOCKET)).expect("register");
        Sock(s)
    }

    fn raw(&self) -> SOCKET {
        self.0.as_raw_socket() as SOCKET
    }

    async fn recv(&self, mut buf: Vec<u8>) -> (i32, Vec<u8>) {
        let io = Io::Recv {
            socket: self.raw(),
            buf: buf.as_mut_ptr(),
            len: buf.capacity() as u32,
        };
        // SAFETY: the pointer is the Vec's heap block, which moves with it.
        let (res, mut buf) = unsafe { Op::new(io, buf) }.await;
        if res > 0 {
            // SAFETY: the kernel wrote `res` bytes.
            unsafe { buf.set_len(res as usize) };
        }
        (res, buf)
    }

    async fn send(&self, buf: Vec<u8>) -> i32 {
        let io = Io::Send {
            socket: self.raw(),
            buf: buf.as_ptr(),
            len: buf.len() as u32,
        };
        // SAFETY: as in `recv`.
        unsafe { Op::new(io, buf) }.await.0
    }

    async fn send_all(&self, data: &[u8]) {
        let mut sent = 0;
        while sent < data.len() {
            let n = self.send(data[sent..].to_vec()).await;
            assert!(n > 0, "send: {}", err(n));
            sent += n as usize;
        }
    }

    async fn connect(addr: SocketAddr) -> Result<Sock, io::Error> {
        let s = Sock::tcp();
        // ConnectEx wants a bound socket.
        s.0.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], 0))))?;
        let addr = Box::new(SockAddr::from(addr));
        let io = Io::Connect {
            socket: s.raw(),
            addr: addr.as_ptr().cast(),
            len: addr.len(),
        };
        // SAFETY: the address is boxed, so it stays put.
        let (res, _addr) = unsafe { Op::new(io, addr) }.await;
        if res < 0 {
            return Err(err(res));
        }
        Ok(s)
    }
}

struct Listener(Socket);

/// What an accept points at: the socket made ahead and the address block.
struct Accepting {
    socket: Option<Socket>,
    addrs: Vec<u8>,
    dropped: Option<Rc<Cell<bool>>>,
}

impl Drop for Accepting {
    fn drop(&mut self) {
        if let Some(flag) = &self.dropped {
            flag.set(true);
        }
    }
}

impl Listener {
    fn bind() -> (Listener, SocketAddr) {
        let s = Socket::new(Domain::IPV4, Type::STREAM, None).expect("socket");
        s.bind(&SockAddr::from(SocketAddr::from(([127, 0, 0, 1], 0))))
            .expect("bind");
        s.listen(128).expect("listen");
        with_driver(|d| d.register_socket(s.as_raw_socket() as SOCKET)).expect("register");
        let addr = s.local_addr().unwrap().as_socket().unwrap();
        (Listener(s), addr)
    }

    fn accept_op(&self, dropped: Option<Rc<Cell<bool>>>) -> Op<Accepting> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).expect("socket");
        let mut data = Accepting {
            socket: Some(socket),
            addrs: vec![0u8; 2 * ACCEPT_ADDR_LEN],
            dropped,
        };
        let io = Io::Accept {
            listener: self.0.as_raw_socket() as SOCKET,
            socket: data.socket.as_ref().unwrap().as_raw_socket() as SOCKET,
            addrs: data.addrs.as_mut_ptr(),
        };
        // SAFETY: the address block is the Vec's heap memory; the socket
        // is owned by `data`.
        unsafe { Op::new(io, data) }
    }

    async fn accept(&self) -> Result<Sock, io::Error> {
        let (res, mut data) = self.accept_op(None).await;
        if res < 0 {
            return Err(err(res));
        }
        let socket = data.socket.take().unwrap();
        with_driver(|d| d.register_socket(socket.as_raw_socket() as SOCKET))?;
        Ok(Sock(socket))
    }
}

/// Poll `future` once and report whether it is still pending.
async fn poll_once<F: std::future::Future + Unpin>(future: &mut F) -> Option<F::Output> {
    std::future::poll_fn(|cx| match std::pin::Pin::new(&mut *future).poll(cx) {
        std::task::Poll::Ready(out) => std::task::Poll::Ready(Some(out)),
        std::task::Poll::Pending => std::task::Poll::Ready(None),
    })
    .await
}

#[test]
fn tcp_echo_through_accept_and_connect() {
    both(|| async {
        let (listener, addr) = Listener::bind();
        let mut accept = Box::pin(listener.accept());
        assert!(
            poll_once(&mut accept).await.is_none(),
            "nobody connected yet"
        );
        let client = Sock::connect(addr).await.expect("connect");
        let server = accept.await.expect("accept");

        // An accepted and a connected socket are ordinary sockets: these
        // calls fail unless the driver updated their context.
        assert_eq!(
            server.0.peer_addr().unwrap().as_socket().unwrap(),
            client.0.local_addr().unwrap().as_socket().unwrap()
        );
        assert_eq!(client.0.peer_addr().unwrap().as_socket().unwrap(), addr);

        client.send_all(b"hello over a completion port").await;
        let (n, buf) = server.recv(Vec::with_capacity(64)).await;
        assert_eq!(n, 28, "{}", err(n));
        assert_eq!(&buf[..], b"hello over a completion port");
        server.send_all(&buf).await;
        let (n, buf) = client.recv(Vec::with_capacity(64)).await;
        assert_eq!(n, 28);
        assert_eq!(&buf[..], b"hello over a completion port");

        client
            .0
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown after ConnectEx");
        let (n, _) = server.recv(Vec::with_capacity(64)).await;
        assert_eq!(n, 0, "end of stream is a receive of 0 bytes");
    });
}

#[test]
fn a_megabyte_arrives_whole_in_both_directions() {
    both(|| async {
        let (listener, addr) = Listener::bind();
        let client = Sock::connect(addr).await.expect("connect");
        let server = listener.accept().await.expect("accept");
        let data: Vec<u8> = (0..1 << 20).map(|i| (i * 31 % 251) as u8).collect();

        for (from, to) in [(&client, &server), (&server, &client)] {
            // One send larger than the socket buffers stays in flight
            // while the receives drain it.
            let whole = data.clone();
            let io = Io::Send {
                socket: from.raw(),
                buf: whole.as_ptr(),
                len: whole.len() as u32,
            };
            // SAFETY: `whole` owns the bytes the send points at.
            let mut send = unsafe { Op::new(io, whole) };
            let mut sent = poll_once(&mut send).await.map(|(n, _)| n);
            let mut got = Vec::with_capacity(data.len());
            while got.len() < data.len() {
                let (n, buf) = to.recv(Vec::with_capacity(16 * 1024)).await;
                assert!(n > 0, "recv: {}", err(n));
                got.extend_from_slice(&buf);
            }
            if sent.is_none() {
                sent = Some(send.await.0);
            }
            assert_eq!(sent, Some(data.len() as i32), "an overlapped send is whole");
            assert!(got == data, "bytes differ");
        }
    });
}

#[test]
fn a_dropped_receive_keeps_its_buffer_until_the_kernel_lets_go() {
    struct Guarded {
        buf: Vec<u8>,
        dropped: Rc<Cell<bool>>,
    }
    impl Drop for Guarded {
        fn drop(&mut self) {
            self.dropped.set(true);
        }
    }

    both(|| async {
        let (listener, addr) = Listener::bind();
        let client = Sock::connect(addr).await.expect("connect");
        let server = listener.accept().await.expect("accept");

        let dropped = Rc::new(Cell::new(false));
        let mut data = Guarded {
            buf: vec![0u8; 4096],
            dropped: dropped.clone(),
        };
        let io = Io::Recv {
            socket: server.raw(),
            buf: data.buf.as_mut_ptr(),
            len: 4096,
        };
        // SAFETY: the buffer is the Vec's heap block, owned by `data`.
        let mut recv = unsafe { Op::new(io, data) };
        assert!(poll_once(&mut recv).await.is_none());
        assert_eq!(with_driver(|d| d.inflight()), 1);

        drop(recv);
        // The cancel was requested, not confirmed: the buffer must still
        // be alive, because the kernel may yet write into it.
        assert!(!dropped.get(), "buffer freed before the completion arrived");
        assert_eq!(with_driver(|d| d.inflight()), 1);
        assert_eq!(with_driver(|d| d.tracked_ops()), 0);

        let deadline = Instant::now() + Duration::from_secs(5);
        while with_driver(|d| d.inflight()) > 0 {
            assert!(
                Instant::now() < deadline,
                "the cancelled receive never completed"
            );
            with_driver(|d| d.park(Park::Wait(Some(Duration::from_millis(100)))));
        }
        assert!(dropped.get(), "buffer kept after the completion");

        // The socket is intact: the next receive gets the next bytes.
        client.send_all(b"after the cancel").await;
        let (n, buf) = server.recv(Vec::with_capacity(64)).await;
        assert_eq!(&buf[..n as usize], b"after the cancel");
    });
}

#[test]
fn a_dropped_accept_closes_the_socket_made_for_it() {
    both(|| async {
        let (listener, addr) = Listener::bind();
        let dropped = Rc::new(Cell::new(false));
        let mut accept = listener.accept_op(Some(dropped.clone()));
        assert!(poll_once(&mut accept).await.is_none());
        drop(accept);
        assert!(!dropped.get(), "accept socket freed while AcceptEx held it");
        let deadline = Instant::now() + Duration::from_secs(5);
        while with_driver(|d| d.inflight()) > 0 {
            assert!(
                Instant::now() < deadline,
                "the cancelled accept never completed"
            );
            with_driver(|d| d.park(Park::Wait(Some(Duration::from_millis(100)))));
        }
        assert!(dropped.get());

        // The listener still accepts.
        let mut accept = Box::pin(listener.accept());
        assert!(poll_once(&mut accept).await.is_none());
        let client = Sock::connect(addr).await.expect("connect");
        let server = accept.await.expect("accept after a cancelled accept");
        client.send_all(b"x").await;
        assert_eq!(server.recv(Vec::with_capacity(8)).await.0, 1);
    });
}

#[test]
fn a_receive_cancelled_as_data_arrives_takes_the_bytes_or_leaves_them() {
    // The race the contract is about: the data and the cancel cross. The
    // operation then completes with the bytes (which a dropped future
    // throws away, as on io_uring) or aborted (the bytes stay in the
    // socket). Never half, never out of order, and its buffer is released
    // only after the verdict.
    both(|| async {
        use std::io::Write;
        let (listener, addr) = Listener::bind();
        let client = Sock::connect(addr).await.expect("connect");
        let server = listener.accept().await.expect("accept");
        // SAFETY: a second handle to the client's socket, for blocking
        // writes; it owns the duplicate.
        let mut peer: StdStream = unsafe {
            use std::os::windows::io::FromRawSocket;
            StdStream::from_raw_socket(client.0.try_clone().unwrap().into_raw_socket())
        };
        let (mut taken, mut left) = (0, 0);
        for i in 0..200u32 {
            let mut recv = Box::pin(server.recv(Vec::with_capacity(4)));
            assert!(poll_once(&mut recv).await.is_none());
            peer.write_all(&i.to_le_bytes()).unwrap();
            if i % 2 == 0 {
                std::thread::yield_now();
            }
            drop(recv);
            while with_driver(|d| d.inflight()) > 0 {
                with_driver(|d| d.park(Park::Wait(Some(Duration::from_millis(50)))));
            }
            // A marker tells which way it went.
            peer.write_all(&u32::MAX.to_le_bytes()).unwrap();
            let mut got = Vec::new();
            while got.len() < 4 || got[got.len() - 4..] != u32::MAX.to_le_bytes() {
                let (n, buf) = server.recv(Vec::with_capacity(16)).await;
                assert!(n > 0, "{}", err(n));
                got.extend_from_slice(&buf);
            }
            if got.len() == 4 {
                taken += 1;
            } else {
                assert_eq!(got[..4], i.to_le_bytes(), "bytes out of order");
                assert_eq!(got.len(), 8);
                left += 1;
            }
        }
        assert_eq!(taken + left, 200);
    });
}

#[test]
fn a_zero_byte_receive_waits_for_data_and_holds_no_buffer() {
    both(|| async {
        let (listener, addr) = Listener::bind();
        let client = Sock::connect(addr).await.expect("connect");
        let server = listener.accept().await.expect("accept");

        let io = Io::Recv {
            socket: server.raw(),
            buf: std::ptr::null_mut(),
            len: 0,
        };
        // SAFETY: a length of 0 touches no memory.
        let mut wait = unsafe { Op::new(io, ()) };
        assert!(poll_once(&mut wait).await.is_none(), "no data yet");
        client.send_all(b"wake").await;
        let (res, ()) = wait.await;
        assert_eq!(res, 0, "{}", err(res));
        // The data is still there for the real receive, which now
        // finishes without waiting.
        let (n, buf) = server.recv(Vec::with_capacity(16)).await;
        assert_eq!(&buf[..n as usize], b"wake");

        // It also wakes for the end of the stream.
        // SAFETY: as above.
        let mut wait = unsafe { Op::new(io, ()) };
        assert!(poll_once(&mut wait).await.is_none());
        drop(client);
        let (res, ()) = wait.await;
        assert!(res == 0 || res == -WSAECONNRESET, "{}", err(res));
    });
}

#[test]
fn connecting_to_a_closed_port_is_refused_with_its_error_code() {
    both(|| async {
        let addr = {
            let l = StdListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let e = Sock::connect(addr).await.err().expect("refused");
        assert_eq!(e.raw_os_error(), Some(WSAECONNREFUSED));
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
    });
}

#[test]
fn a_peer_that_resets_is_reported_as_a_reset() {
    both(|| async {
        let (listener, addr) = Listener::bind();
        let client = Sock::connect(addr).await.expect("connect");
        let server = listener.accept().await.expect("accept");
        client.0.set_linger(Some(Duration::ZERO)).unwrap();
        drop(client);
        let (res, _) = server.recv(Vec::with_capacity(16)).await;
        assert_eq!(res, -WSAECONNRESET, "{}", err(res));
        assert_eq!(err(res).kind(), io::ErrorKind::ConnectionReset);
    });
}

#[test]
fn many_accepts_wait_at_once_and_each_gets_one_connection() {
    both(|| async {
        let (listener, addr) = Listener::bind();
        let mut accepts: Vec<_> = (0..32).map(|_| Box::pin(listener.accept())).collect();
        for a in &mut accepts {
            assert!(poll_once(a).await.is_none());
        }
        assert_eq!(with_driver(|d| d.inflight()), 32);
        let mut clients = Vec::new();
        for i in 0..32u8 {
            let c = Sock::connect(addr).await.expect("connect");
            c.send_all(&[i]).await;
            clients.push(c);
        }
        let mut seen = [false; 32];
        for a in accepts {
            let s = a.await.expect("accept");
            let (n, buf) = s.recv(Vec::with_capacity(4)).await;
            assert_eq!(n, 1);
            assert!(
                !mem::replace(&mut seen[buf[0] as usize], true),
                "a connection twice"
            );
        }
        assert!(seen.iter().all(|&s| s));
    });
}

#[test]
fn datagrams_roundtrip_with_their_sender() {
    both(|| async {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        for s in [&a, &b] {
            with_driver(|d| d.register_socket(s.as_raw_socket() as SOCKET)).unwrap();
        }

        struct From {
            buf: Vec<u8>,
            addr: Box<windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE>,
            len: Box<i32>,
        }
        let recv_from = |s: &UdpSocket, cap: usize| {
            let mut data = From {
                buf: vec![0u8; cap],
                // SAFETY: plain data; all-zero is valid.
                addr: Box::new(unsafe { mem::zeroed() }),
                len: Box::new(mem::size_of::<
                    windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE,
                >() as i32),
            };
            let io = Io::RecvFrom {
                socket: s.as_raw_socket() as SOCKET,
                buf: data.buf.as_mut_ptr(),
                len: cap as u32,
                addr: (&mut *data.addr as *mut _ as *mut _),
                addr_len: &mut *data.len,
            };
            // SAFETY: buffer, address and length are heap blocks owned by
            // `data`.
            unsafe { Op::new(io, data) }
        };

        let mut pending = recv_from(&b, 1500);
        assert!(poll_once(&mut pending).await.is_none());

        let payload = b"datagram".to_vec();
        let dest = Box::new(SockAddr::from(b_addr));
        let io = Io::SendTo {
            socket: a.as_raw_socket() as SOCKET,
            buf: payload.as_ptr(),
            len: payload.len() as u32,
            addr: dest.as_ptr().cast(),
            addr_len: dest.len(),
        };
        // SAFETY: payload and address are heap blocks owned by the tuple.
        let (n, _) = unsafe { Op::new(io, (payload, dest)) }.await;
        assert_eq!(n, 8, "{}", err(n));

        let (n, data) = pending.await;
        assert_eq!(n, 8, "{}", err(n));
        assert_eq!(&data.buf[..8], b"datagram");
        // SAFETY: the kernel wrote an address of `len` bytes.
        let from = unsafe { SockAddr::new(mem::transmute_copy(&*data.addr), *data.len) };
        assert_eq!(from.as_socket(), Some(a_addr));

        // A datagram longer than the buffer: Windows reports the error
        // and fills the buffer (Linux reports the truncated length).
        a.send_to(&[7u8; 100], b_addr).unwrap();
        let (n, data) = recv_from(&b, 10).await;
        assert_eq!(n, -WSAEMSGSIZE, "{}", err(n));
        assert_eq!(&data.buf[..], &[7u8; 10]);
    });
}

#[test]
fn files_read_and_write_at_offsets() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

    both(|| async {
        let path = std::env::temp_dir().join(format!(
            "giong-iocp-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(&path)
            .expect("open");
        let handle = file.as_raw_handle() as HANDLE;
        // SAFETY: `file` is open.
        with_driver(|d| unsafe { d.register(handle) }).expect("register file");

        let write = |data: Vec<u8>, offset: u64| {
            let io = Io::Write {
                file: handle,
                buf: data.as_ptr(),
                len: data.len() as u32,
                offset,
            };
            // SAFETY: the Vec owns the bytes.
            unsafe { Op::new(io, data) }
        };
        let read = |cap: usize, offset: u64| {
            let mut buf = vec![0u8; cap];
            let io = Io::Read {
                file: handle,
                buf: buf.as_mut_ptr(),
                len: cap as u32,
                offset,
            };
            // SAFETY: the Vec owns the block.
            unsafe { Op::new(io, buf) }
        };

        let (n, _) = write(b"world".to_vec(), 6).await;
        assert_eq!(n, 5, "{}", err(n));
        let (n, _) = write(b"hello ".to_vec(), 0).await;
        assert_eq!(n, 6, "{}", err(n));

        let (n, buf) = read(64, 0).await;
        assert_eq!(n, 11, "{}", err(n));
        assert_eq!(&buf[..11], b"hello world");
        let (n, buf) = read(3, 6).await;
        assert_eq!((n, &buf[..]), (3, &b"wor"[..]));
        let (n, _) = read(16, 11).await;
        assert_eq!(
            n,
            0,
            "reading at the end of a file is 0, not an error: {}",
            err(n)
        );
        let (n, _) = read(16, 4096).await;
        assert_eq!(n, 0, "reading past the end is 0: {}", err(n));

        drop(file);
        let _ = std::fs::remove_file(&path);
    });
}

#[test]
fn the_bell_wakes_a_parked_worker_from_another_thread() {
    let mut driver = Driver::new().expect("driver");
    let bell = driver.bell();

    let start = Instant::now();
    let parked = driver.park(Park::Wait(Some(Duration::from_millis(60))));
    assert!(!parked.rung && parked.completions == 0);
    assert!(
        start.elapsed() >= Duration::from_millis(60),
        "park returned early"
    );

    let ringer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        bell.ring();
        bell
    });
    let start = Instant::now();
    let parked = driver.park(Park::Wait(None));
    assert!(parked.rung);
    assert!(start.elapsed() < Duration::from_secs(5));
    let bell = ringer.join().unwrap();

    // A ring before the park is not lost.
    bell.ring();
    assert!(driver.park(Park::Poll).rung);
    assert!(!driver.park(Park::Poll).rung);

    // The bell outlives the driver without harm.
    drop(driver);
    bell.ring();
}

#[test]
fn shutdown_cancels_what_is_in_flight() {
    for inline in [true, false] {
        let driver = Driver::with_inline_completions(inline).unwrap();
        let (pending, mut driver) = block_on(driver, async {
            let (listener, addr) = Listener::bind();
            let client = Sock::connect(addr).await.expect("connect");
            let server = listener.accept().await.expect("accept");
            let mut recv = Box::pin(server.recv(Vec::with_capacity(64)));
            assert!(poll_once(&mut recv).await.is_none());
            let mut accept = Box::pin(listener.accept());
            assert!(poll_once(&mut accept).await.is_none());
            sleep(Duration::from_millis(10)).await;
            // Leak the futures: a worker that stops with tasks alive.
            mem::forget(recv);
            mem::forget(accept);
            (listener, client, server)
        });
        assert_eq!(driver.inflight(), 2);
        assert!(driver.shutdown(Duration::from_secs(5)));
        assert_eq!(driver.inflight(), 0);
        drop(pending);
        let _ = ERROR_OPERATION_ABORTED;
    }
}
