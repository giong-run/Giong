//! Every handle giong opens on Windows is closed again: each owner (the
//! runtime and its completion ports, listeners and their accepts, streams
//! and their connects, receives and sends, shared addresses and the threads
//! that hand them out, datagram sockets, files and the blocking threads
//! that read and write them, the resolver) is used many times over, with
//! operations cut short as well as finished, and the process's handle
//! count must come back to where it was.
//!
//! Unix descriptors are checked by the soak and the leak tests of each
//! module; the test exists on every platform so that the suites list the
//! same names.

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

mod common;

#[test]
fn every_handle_giong_opens_on_windows_is_closed_again() {
    #[cfg(windows)]
    windows::check_every_owner();
}

#[cfg(windows)]
mod windows {
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::time::{Duration, Instant};

    use giong::net::{TcpListener, TcpStream, UdpSocket};
    use giong::time::{sleep, timeout};

    /// How often each owner is used. A handle left behind by every use
    /// shows as this many; one opened once per process (Winsock's helpers)
    /// is taken before the count.
    const ROUNDS: usize = 30;

    pub(super) fn check_every_owner() {
        let dir = std::env::temp_dir().join(format!("giong-handles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        check("a runtime of two workers", runtime);
        check("a stream echoed and closed", echo);
        check("a listener dropped while its accept waits", pending_accept);
        check("a connect cut short", cut_connect);
        check("a stream dropped while its receive waits", pending_recv);
        check("a write cut short", cut_write);
        check("an address shared by two workers", shared_address);
        check("a datagram socket", datagrams);
        check("a datagram address shared by two workers", shared_datagrams);
        let files = dir.clone();
        check("a file written, read and closed", move || file(&files));
        let files = dir.clone();
        check("a file open cut short", move || cut_open(&files));
        let files = dir.clone();
        check("a file sent on a stream", move || send_file(&files));
        check("a name resolved", resolve);
        drop(std::fs::remove_dir_all(&dir));
    }

    /// Runs `round` once to open what a process opens once, then
    /// [`ROUNDS`] times, and checks that the handle count comes back.
    ///
    /// One Event more is looked at again rather than failed at once: the
    /// system opened one in a merge queue run (#391), in a round that opens
    /// no Event of giong's, and 2,400 rounds of four owners on each of
    /// windows-2022 and windows-2025 left none, so it is opened lazily and
    /// rarely, and kept. A handle each round leaves would show as
    /// [`ROUNDS`] of them; so the round runs [`ROUNDS`] times more, and the
    /// count must not grow again.
    fn check(what: &str, mut round: impl FnMut()) {
        round();
        let before = settled(usize::MAX);
        let types_before = handle_types();
        for _ in 0..ROUNDS {
            round();
        }
        let after = settled(before);
        eprintln!("{what}: {before} handles before {ROUNDS} rounds, {after} after");
        if after <= before {
            return;
        }
        let new = new_handles(&types_before);
        let one_event = after == before + 1 && new.len() == 1 && new[0].1 == "Event";
        assert!(
            one_event,
            "{what}: {after} handles open after {ROUNDS} rounds, {before} before; new: {}",
            listed(&new)
        );
        for _ in 0..ROUNDS {
            round();
        }
        let again = settled(after);
        eprintln!(
            "{what}: one Event more ({}), then {again} handles after {ROUNDS} more rounds",
            listed(&new)
        );
        assert!(
            again <= after,
            "{what}: {again} handles open after {} rounds, {before} before; new: {}",
            2 * ROUNDS,
            listed(&new_handles(&types_before))
        );
    }

    /// The handle count once it is at most `ceiling`, or once it has not
    /// gone down for 15 seconds: the blocking threads stop after 10 idle
    /// seconds, and the event each keeps for its transfers goes with it.
    fn settled(ceiling: usize) -> usize {
        let start = Instant::now();
        let mut low = handle_count();
        let mut since = Instant::now();
        loop {
            if low <= ceiling && ceiling != usize::MAX {
                return low;
            }
            if since.elapsed() > Duration::from_secs(15)
                || start.elapsed() > Duration::from_secs(60)
            {
                return low;
            }
            std::thread::sleep(Duration::from_millis(100));
            let now = handle_count();
            if now < low {
                low = now;
                since = Instant::now();
            }
        }
    }

    fn handle_count() -> usize {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        let mut count = 0u32;
        // SAFETY: the pseudo handle of this process, and a local to write to.
        let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
        assert!(ok != 0, "GetProcessHandleCount failed");
        count as usize
    }

    /// The object type of each open handle, by value.
    fn handle_types() -> BTreeMap<usize, String> {
        #[repr(C)]
        struct UnicodeString {
            length: u16,
            maximum_length: u16,
            buffer: *const u16,
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn NtQueryObject(
                handle: isize,
                class: u32,
                info: *mut u64,
                len: u32,
                returned: *mut u32,
            ) -> i32;
        }
        let mut buf = vec![0u64; 256];
        let mut types = BTreeMap::new();
        for handle in (4..0x4_0000usize).step_by(4) {
            let mut returned = 0;
            // SAFETY: a buffer of the length given, which the call fills
            // with an OBJECT_TYPE_INFORMATION (class 2); asking for the
            // type never blocks. A value that is no handle fails.
            let status = unsafe {
                NtQueryObject(
                    handle as isize,
                    2,
                    buf.as_mut_ptr(),
                    (buf.len() * 8) as u32,
                    &mut returned,
                )
            };
            if status < 0 {
                continue;
            }
            // SAFETY: on success the buffer starts with the type's name, a
            // UNICODE_STRING whose characters the call wrote into it.
            let name = unsafe {
                let name = &*(buf.as_ptr() as *const UnicodeString);
                let chars = std::slice::from_raw_parts(name.buffer, name.length as usize / 2);
                String::from_utf16_lossy(chars)
            };
            types.insert(handle, name);
        }
        types
    }

    /// The handles open now that were not in `before`, with their types.
    fn new_handles(before: &BTreeMap<usize, String>) -> Vec<(usize, String)> {
        handle_types()
            .into_iter()
            .filter(|(h, t)| before.get(h) != Some(t))
            .collect()
    }

    fn listed(handles: &[(usize, String)]) -> String {
        let all: Vec<String> = handles.iter().map(|(h, t)| format!("{h:#x} {t}")).collect();
        format!("[{}]", all.join(", "))
    }

    fn on_two_workers<F, Fut>(f: F)
    where
        F: Fn(usize) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        giong::Builder::new()
            .workers(2)
            .build()
            .unwrap()
            .run(f)
            .unwrap();
    }

    fn block_on<Fut: std::future::Future<Output = ()>>(fut: Fut) {
        giong::Builder::new().workers(1).block_on(fut).unwrap();
    }

    fn runtime() {
        on_two_workers(|_| async {});
    }

    fn echo() {
        block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client = giong::spawn(async move {
                let stream = TcpStream::connect(addr).await.unwrap();
                let (res, _) = stream.write_all(b"ping".to_vec()).await;
                res.unwrap();
                let (res, buf) = stream.read(Vec::with_capacity(4)).await;
                assert_eq!(res.unwrap(), 4);
                assert_eq!(buf, b"ping");
            });
            let stream = listener.accept().await.unwrap();
            let (res, buf) = stream.read(Vec::with_capacity(4)).await;
            assert_eq!(res.unwrap(), 4);
            let (res, _) = stream.write_all(buf).await;
            res.unwrap();
            client.await.unwrap();
        });
    }

    fn pending_accept() {
        block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            assert!(timeout(Duration::from_millis(5), listener.accept())
                .await
                .is_err());
            // A connection the accept took in the meantime is closed with it.
            let addr = listener.local_addr().unwrap();
            let _ = std::net::TcpStream::connect(addr);
            sleep(Duration::from_millis(2)).await;
        });
    }

    fn cut_connect() {
        block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let _ = timeout(Duration::from_micros(1), TcpStream::connect(addr)).await;
            // An address nobody answers on: the connect is still trying.
            let silent: SocketAddr = "10.255.255.1:9".parse().unwrap();
            let _ = timeout(Duration::from_millis(5), TcpStream::connect(silent)).await;
        });
    }

    fn pending_recv() {
        block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let stream = TcpStream::connect(addr).await.unwrap();
            let peer = listener.accept().await.unwrap();
            assert!(timeout(Duration::from_millis(5), stream.recv())
                .await
                .is_err());
            let read = timeout(Duration::from_millis(5), peer.read(vec![0u8; 16]));
            assert!(read.await.is_err());
        });
    }

    fn cut_write() {
        block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let stream = TcpStream::connect(addr).await.unwrap();
            let _peer = listener.accept().await.unwrap();
            // More than the buffers hold: the write waits until it is cut.
            let big = vec![7u8; 64 << 20];
            let _ = timeout(Duration::from_millis(20), stream.write_all(big)).await;
        });
    }

    fn shared_address() {
        let (_holder, port) = super::common::shared_port();
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
        on_two_workers(move |worker| {
            let ready = ready.clone();
            async move {
                let listener = TcpListener::bind(addr).unwrap();
                ready.wait();
                if worker == 0 {
                    for _ in 0..4 {
                        let stream = TcpStream::connect(addr).await.unwrap();
                        let (res, _) = stream.write_all(b"x".to_vec()).await;
                        res.unwrap();
                    }
                }
                // Each takes what comes for a while, then lets go with
                // connections perhaps still waiting for it.
                let _ = timeout(Duration::from_millis(20), async {
                    loop {
                        let stream = listener.accept().await.unwrap();
                        drop(stream);
                    }
                })
                .await;
            }
        });
    }

    fn datagrams() {
        block_on(async {
            let a = UdpSocket::bind("127.0.0.1:0").unwrap();
            let b = UdpSocket::bind("127.0.0.1:0").unwrap();
            let (res, _) = a.send_to(b"hi".to_vec(), b.local_addr().unwrap()).await;
            res.unwrap();
            let (datagram, _) = b.recv_from().await.unwrap();
            assert_eq!(datagram.as_slice(), b"hi");
            // A receive that waits, cut short.
            assert!(timeout(Duration::from_millis(5), a.recv_from())
                .await
                .is_err());
        });
    }

    fn shared_datagrams() {
        let (_holder, port) = super::common::shared_port();
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
        on_two_workers(move |worker| {
            let ready = ready.clone();
            async move {
                let socket = UdpSocket::bind(addr).unwrap();
                ready.wait();
                if worker == 0 {
                    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
                    for _ in 0..4 {
                        let (res, _) = sender.send_to(b"x".to_vec(), addr).await;
                        res.unwrap();
                    }
                }
                let _ = timeout(Duration::from_millis(20), async {
                    loop {
                        socket.recv_from().await.unwrap();
                    }
                })
                .await;
            }
        });
    }

    fn file(dir: &std::path::Path) {
        let path = dir.join("file");
        block_on(async move {
            let file = giong::fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .await
                .unwrap();
            let (res, _) = file.write_all_at(vec![1u8; 10_000], 0).await;
            res.unwrap();
            let (res, _) = file.read_at(Vec::with_capacity(100), 5).await;
            assert_eq!(res.unwrap(), 100);
            file.sync_all().await.unwrap();
            file.allocate(0, 20_000).await.unwrap();
            assert_eq!(file.metadata().await.unwrap().len(), 20_000);
            file.close().await.unwrap();
            // Dropped instead of closed.
            let file = giong::fs::File::open(&path).await.unwrap();
            drop(file);
            assert_eq!(giong::fs::read(&path).await.unwrap().len(), 20_000);
            giong::fs::remove_file(&path).await.unwrap();
        });
    }

    fn cut_open(dir: &std::path::Path) {
        let path = dir.join("cut");
        block_on(async move {
            let _ = timeout(Duration::ZERO, giong::fs::File::create(&path)).await;
            // The open went on without its future: let it finish.
            sleep(Duration::from_millis(20)).await;
            drop(std::fs::remove_file(&path));
        });
    }

    fn send_file(dir: &std::path::Path) {
        let path = dir.join("sent");
        block_on(async move {
            giong::fs::write(&path, vec![3u8; 100_000]).await.unwrap();
            let file = giong::fs::File::open(&path).await.unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let stream = TcpStream::connect(addr).await.unwrap();
            let peer = listener.accept().await.unwrap();
            let reader = giong::spawn(async move {
                let mut got = 0;
                while got < 100_000 {
                    got += peer.recv().await.unwrap().len();
                }
            });
            stream.send_file(&file, 0, 100_000).await.unwrap();
            reader.await.unwrap();
            drop(file);
            giong::fs::remove_file(&path).await.unwrap();
        });
    }

    fn resolve() {
        block_on(async {
            let addrs = giong::net::lookup_host("localhost:80").await.unwrap();
            assert!(!addrs.is_empty());
        });
    }
}
