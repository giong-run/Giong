//! Soak test: hours of churn, then a check that nothing piled up.
//!
//! Every worker runs a TCP echo server and clients that connect to all of
//! them, plus UDP, file and cross-worker traffic. Clients mix clean closes,
//! aborts with unread data, half-closes and receives cancelled by a timeout.
//! A thread outside the runtime samples the process's open descriptors and
//! resident memory once a second.
//!
//! At the end it checks that:
//! * the descriptor count did not grow during the run, and is back to where
//!   it started once the runtime is dropped;
//! * resident memory levelled off after the warm-up;
//! * every worker's driver is back to idle: no tracked operations, nothing in
//!   flight, every receive buffer returned, no stray timers or tasks.
//!
//! It runs for `GIONG_SOAK_SECS` seconds (default 60), so it is not part of
//! the default run:
//!
//! ```text
//! GIONG_SOAK_SECS=3600 cargo test --release --test soak -- --ignored --nocapture
//! ```

use std::cell::Cell;
use std::collections::BTreeMap;
use std::net::{Shutdown, SocketAddr};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream, UdpSocket};
use giong::time::{sleep, timeout};

mod common;

/// Client tasks per worker.
const CLIENTS: usize = 32;
/// The least time between two connects of one client on Windows (see
/// `client`): with 4 workers, about 85 connects a second.
#[cfg(windows)]
const WINDOWS_CONNECT_GAP: Duration = Duration::from_millis(1500);

/// Client tasks per worker: [`CLIENTS`], or `GIONG_SOAK_CLIENTS` for a
/// lighter run.
fn clients() -> usize {
    std::env::var("GIONG_SOAK_CLIENTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(CLIENTS)
}
const RECV_BUFFERS: usize = 512;

#[derive(Default)]
struct Counters {
    connections: AtomicU64,
    round_trips: AtomicU64,
    cancelled_recvs: AtomicU64,
    datagrams: AtomicU64,
    files: AtomicU64,
    cross_worker: AtomicU64,
    /// Messages the server got in more than one receive.
    split: AtomicU64,
    /// Connects answered "connection refused" although every listener was
    /// up. Linux does this on its own, rarely: a client that aborts a
    /// connection the server already closed frees its port at once, and when
    /// the next connect from that port reaches the server before the abort
    /// does, the old connection's remains answer it with a reset. Plain
    /// `std::net` sockets get the same (the pull request that added this
    /// counter has the program). Counted and held to a small share, since a
    /// listener that went away would refuse thousands.
    connects_refused: AtomicU64,
    /// Connects that found no free local port (Windows, where the clients
    /// are paced so that this does not happen: must stay 0).
    ports_used_up: AtomicU64,
    /// Errors that none of the scenarios should produce.
    unexpected: AtomicU64,
}

impl Counters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn unexpected(&self, what: &str, err: impl std::fmt::Display) {
        if self.unexpected.fetch_add(1, Ordering::Relaxed) < 20 {
            eprintln!("soak: unexpected error in {what}: {err}");
        }
    }
}

/// xorshift64: each task has its own, seeded from its identity.
struct Rng(Cell<u64>);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(Cell::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1))
    }

    fn below(&self, n: u64) -> u64 {
        let mut x = self.0.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0.set(x);
        x % n
    }
}

/// What a client asks the server to do with a message, in its first byte.
const ECHO: u8 = b'e';
/// Wait before echoing, long enough for the client's timeout to fire.
const STALL: u8 = b's';
/// Close the connection without reading the rest.
const HANG_UP: u8 = b'h';

/// A message is one of the requests above, its whole length (two bytes,
/// big-endian), a seed, then bytes that follow from the seed. TCP keeps no
/// message boundaries: a receive can end inside a message (on Linux 6.12 and
/// newer, wherever a shared receive buffer fills up) or hold more than one.
/// So the server reads messages by their length, and checks every byte.
const HEADER: usize = 4;

fn message(request: u8, len: usize, seed: u8) -> Vec<u8> {
    let mut msg: Vec<u8> = (0..len).map(|i| filler(seed, i)).collect();
    msg[0] = request;
    msg[1..3].copy_from_slice(&(len as u16).to_be_bytes());
    msg[3] = seed;
    msg
}

fn filler(seed: u8, at: usize) -> u8 {
    seed.wrapping_add((at % 251) as u8)
}

/// The length a message's header gives; `msg` holds at least the header.
fn message_len(msg: &[u8]) -> usize {
    u16::from_be_bytes([msg[1], msg[2]]).into()
}

/// Whether `msg` is a whole message with every byte where it belongs.
fn intact(msg: &[u8]) -> bool {
    msg.len() >= HEADER
        && message_len(msg) == msg.len()
        && (HEADER..msg.len()).all(|at| msg[at] == filler(msg[3], at))
}

async fn serve(stream: TcpStream, deadline: Instant, counters: Arc<Counters>) {
    let mut pending = Vec::new();
    loop {
        // Read until a whole message is in; a hang-up needs its first byte
        // only.
        let mut receives = 0;
        while pending.first() != Some(&HANG_UP)
            && (pending.len() < HEADER || pending.len() < message_len(&pending))
        {
            let left = deadline.saturating_duration_since(Instant::now()) + Duration::from_secs(1);
            receives += 1;
            match timeout(left, stream.recv()).await {
                Ok(Ok(data)) if data.is_empty() => return,
                Ok(Ok(data)) => pending.extend_from_slice(&data),
                // Clients abort connections on purpose.
                Ok(Err(_)) | Err(_) => return,
            }
        }
        match pending[0] {
            HANG_UP => return,
            STALL => sleep(Duration::from_millis(20)).await,
            ECHO => {}
            other => {
                counters.unexpected("server", format!("unknown request {other}"));
                return;
            }
        }
        if receives > 1 {
            Counters::bump(&counters.split);
        }
        let msg: Vec<u8> = pending.drain(..message_len(&pending)).collect();
        if !intact(&msg) {
            counters.unexpected("server", "a message arrived with bytes out of place");
            return;
        }
        if stream.write_all(msg).await.0.is_err() {
            return;
        }
    }
}

/// Receives exactly `len` bytes.
async fn recv_exact(stream: &TcpStream, len: usize) -> std::io::Result<Vec<u8>> {
    let mut got = Vec::with_capacity(len);
    while got.len() < len {
        let data = stream.recv().await?;
        if data.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        got.extend_from_slice(&data);
    }
    Ok(got)
}

/// Waits for `fut`, but not forever: a server that never answers or never
/// closes is a bug the soak test must report, not hang on.
async fn within<T>(
    counters: &Counters,
    what: &str,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    match timeout(Duration::from_secs(10), fut).await {
        Ok(value) => Some(value),
        Err(err) => {
            counters.unexpected(what, err);
            None
        }
    }
}

async fn client(addr: SocketAddr, deadline: Instant, rng: Rng, counters: Arc<Counters>) {
    #[cfg(windows)]
    let mut next_connect = Instant::now();
    while Instant::now() < deadline {
        // Windows holds a closed connection's local port for TIME_WAIT and
        // has about 16,000 of them: at full speed the clients use them all
        // up, and connects that reuse a port then collide with connections
        // still in TIME_WAIT. Each client connects at most once every
        // WINDOWS_CONNECT_GAP, well under what the ports allow.
        #[cfg(windows)]
        {
            let now = Instant::now();
            if now < next_connect {
                sleep(next_connect - now).await;
            }
            next_connect = Instant::now() + WINDOWS_CONNECT_GAP;
        }
        let stream = match TcpStream::connect(addr).await {
            Ok(stream) => stream,
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                Counters::bump(&counters.connects_refused);
                continue;
            }
            Err(err) if cfg!(windows) && err.kind() == std::io::ErrorKind::AddrInUse => {
                Counters::bump(&counters.ports_used_up);
                sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(err) => {
                counters.unexpected("connect", err);
                sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        Counters::bump(&counters.connections);
        let len = HEADER + rng.below(3000) as usize;
        let mut msg = message(ECHO, len, rng.below(256) as u8);
        match rng.below(4) {
            // Some round trips, then a clean close.
            0 | 1 => {
                for _ in 0..1 + rng.below(8) {
                    msg[0] = ECHO;
                    let (res, back) = stream.write_all(msg).await;
                    msg = back;
                    if let Err(err) = res {
                        counters.unexpected("write", err);
                        break;
                    }
                    match within(&counters, "echo", recv_exact(&stream, len)).await {
                        Some(Ok(reply)) if reply == msg => Counters::bump(&counters.round_trips),
                        Some(Ok(_)) => counters.unexpected("echo", "reply differs"),
                        Some(Err(err)) => {
                            counters.unexpected("echo", err);
                            break;
                        }
                        None => break,
                    }
                }
            }
            // Ask for a slow reply and give up waiting: a cancelled receive.
            2 => {
                msg[0] = STALL;
                let _ = stream.write_all(msg).await;
                if timeout(Duration::from_millis(1), stream.recv())
                    .await
                    .is_err()
                {
                    Counters::bump(&counters.cancelled_recvs);
                }
                // Dropped with the reply still on its way.
            }
            // Half-close, or have the server hang up on unread data.
            _ => {
                if rng.below(2) == 0 {
                    msg[0] = ECHO;
                    let _ = stream.write_all(msg).await;
                    let _ = stream.shutdown(Shutdown::Write);
                    // Read the echo and then the end of the stream.
                    let _ = within(&counters, "half-close", async {
                        let _ = recv_exact(&stream, len).await;
                        let _ = stream.recv().await;
                    })
                    .await;
                } else {
                    msg[0] = HANG_UP;
                    let _ = stream.write_all(msg).await;
                    let _ = within(&counters, "hang-up", stream.recv()).await;
                }
            }
        }
    }
}

async fn udp(
    peers: Vec<SocketAddr>,
    socket: UdpSocket,
    deadline: Instant,
    counters: Arc<Counters>,
) {
    let rng = Rng::new(socket.local_addr().unwrap().port().into());
    let socket = Rc::new(socket);
    // Answer whatever arrives; stop shortly after the deadline.
    let echo = giong::spawn({
        let (socket, counters) = (socket.clone(), counters.clone());
        async move {
            while let Ok(Ok((dgram, from))) = timeout(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(500),
                socket.recv_from(),
            )
            .await
            {
                if dgram.as_slice().first() == Some(&b'?') {
                    let mut reply = dgram.as_slice().to_vec();
                    drop(dgram);
                    reply[0] = b'!';
                    if let (Err(err), _) = socket.send_to(reply, from).await {
                        counters.unexpected("udp send", err);
                    }
                }
                Counters::bump(&counters.datagrams);
            }
        }
    });
    while Instant::now() < deadline {
        let to = peers[rng.below(peers.len() as u64) as usize];
        let len = 1 + rng.below(1200) as usize;
        let mut msg = vec![b'u'; len];
        msg[0] = b'?';
        if let (Err(err), _) = socket.send_to(msg, to).await {
            counters.unexpected("udp send", err);
        }
        sleep(Duration::from_micros(200)).await;
    }
    let _ = echo.await;
}

async fn files(dir: std::path::PathBuf, deadline: Instant, counters: Arc<Counters>) {
    let rng = Rng::new(giong::worker_id() as u64 + 77);
    let mut n = 0u64;
    while Instant::now() < deadline {
        n += 1;
        let path = dir.join(format!("w{}-{n}", giong::worker_id()));
        let result = async {
            let file = giong::fs::File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .await?;
            let len = 1 + rng.below(64 * 1024) as usize;
            let (res, _) = file.write_all_at(vec![7u8; len], 0).await;
            res?;
            let (res, buf) = file.read_at(vec![0u8; len], 0).await;
            assert_eq!(res?, len);
            assert!(buf.iter().all(|&b| b == 7));
            assert_eq!(file.metadata().await?.len(), len as u64);
            if rng.below(2) == 0 {
                file.close().await?;
            } // else: closed on drop
            giong::fs::remove_file(&path).await
        }
        .await;
        match result {
            Ok(()) => Counters::bump(&counters.files),
            Err(err) => counters.unexpected("files", err),
        }
    }
}

/// Bounces a message around the workers through `spawn_on` and channels.
async fn cross_worker(workers: usize, deadline: Instant, counters: Arc<Counters>) {
    let handle = giong::handle();
    while Instant::now() < deadline {
        let target = (giong::worker_id() + 1) % workers;
        let (tx, mut rx) = giong::sync::mpsc::unbounded::<usize>();
        let task = handle.spawn_on(target, move || async move {
            let _ = tx.send(giong::worker_id());
            giong::worker_id()
        });
        match timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(id)) if id == target => {}
            other => counters.unexpected("cross worker", format!("{other:?}")),
        }
        match timeout(Duration::from_secs(5), task).await {
            Ok(Ok(id)) if id == target => Counters::bump(&counters.cross_worker),
            other => counters.unexpected("spawn_on", format!("{other:?}")),
        }
        sleep(Duration::from_micros(100)).await;
    }
}

#[cfg(unix)]
fn open_fds() -> usize {
    // macOS lists a process's own descriptors in /dev/fd.
    let dir = if cfg!(target_os = "macos") {
        "/dev/fd"
    } else {
        "/proc/self/fd"
    };
    std::fs::read_dir(dir).unwrap().count()
}

/// The process's open handles: sockets, files, events, threads and the rest.
#[cfg(windows)]
fn open_fds() -> usize {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut count = 0u32;
    // SAFETY: the pseudo handle of this process, and a local to write to.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    assert!(ok != 0, "GetProcessHandleCount failed");
    count as usize
}

#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Resident memory from the kernel's task information (`proc_pidinfo`).
#[cfg(target_os = "macos")]
fn rss_kib() -> u64 {
    // SAFETY: all-zero is a valid `proc_taskinfo`, which the call fills in;
    // the size passed is its size.
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        );
        assert_eq!(n, size, "proc_pidinfo: {}", std::io::Error::last_os_error());
        info.pti_resident_size / 1024
    }
}

/// The process's working set, Windows' resident memory.
#[cfg(windows)]
fn rss_kib() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: all-zero is a valid value of this plain structure.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the pseudo handle of this process, and a structure of the size
    // given.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    assert!(ok != 0, "GetProcessMemoryInfo failed");
    counters.WorkingSetSize as u64 / 1024
}

/// One short life of a runtime, using what the soak uses: two workers
/// share a listening address and take connections that carry a message
/// each, a datagram goes round, a file is written, synced and removed.
fn runtime_life() {
    const CONNECTIONS: usize = 20;
    let (holder, port) = common::shared_port();
    let shared: SocketAddr = ([127, 0, 0, 1], port).into();
    let dir = std::env::temp_dir().join(format!("giong-soak-life-{}", std::process::id()));
    giong::Builder::new()
        .workers(2)
        .build()
        .unwrap()
        .run(move |worker| {
            let path = dir.with_extension(format!("{worker}.tmp"));
            async move {
                let listener = TcpListener::bind(shared).unwrap();
                // Let both listeners come up, so that the address is shared.
                sleep(Duration::from_millis(100)).await;
                let accept = giong::spawn(async move {
                    while let Ok(Ok(stream)) =
                        timeout(Duration::from_millis(500), listener.accept()).await
                    {
                        giong::spawn(async move {
                            while let Ok(data) = stream.recv().await {
                                if data.is_empty()
                                    || stream.write_all(data.to_vec()).await.0.is_err()
                                {
                                    return;
                                }
                            }
                        });
                    }
                });
                for _ in 0..CONNECTIONS {
                    let stream = TcpStream::connect(shared).await.unwrap();
                    stream.write_all(vec![ECHO; 8]).await.0.unwrap();
                    let mut back = 0;
                    while back < 8 {
                        let data = stream.recv().await.unwrap();
                        assert!(!data.is_empty(), "the echo server hung up");
                        back += data.len();
                    }
                }
                accept.await.unwrap();

                let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
                udp.send_to(vec![1u8], udp.local_addr().unwrap())
                    .await
                    .0
                    .unwrap();
                drop(udp.recv_from().await.unwrap());

                let file = giong::fs::File::create(&path).await.unwrap();
                file.write_all_at(vec![0u8; 16], 0).await.0.unwrap();
                file.sync_all().await.unwrap();
                drop(file);
                giong::fs::remove_file(&path).await.unwrap();
            }
        })
        .unwrap();
    drop(holder);
}

/// What each open descriptor is, by number: where its link in `/proc` or
/// `/dev/fd` points. Only for the message of a failed check.
#[cfg(unix)]
fn open_handles() -> BTreeMap<usize, String> {
    let dir = if cfg!(target_os = "macos") {
        "/dev/fd"
    } else {
        "/proc/self/fd"
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let fd = entry.file_name().to_str()?.parse().ok()?;
            let target = std::fs::read_link(entry.path()).ok()?;
            Some((fd, target.display().to_string()))
        })
        .collect()
}

/// What each open handle is, by value: its object type (`File` for a file
/// or socket, `Event`, `Thread`, `Key`, ...). Only for the message of a
/// failed check. Handle values are multiples of 4; the ones in use stay
/// well below the bound searched.
#[cfg(windows)]
fn open_handles() -> BTreeMap<usize, String> {
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
    const OBJECT_TYPE_INFORMATION: u32 = 2;
    let mut buf = vec![0u64; 256];
    let mut handles = BTreeMap::new();
    for handle in (4..0x4_0000usize).step_by(4) {
        let mut returned = 0;
        // SAFETY: a buffer of the length given, which the call fills with an
        // OBJECT_TYPE_INFORMATION; asking for the type never blocks, unlike
        // asking for the name. A value that is no handle fails.
        let status = unsafe {
            NtQueryObject(
                handle as isize,
                OBJECT_TYPE_INFORMATION,
                buf.as_mut_ptr(),
                (buf.len() * 8) as u32,
                &mut returned,
            )
        };
        if status < 0 {
            continue;
        }
        // SAFETY: on success the buffer starts with the type's name, a
        // UNICODE_STRING whose characters the call wrote into the buffer.
        let name = unsafe {
            let name = &*(buf.as_ptr() as *const UnicodeString);
            let chars = std::slice::from_raw_parts(name.buffer, name.length as usize / 2);
            String::from_utf16_lossy(chars)
        };
        // A file handle says what it is: a disk file, a pipe or socket
        // (sockets are pipes to GetFileType), a character device.
        let name = if name == "File" {
            use windows_sys::Win32::Storage::FileSystem::GetFileType;
            // SAFETY: a handle the query above found open; asking for its
            // kind does not block.
            let kind = unsafe { GetFileType(handle as _) };
            let kind = match kind {
                1 => "disk",
                2 => "character device",
                3 => "pipe or socket",
                _ => "unknown",
            };
            format!("File ({kind})")
        } else if name == "Event" {
            format!("Event ({})", event_kind(handle))
        } else {
            name
        };
        handles.insert(handle, name);
    }
    handles
}

/// Whether the event behind `handle` resets itself when a wait ends
/// (`auto-reset`, a synchronization event) or stays set until it is reset
/// (`manual-reset`, a notification event). giong makes only manual-reset
/// events: the one each thread keeps for its file transfers.
#[cfg(windows)]
fn event_kind(handle: usize) -> &'static str {
    #[link(name = "ntdll")]
    extern "system" {
        fn NtQueryEvent(
            handle: isize,
            class: u32,
            info: *mut [u32; 2],
            len: u32,
            returned: *mut u32,
        ) -> i32;
    }
    let mut info = [0u32; 2];
    let mut returned = 0;
    // SAFETY: EVENT_BASIC_INFORMATION (class 0) is two 32-bit fields, the
    // event's type and its state, written into a local of that size.
    let status = unsafe { NtQueryEvent(handle as isize, 0, &mut info, 8, &mut returned) };
    match (status >= 0, info[0]) {
        (true, 0) => "manual-reset",
        (true, 1) => "auto-reset",
        _ => "unknown kind",
    }
}

/// The descriptors or handles open now that were not in `before`, and on
/// Windows the threads alive now: a handle a thread holds for itself (an
/// event Winsock or giong keeps per thread) goes with its thread.
fn new_handles(before: &BTreeMap<usize, String>) -> String {
    let now = open_handles();
    let new: Vec<String> = now
        .iter()
        .filter(|(n, what)| before.get(n) != Some(what))
        .map(|(n, what)| format!("{n:#x} {what}"))
        .collect();
    #[cfg(unix)]
    let threads = String::new();
    #[cfg(windows)]
    let threads = format!("; threads alive: {}", live_threads().join(", "));
    format!("[{}]{threads}", new.join(", "))
}

/// The threads of this process, by id and name (unnamed: `-`).
#[cfg(windows)]
fn live_threads() -> Vec<String> {
    #[repr(C)]
    struct ThreadEntry {
        size: u32,
        usage: u32,
        thread: u32,
        owner: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process: u32) -> isize;
        fn Thread32First(snapshot: isize, entry: *mut ThreadEntry) -> i32;
        fn Thread32Next(snapshot: isize, entry: *mut ThreadEntry) -> i32;
        fn OpenThread(access: u32, inherit: i32, thread: u32) -> isize;
        fn GetThreadDescription(thread: isize, description: *mut *mut u16) -> i32;
        fn LocalFree(memory: *mut u16) -> *mut u16;
        fn CloseHandle(handle: isize) -> i32;
    }
    const TH32CS_SNAPTHREAD: u32 = 4;
    const THREAD_QUERY_LIMITED_INFORMATION: u32 = 0x800;
    let pid = std::process::id();
    let mut threads = Vec::new();
    // SAFETY: plain calls with an entry of the size it says; the snapshot,
    // each thread handle and each description are freed once read.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == -1 {
            return threads;
        }
        let mut entry = std::mem::zeroed::<ThreadEntry>();
        entry.size = std::mem::size_of::<ThreadEntry>() as u32;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.owner == pid {
                let mut name = String::from("-");
                let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.thread);
                if thread != 0 {
                    let mut text = std::ptr::null_mut();
                    if GetThreadDescription(thread, &mut text) >= 0 && !text.is_null() {
                        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
                        let read = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
                        if !read.is_empty() {
                            name = read;
                        }
                        LocalFree(text);
                    }
                    CloseHandle(thread);
                }
                threads.push(format!("{} {name}", entry.thread));
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    threads
}

/// The descriptor count once it is back to `before` or below, or after 15
/// seconds. On Linux and macOS it is back at once. Windows' handle count
/// also holds what threads that are still ending use, such as the blocking
/// pool's, which stop after 10 idle seconds: those come down, a leak stays.
fn settled_fds(before: usize) -> usize {
    let give_up = Instant::now() + Duration::from_secs(15);
    loop {
        let now = open_fds();
        if now <= before || Instant::now() > give_up {
            return now;
        }
        eprintln!("soak: {now} descriptors open, {before} before the soak; waiting");
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Waits until the descriptor count has not gone down for 12 seconds (at
/// most a minute): the blocking threads stop after 10 idle seconds.
#[cfg(windows)]
fn quiet_fds() {
    let start = Instant::now();
    let (mut low, mut since) = (open_fds(), Instant::now());
    while since.elapsed() < Duration::from_secs(12) && start.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(200));
        let now = open_fds();
        if now < low {
            (low, since) = (now, Instant::now());
        }
    }
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[test]
#[ignore = "runs for GIONG_SOAK_SECS (default 60); run with --ignored"]
fn soak() {
    let secs: u64 = std::env::var("GIONG_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
    // On Windows, a short soak first. Windows opens handles once per process
    // for some of what the soak does (Winsock's helpers, the thread that
    // hands round a shared address's connections), and they stay open: the
    // count before the soak is taken once they are
    // (`the_descriptor_count_is_flat_over_runtime_lives` checks that they
    // are opened once, not per runtime).
    #[cfg(windows)]
    run(workers, 5);
    // Once the warm-up's blocking threads have stopped, with the event each
    // keeps for its transfers: counted while they live, those would hide as
    // many handles left behind by the soak.
    #[cfg(windows)]
    quiet_fds();
    let fds_before = open_fds();
    let handles_before = open_handles();
    #[cfg(windows)]
    eprintln!("soak: threads before: {}", live_threads().join(", "));
    #[cfg(windows)]
    let origins = origins::watch();
    let (counters, samples, idle) = run(workers, secs);

    let c = &counters;

    // 1. Every scenario behaved as designed.
    assert_eq!(c.unexpected.load(Ordering::Relaxed), 0, "unexpected errors");
    // Refused connects are the kernel's (see `Counters::connects_refused`):
    // one in 317 million was measured, so one in 50 million leaves room and
    // still fails when a listener stops answering.
    let (refused, connections) = (
        c.connects_refused.load(Ordering::Relaxed),
        c.connections.load(Ordering::Relaxed),
    );
    assert!(
        refused <= 1 + connections / 50_000_000,
        "{refused} of {connections} connects were refused while every listener was up"
    );
    assert_eq!(
        c.ports_used_up.load(Ordering::Relaxed),
        0,
        "connects found no free local port: the clients' pace is too fast"
    );
    assert!(c.connections.load(Ordering::Relaxed) > 0);
    assert!(c.cancelled_recvs.load(Ordering::Relaxed) > 0);

    // 2. Every worker's driver went back to idle.
    for (worker, stats, quiet) in &idle {
        assert!(quiet, "worker {worker} did not go idle: {stats}");
    }

    // 3. Descriptors: nothing left once the runtime is gone, and no growth
    //    during the run beyond the connections that happen to be open.
    // The watcher stops first: its thread holds handles of its own.
    #[cfg(windows)]
    let traced = origins.map(origins::Watcher::finish);
    let fds_after = settled_fds(fds_before);
    #[allow(unused_mut)]
    let mut new = new_handles(&handles_before);
    // Winsock makes a few events for itself the first time some paths run
    // (one in the WSASocketW that sets up a protocol, one in a closesocket
    // that has to wait), on whichever thread gets there first, and keeps
    // them for the life of the process. Soaks of one to two and a half
    // hours each ended with one of them and nothing else, whatever their
    // length and connection count: made once, not per socket. Up to two
    // events whose traced open came from inside ws2_32 or mswsock are
    // allowed; an event giong made, one opened where the trace cannot
    // tell, or any other handle left behind still fails here.
    #[cfg(windows)]
    let system = match traced {
        Some(origins) => {
            let events: Vec<usize> = open_handles()
                .into_iter()
                .filter(|(n, what)| {
                    what.starts_with("Event") && handles_before.get(n) != Some(what)
                })
                .map(|(n, _)| n)
                .collect();
            let (report, winsock) = origins.report(&events);
            new += &report;
            winsock.min(2)
        }
        None => 0,
    };
    #[cfg(unix)]
    let system = 0;
    assert!(
        fds_after <= fds_before + system,
        "descriptors leaked: {fds_after} open, {fds_before} before the soak; new since: {new}"
    );
    if samples.len() >= 8 {
        let half = samples.len() / 2;
        let first = samples[1..half].iter().map(|s| s.0).max().unwrap();
        let second = samples[half..samples.len() - 1]
            .iter()
            .map(|s| s.0)
            .max()
            .unwrap();
        // Each open connection holds two descriptors, client and server.
        let slack = (2 * workers * clients() + 16) as u64;
        assert!(
            second <= first + slack,
            "descriptor count grew: at most {first} in the first half, {second} in the second"
        );

        // 4. Memory levels off after the first quarter: compare the medians
        //    of the second and the last quarter.
        let quarter = samples.len() / 4;
        let mut q2: Vec<u64> = samples[quarter..2 * quarter].iter().map(|s| s.1).collect();
        let mut q4: Vec<u64> = samples[3 * quarter..].iter().map(|s| s.1).collect();
        let (q2, q4) = (median(&mut q2), median(&mut q4));
        assert!(
            q4 <= q2 + q2 / 10 + 8 * 1024,
            "resident memory kept growing: median {q2} KiB in the second quarter, {q4} KiB in the last"
        );
    }
}

/// What [`soak`] checks after a run: the counters, the descriptor count and
/// resident memory sampled once a second, and how each worker's driver
/// ended.
type Run = (Arc<Counters>, Vec<(u64, u64)>, Vec<(usize, String, bool)>);

/// The soak's traffic on `workers` workers for `secs` seconds.
fn run(workers: usize, secs: u64) -> Run {
    // A free port for every worker's listener (SO_REUSEPORT), and one UDP
    // socket per worker, bound up front so that every worker knows the others.
    let (port_holder, port) = common::shared_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let udp_sockets: Vec<_> = (0..workers)
        .map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
        .collect();
    let udp_peers: Vec<SocketAddr> = udp_sockets
        .iter()
        .map(|s| s.local_addr().unwrap())
        .collect();
    let udp_sockets = Arc::new(Mutex::new(
        udp_sockets.into_iter().map(Some).collect::<Vec<_>>(),
    ));
    let dir = std::env::temp_dir().join(format!("giong-soak-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let counters = Arc::new(Counters::default());
    let samples = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (samples, stop, counters) = (samples.clone(), stop.clone(), counters.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let (fds, rss) = (open_fds() as u64, rss_kib());
                samples.lock().unwrap().push((fds, rss));
                let t = start.elapsed().as_secs();
                if t % 10 == 0 {
                    eprintln!(
                        "soak: {t:>5}s  fds {fds:>4}  rss {:>6} KiB  conns {:>9}  echoes {:>9}  \
                         cancelled {:>8}  split {:>8}  datagrams {:>9}  files {:>7}  hops {:>8}",
                        rss,
                        counters.connections.load(Ordering::Relaxed),
                        counters.round_trips.load(Ordering::Relaxed),
                        counters.cancelled_recvs.load(Ordering::Relaxed),
                        counters.split.load(Ordering::Relaxed),
                        counters.datagrams.load(Ordering::Relaxed),
                        counters.files.load(Ordering::Relaxed),
                        counters.cross_worker.load(Ordering::Relaxed),
                    );
                }
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let idle = Arc::new(Mutex::new(Vec::new()));
    {
        let (counters, idle, dir) = (counters.clone(), idle.clone(), dir.clone());
        giong::Builder::new()
            .workers(workers)
            .recv_buffers(RECV_BUFFERS, 4096)
            .build()
            .unwrap()
            .run(move |worker| {
                let (counters, idle, dir) = (counters.clone(), idle.clone(), dir.clone());
                let udp_socket = udp_sockets.lock().unwrap()[worker].take().unwrap();
                let udp_peers = udp_peers.clone();
                async move {
                    let listener = TcpListener::bind(addr).expect("bind");
                    let acceptor = giong::spawn({
                        let counters = counters.clone();
                        async move {
                            loop {
                                // Clients on other workers may still be
                                // connecting right after the deadline.
                                let left = deadline.saturating_duration_since(Instant::now())
                                    + Duration::from_secs(2);
                                match timeout(left, listener.accept()).await {
                                    Ok(Ok(stream)) => {
                                        giong::spawn(serve(stream, deadline, counters.clone()));
                                    }
                                    Ok(Err(err)) => counters.unexpected("accept", err),
                                    Err(_) => return,
                                }
                            }
                        }
                    });
                    // Let every worker's listener come up first.
                    sleep(Duration::from_millis(100)).await;

                    let mut tasks = Vec::new();
                    let clients = clients();
                    for i in 0..clients {
                        let rng = Rng::new((worker * clients + i) as u64 + 1);
                        tasks.push(giong::spawn(client(addr, deadline, rng, counters.clone())));
                    }
                    let udp_socket = UdpSocket::from_std(udp_socket);
                    tasks.push(giong::spawn(udp(
                        udp_peers,
                        udp_socket,
                        deadline,
                        counters.clone(),
                    )));
                    tasks.push(giong::spawn(files(dir, deadline, counters.clone())));
                    tasks.push(giong::spawn(cross_worker(
                        workers,
                        deadline,
                        counters.clone(),
                    )));
                    for task in tasks {
                        task.await.expect("task");
                    }
                    acceptor.await.expect("acceptor");

                    // Server tasks see their clients go away; give the last
                    // closes and cancellations a moment to complete.
                    let settle = Instant::now() + Duration::from_secs(5);
                    loop {
                        sleep(Duration::from_millis(20)).await;
                        let stats = giong::observe::worker_stats();
                        // The main future is not counted as a task; the one
                        // operation left in flight is the eventfd read that
                        // cross-thread wake-ups complete.
                        let quiet = stats.tasks == 0
                            && stats.tracked_ops == 0
                            && stats.inflight <= 1
                            && stats.free_recv_buffers == stats.recv_buffers;
                        if quiet || Instant::now() > settle {
                            idle.lock()
                                .unwrap()
                                .push((worker, format!("{stats:?}"), quiet));
                            break;
                        }
                    }
                }
            })
            .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    drop(port_holder);

    let c = &counters;
    eprintln!(
        "soak: done after {secs}s on {workers} workers: {} connections, {} echoes, \
         {} cancelled receives, {} messages received in pieces, {} datagrams, {} files, \
         {} cross-worker hops, {} connects refused by the kernel, {} connects that \
         found no free local port",
        c.connections.load(Ordering::Relaxed),
        c.round_trips.load(Ordering::Relaxed),
        c.cancelled_recvs.load(Ordering::Relaxed),
        c.split.load(Ordering::Relaxed),
        c.datagrams.load(Ordering::Relaxed),
        c.files.load(Ordering::Relaxed),
        c.cross_worker.load(Ordering::Relaxed),
        c.connects_refused.load(Ordering::Relaxed),
        c.ports_used_up.load(Ordering::Relaxed),
    );
    let samples = std::mem::take(&mut *samples.lock().unwrap());
    let idle = std::mem::take(&mut *idle.lock().unwrap());
    (counters, samples, idle)
}

/// What a runtime opens once per process (Winsock's helpers, the shared
/// address's thread) stays; what each runtime opens goes with it. Five
/// lives of a runtime: after the first, the count stays where the first
/// left it. The soak's final check relies on this.
#[test]
fn the_descriptor_count_is_flat_over_runtime_lives() {
    let before = open_fds();
    runtime_life();
    let first = settled_fds(open_fds());
    let mut counts = vec![before, first];
    for _ in 0..4 {
        runtime_life();
        counts.push(settled_fds(first));
    }
    eprintln!(
        "descriptors: {before} before, then after each life: {:?}",
        &counts[1..]
    );
    assert!(
        counts[2..].iter().all(|&n| n <= first),
        "descriptors grew over runtime lives: {before} before, then {:?}",
        &counts[1..]
    );
}

/// Where each event the soak sees appear was made: Windows' handle tracing
/// records a stack for every handle opened in the process, in a ring that
/// a long soak overwrites many times over, so a watcher thread looks for
/// new events every second and keeps the stack of each one's latest open
/// while it is still in the ring. A failed final check names the stack of
/// every event left over.
#[cfg(windows)]
mod origins {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    const PROCESS_HANDLE_TRACING: u32 = 32;
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
    /// One PROCESS_HANDLE_TRACING_ENTRY: the handle, the CLIENT_ID (process
    /// and thread), the kind of use (1: open), and 16 return addresses.
    const ENTRY: usize = 8 + 16 + 8 + 16 * 8;
    /// The PROCESS_HANDLE_TRACING_QUERY header before the entries.
    const HEADER: usize = 16;

    #[link(name = "ntdll")]
    extern "system" {
        fn NtSetInformationProcess(process: isize, class: u32, info: *const u32, len: u32) -> i32;
        fn NtQuerySystemInformation(
            class: u32,
            info: *mut u64,
            len: u32,
            returned: *mut u32,
        ) -> i32;
        fn NtQueryInformationProcess(
            process: isize,
            class: u32,
            info: *mut u64,
            len: u32,
            returned: *mut u32,
        ) -> i32;
    }

    #[link(name = "dbghelp")]
    extern "system" {
        fn SymInitializeW(process: isize, search: *const u16, invade: i32) -> i32;
        fn SymFromAddrW(process: isize, addr: u64, displacement: *mut u64, symbol: *mut u8) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventW(attributes: usize, manual: i32, initial: i32, name: usize) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn GetModuleHandleExW(flags: u32, addr: usize, module: *mut isize) -> i32;
        fn GetModuleFileNameW(module: isize, name: *mut u16, len: u32) -> u32;
    }

    /// One open as the trace has it: the thread and the return addresses.
    #[derive(Clone)]
    pub(super) struct Open {
        thread: usize,
        thread_name: String,
        seen_after: Duration,
        stack: Vec<usize>,
    }

    pub(super) struct Watcher {
        stop: Arc<AtomicBool>,
        opens: Arc<Mutex<HashMap<usize, Open>>>,
        thread: Option<JoinHandle<()>>,
    }

    /// Turns tracing on and starts watching for events not in `before`.
    /// `None` if Windows refuses the tracing (the check then goes without).
    pub(super) fn watch() -> Option<Watcher> {
        // With tracing on, a use of a value that is no handle raises
        // STATUS_INVALID_HANDLE, so the watcher never touches handles: it
        // finds events in the system's handle list, by the type number an
        // event of its own has there.
        let event_type = event_type()?;
        let before: HashSet<usize> = own_handles(event_type).into_iter().collect();
        // PROCESS_HANDLE_TRACING_ENABLE_EX: no flags, the largest ring.
        let enable = [0u32, 0x2_0000];
        // SAFETY: this process's pseudo handle and a struct of the size given.
        let status =
            unsafe { NtSetInformationProcess(-1, PROCESS_HANDLE_TRACING, enable.as_ptr(), 8) };
        if status < 0 {
            eprintln!(
                "soak: handle tracing refused ({status:#x}); events left over are not traced"
            );
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let opens = Arc::new(Mutex::new(HashMap::new()));
        let thread = {
            let (stop, opens) = (stop.clone(), opens.clone());
            std::thread::Builder::new()
                .name("soak-origins".into())
                .spawn(move || {
                    let start = Instant::now();
                    let mut buf = vec![0u64; 1 << 19];
                    while !stop.load(Ordering::Relaxed) {
                        for handle in own_handles(event_type) {
                            if before.contains(&handle) {
                                continue;
                            }
                            if let Some(open) = latest_open(handle, &mut buf, start) {
                                opens.lock().unwrap().insert(handle, open);
                            }
                        }
                        std::thread::sleep(Duration::from_secs(1));
                    }
                })
                .unwrap()
        };
        Some(Watcher {
            stop,
            opens,
            thread: Some(thread),
        })
    }

    /// The process's handles in the system's handle list
    /// (SystemExtendedHandleInformation), with each one's type number.
    fn own_handle_types() -> Vec<(usize, u16)> {
        const ENTRY: usize = 40;
        let pid = std::process::id() as usize;
        let mut buf = vec![0u64; 1 << 18];
        loop {
            let mut returned = 0u32;
            // SAFETY: a buffer of the length given, which the call fills
            // with a count and SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX entries.
            let status = unsafe {
                NtQuerySystemInformation(
                    64,
                    buf.as_mut_ptr(),
                    (buf.len() * 8) as u32,
                    &mut returned,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH && buf.len() < 1 << 26 {
                let len = buf.len() * 2;
                buf.resize(len, 0);
                continue;
            }
            if status < 0 {
                return Vec::new();
            }
            // SAFETY: the buffer's own bytes.
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 8) };
            let word = |at: usize| usize::from_ne_bytes(bytes[at..at + 8].try_into().unwrap());
            let count = word(0).min((bytes.len() - 16) / ENTRY);
            return (0..count)
                .map(|i| 16 + i * ENTRY)
                .filter(|&at| word(at + 8) == pid)
                .map(|at| {
                    (
                        word(at + 16),
                        u16::from_ne_bytes([bytes[at + 30], bytes[at + 31]]),
                    )
                })
                .collect();
        }
    }

    /// The process's handles of type number `kind`.
    fn own_handles(kind: u16) -> Vec<usize> {
        own_handle_types()
            .into_iter()
            .filter(|&(_, t)| t == kind)
            .map(|(h, _)| h)
            .collect()
    }

    /// The type number of events, from one made for the purpose.
    fn event_type() -> Option<u16> {
        // SAFETY: makes an unnamed event and closes it once looked up.
        let event = unsafe { CreateEventW(0, 1, 0, 0) };
        let kind = own_handle_types()
            .into_iter()
            .find(|&(h, _)| h == event as usize)
            .map(|(_, t)| t);
        // SAFETY: the event made above.
        unsafe { CloseHandle(event) };
        if kind.is_none() {
            eprintln!(
                "soak: the system's handle list is not readable; events left over are not traced"
            );
        }
        kind
    }

    /// The latest open of `handle` still in the trace, or `None` if the ring
    /// no longer holds one.
    fn latest_open(handle: usize, buf: &mut Vec<u64>, start: Instant) -> Option<Open> {
        loop {
            buf[0] = handle as u64;
            buf[1] = 0;
            let mut returned = 0u32;
            // SAFETY: a PROCESS_HANDLE_TRACING_QUERY naming the handle, in a
            // buffer of the length given, which the call fills with entries.
            let status = unsafe {
                NtQueryInformationProcess(
                    -1,
                    PROCESS_HANDLE_TRACING,
                    buf.as_mut_ptr(),
                    (buf.len() * 8) as u32,
                    &mut returned,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH && buf.len() < 1 << 23 {
                let len = buf.len() * 2;
                buf.resize(len, 0);
                continue;
            }
            if status < 0 {
                return None;
            }
            let total = buf[1] as u32 as usize;
            let bytes: &[u8] =
                // SAFETY: the buffer's own bytes.
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 8) };
            let word = |at: usize| usize::from_ne_bytes(bytes[at..at + 8].try_into().unwrap());
            // The newest entry comes first.
            return (0..total.min((bytes.len() - HEADER) / ENTRY))
                .map(|i| HEADER + i * ENTRY)
                .find(|&at| word(at) == handle && word(at + 24) as u32 == 1)
                .map(|at| {
                    let thread = word(at + 16);
                    Open {
                        thread,
                        thread_name: thread_name(thread),
                        seen_after: start.elapsed(),
                        stack: (0..16)
                            .map(|k| word(at + 32 + k * 8))
                            .filter(|&a| a != 0)
                            .collect(),
                    }
                });
        }
    }

    fn thread_name(id: usize) -> String {
        super::live_threads()
            .into_iter()
            .find_map(|t| {
                let (tid, name) = t.split_once(' ')?;
                (tid.parse::<usize>().ok()? == id).then(|| name.to_string())
            })
            .unwrap_or_else(|| "gone".into())
    }

    /// What the watcher saw, once it stopped.
    pub(super) struct Traced(HashMap<usize, Open>);

    impl Watcher {
        /// Stops watching, and turns tracing off: the handles can then be
        /// looked at one by one again.
        pub(super) fn finish(mut self) -> Traced {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            // SAFETY: no struct: turns this process's tracing off.
            let status =
                unsafe { NtSetInformationProcess(-1, PROCESS_HANDLE_TRACING, std::ptr::null(), 0) };
            assert!(
                status >= 0,
                "handle tracing could not be turned off: {status:#x}"
            );
            Traced(std::mem::take(&mut *self.opens.lock().unwrap()))
        }
    }

    impl Traced {
        /// Where each of `handles` was opened.
        /// Where each of `handles` was opened, and how many of them Winsock
        /// made for itself.
        pub(super) fn report(&self, handles: &[usize]) -> (String, usize) {
            let opens = &self.0;
            let mut winsock = 0;
            // How many events the watcher traced, to tell a trace that works
            // from one that saw nothing.
            let mut out = format!("; {} new events traced", opens.len());
            // SAFETY: loads the symbols of this process's modules, once.
            unsafe { SymInitializeW(-1, std::ptr::null(), 1) };
            for handle in handles {
                match opens.get(handle) {
                    None => out += &format!("\n  {handle:#x}: no open seen in the trace"),
                    Some(open) => {
                        let by_winsock = made_by_winsock(&open.stack);
                        winsock += usize::from(by_winsock);
                        out += &format!(
                            "\n  {handle:#x}: opened on thread {} ({}), seen {:?} into the soak{}:",
                            open.thread,
                            open.thread_name,
                            open.seen_after,
                            if by_winsock { ", by Winsock" } else { "" }
                        );
                        for &addr in &open.stack {
                            out += &format!("\n    {}", symbol(addr));
                        }
                    }
                }
            }
            (out, winsock)
        }
    }

    /// Whether the code that asked for the handle is Winsock's: the first
    /// caller outside the kernel and the layers every open goes through
    /// (ntdll, kernelbase, kernel32) is ws2_32 or mswsock.
    fn made_by_winsock(stack: &[usize]) -> bool {
        stack
            .iter()
            .filter(|&&addr| addr < 0xffff_8000_0000_0000)
            .map(|&addr| module_of(addr).to_ascii_lowercase())
            .find(|m| !matches!(m.as_str(), "ntdll.dll" | "kernelbase.dll" | "kernel32.dll"))
            .is_some_and(|m| m == "ws2_32.dll" || m == "mswsock.dll")
    }

    /// The file name of the module `addr` is in (`?` if none).
    fn module_of(addr: usize) -> String {
        let mut module = 0isize;
        let mut name = [0u16; 260];
        // SAFETY: GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS (4) with
        // UNCHANGED_REFCOUNT (2) only looks the module up; the name goes
        // into a local of the length given.
        unsafe {
            if GetModuleHandleExW(6, addr, &mut module) == 0 {
                return "?".into();
            }
            let n = GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) as usize;
            let path = String::from_utf16_lossy(&name[..n]);
            path.rsplit('\\').next().unwrap_or("").to_string()
        }
    }

    /// `module!symbol+offset` for a return address, as far as known.
    fn symbol(addr: usize) -> String {
        let module_name = module_of(addr);
        let mut module = 0isize;
        // SAFETY: as in `module_of`: only looks the module up.
        unsafe { GetModuleHandleExW(6, addr, &mut module) };
        // SYMBOL_INFOW: 88 bytes of fields, then the name (MaxNameLen chars).
        let mut info = vec![0u64; (88 + 2 * 512) / 8 + 1];
        let mut displacement = 0u64;
        // SAFETY: SizeOfStruct (88) and MaxNameLen (512) set, room for both.
        let symbol = unsafe {
            let raw = info.as_mut_ptr().cast::<u8>();
            *(raw as *mut u32) = 88;
            *(raw.add(80) as *mut u32) = 512;
            if SymFromAddrW(-1, addr as u64, &mut displacement, raw) != 0 {
                let len = *(raw.add(76) as *const u32) as usize;
                let chars = std::slice::from_raw_parts(raw.add(84) as *const u16, len);
                format!("{}+{displacement:#x}", String::from_utf16_lossy(chars))
            } else {
                format!("{:#x}", addr - module as usize)
            }
        };
        format!("{module_name}!{symbol}")
    }
}
