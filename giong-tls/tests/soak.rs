//! Soak test for TLS: hours of handshakes, traffic and interrupted
//! operations, then a check that nothing piled up.
//!
//! The same shape as the runtime's own soak test (`tests/soak.rs` in the
//! repository root), with a `TlsStream` at both ends of every connection.
//! Every worker runs a TLS echo server and clients that connect to all of
//! them. Connections mix TLS 1.3 and 1.2, new and resumed sessions, and,
//! where the kernel has the `tls` module, records encrypted in the process
//! and by the kernel, on either side. Clients mix clean closes
//! (`close_notify` both ways), connections dropped without one, half-closes,
//! handshakes given up part-way, receives cancelled by a timeout and then
//! taken up again, and writes cancelled in the middle of a record.
//!
//! At the end it checks that:
//! * no scenario saw an error it should not see, no byte was lost or out of
//!   place, and every clean close was seen as one by the other side;
//! * the descriptor count did not grow during the run, and is back to where
//!   it started once the runtime is dropped;
//! * resident memory levelled off after the warm-up;
//! * every worker's driver is back to idle.
//!
//! It runs for `GIONG_SOAK_SECS` seconds (default 60), so it is not part of
//! the default run:
//!
//! ```text
//! GIONG_SOAK_SECS=3600 cargo test --release -p giong-tls --test soak -- --ignored --nocapture
//! ```
//!
//! With `GIONG_KTLS_REQUIRED` set it fails unless connections went to the
//! kernel.

use std::cell::Cell;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream};
use giong::time::{sleep, timeout};
use giong_tls::rustls::pki_types::pem::PemObject;
use giong_tls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use giong_tls::rustls::{self, ClientConfig, RootCertStore, ServerConfig};
use giong_tls::{ktls_stats, ktls_support, Ktls, TlsAcceptor, TlsConnector, TlsStream};

/// Client tasks per worker.
const CLIENTS: usize = 16;

/// Client tasks per worker: [`CLIENTS`], or `GIONG_SOAK_CLIENTS`. The macOS
/// runners take fewer: at the full load their kernel refuses some sends and
/// connects with `ENOBUFS` (no buffer), where Linux makes the sender wait.
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
    resumed: AtomicU64,
    round_trips: AtomicU64,
    clean_closes: AtomicU64,
    cancelled_handshakes: AtomicU64,
    cancelled_recvs: AtomicU64,
    /// Receives cancelled and then taken up again on the same stream.
    resumed_recvs: AtomicU64,
    cancelled_writes: AtomicU64,
    /// Handshakes the server did not finish (clients give some up).
    refused: AtomicU64,
    /// Connects answered "connection refused" although every listener was
    /// up. Linux does this on its own, rarely: a client that aborts a
    /// connection the server already closed frees its port at once, and when
    /// the next connect from that port reaches the server before the abort
    /// does, the old connection's remains answer it with a reset. Plain
    /// `std::net` sockets get the same (see `tests/soak.rs` of the runtime). Counted and held to a small share, since a
    /// listener that went away would refuse thousands.
    connects_refused: AtomicU64,
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
/// Close the connection without reading the rest and without `close_notify`.
const HANG_UP: u8 = b'h';
/// Stop reading for a moment, then close: the client's write fills the
/// socket and waits, where its timeout cancels it.
const SINK: u8 = b'k';

/// A message is one of the requests above, its whole length (two bytes,
/// big-endian), a seed, then bytes that follow from the seed. Neither TCP nor
/// TLS keeps message boundaries, so both sides read by the length and check
/// every byte. The longest messages take several TLS records.
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

async fn serve(tls: TlsStream, deadline: Instant, counters: Arc<Counters>) {
    let mut pending = Vec::new();
    loop {
        // Read until a whole message is in; a hang-up or a sink needs its
        // first byte only.
        while !matches!(pending.first(), Some(&HANG_UP | &SINK))
            && (pending.len() < HEADER || pending.len() < message_len(&pending))
        {
            let left = deadline.saturating_duration_since(Instant::now()) + Duration::from_secs(1);
            match timeout(left, tls.recv()).await {
                Ok(Ok(data)) if data.is_empty() => {
                    // The client is done sending: answer its close_notify,
                    // or close what a dropped client left behind.
                    let _ = tls.shutdown().await;
                    return;
                }
                Ok(Ok(data)) => pending.extend_from_slice(&data),
                // Clients abort connections on purpose.
                Ok(Err(_)) | Err(_) => return,
            }
        }
        match pending[0] {
            HANG_UP => return,
            SINK => {
                sleep(Duration::from_millis(30)).await;
                return;
            }
            STALL => sleep(Duration::from_millis(20)).await,
            ECHO => {}
            other => {
                counters.unexpected("server", format!("unknown request {other}"));
                return;
            }
        }
        let msg: Vec<u8> = pending.drain(..message_len(&pending)).collect();
        if !intact(&msg) {
            counters.unexpected("server", "a message arrived with bytes out of place");
            return;
        }
        if tls.write_all(msg).await.0.is_err() {
            return;
        }
    }
}

/// Receives exactly `len` bytes.
async fn recv_exact(tls: &TlsStream, len: usize) -> std::io::Result<Vec<u8>> {
    let mut got = Vec::with_capacity(len);
    while got.len() < len {
        let data = tls.recv().await?;
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

/// One echo: `msg` out, the same bytes back. `false` if it went wrong, which
/// is already counted.
async fn round_trip(tls: &TlsStream, msg: &mut Vec<u8>, counters: &Counters) -> bool {
    let (res, back) = tls.write_all(std::mem::take(msg)).await;
    *msg = back;
    if let Err(err) = res {
        counters.unexpected("write", err);
        return false;
    }
    match within(counters, "echo", recv_exact(tls, msg.len())).await {
        Some(Ok(reply)) if reply == *msg => {
            Counters::bump(&counters.round_trips);
            true
        }
        Some(Ok(_)) => {
            counters.unexpected("echo", "reply differs");
            false
        }
        Some(Err(err)) => {
            counters.unexpected("echo", err);
            false
        }
        None => false,
    }
}

async fn client(
    addrs: Vec<SocketAddr>,
    connectors: Vec<TlsConnector>,
    deadline: Instant,
    rng: Rng,
    counters: Arc<Counters>,
) {
    while Instant::now() < deadline {
        let addr = addrs[rng.below(addrs.len() as u64) as usize];
        let tcp = match TcpStream::connect(addr).await {
            Ok(tcp) => tcp,
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                Counters::bump(&counters.connects_refused);
                continue;
            }
            Err(err) => {
                counters.unexpected("connect", err);
                sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        let connector = &connectors[rng.below(connectors.len() as u64) as usize];
        // One handshake in eight gets too little time and is dropped
        // wherever it is by then.
        let tls = if rng.below(8) == 0 {
            let patience = Duration::from_micros(rng.below(1500));
            match timeout(patience, connector.connect("localhost", tcp)).await {
                Ok(tls) => tls,
                Err(_) => {
                    Counters::bump(&counters.cancelled_handshakes);
                    continue;
                }
            }
        } else {
            match within(&counters, "handshake", connector.connect("localhost", tcp)).await {
                Some(tls) => tls,
                None => continue,
            }
        };
        let tls = match tls {
            Ok(tls) => tls,
            Err(err) => {
                counters.unexpected("handshake", err);
                continue;
            }
        };
        Counters::bump(&counters.connections);
        if tls.resumed() {
            Counters::bump(&counters.resumed);
        }
        // Mostly small messages; one in eight takes several records.
        let len = HEADER
            + if rng.below(8) == 0 {
                rng.below(60_000) as usize
            } else {
                rng.below(3000) as usize
            };
        let mut msg = message(ECHO, len, rng.below(256) as u8);
        match rng.below(5) {
            // Some round trips, then a clean close: close_notify both ways.
            0 | 1 => {
                let mut ok = true;
                for _ in 0..1 + rng.below(8) {
                    ok = round_trip(&tls, &mut msg, &counters).await;
                    if !ok {
                        break;
                    }
                }
                if ok {
                    if let Err(err) = tls.shutdown().await {
                        counters.unexpected("shutdown", err);
                        continue;
                    }
                    match within(&counters, "close", tls.recv()).await {
                        Some(Ok(data)) if data.is_empty() && tls.received_close_notify() => {
                            Counters::bump(&counters.clean_closes);
                        }
                        Some(Ok(data)) if data.is_empty() => {
                            counters.unexpected("close", "the server closed without close_notify");
                        }
                        Some(Ok(_)) => counters.unexpected("close", "data after the last echo"),
                        Some(Err(err)) => counters.unexpected("close", err),
                        None => {}
                    }
                }
            }
            // Ask for a slow reply and give up waiting: a cancelled receive.
            // Half the time, receive again: a cancelled receive loses nothing
            // and leaves the stream able to send.
            2 => {
                msg[0] = STALL;
                let (res, back) = tls.write_all(msg).await;
                msg = back;
                if let Err(err) = res {
                    counters.unexpected("write", err);
                    continue;
                }
                let mut got = Vec::new();
                match timeout(Duration::from_millis(1), tls.recv()).await {
                    Err(_) => Counters::bump(&counters.cancelled_recvs),
                    Ok(Ok(data)) => got = data.into_vec(),
                    Ok(Err(err)) => {
                        counters.unexpected("stalled echo", err);
                        continue;
                    }
                }
                if rng.below(2) == 0 {
                    let rest = len - got.len();
                    match within(&counters, "stalled echo", recv_exact(&tls, rest)).await {
                        Some(Ok(rest)) => {
                            got.extend_from_slice(&rest);
                            if got == msg {
                                Counters::bump(&counters.resumed_recvs);
                            } else {
                                counters.unexpected("stalled echo", "reply differs");
                                continue;
                            }
                        }
                        Some(Err(err)) => {
                            counters.unexpected("stalled echo", err);
                            continue;
                        }
                        None => continue,
                    }
                    msg[0] = ECHO;
                    round_trip(&tls, &mut msg, &counters).await;
                }
                // Dropped without close_notify, the reply perhaps on its way.
            }
            // A write too large for the socket, to a server that stops
            // reading: cancelled part-way, in the middle of a record. The
            // stream must still close without waiting forever.
            3 => {
                let mut big = vec![0u8; 4 << 20];
                big[0] = SINK;
                if timeout(Duration::from_millis(2), tls.write_all(big))
                    .await
                    .is_err()
                {
                    Counters::bump(&counters.cancelled_writes);
                }
                let _ = within(
                    &counters,
                    "shutdown after a cancelled write",
                    tls.shutdown(),
                )
                .await;
            }
            // Half-close, or have the server hang up on unread data.
            _ => {
                if rng.below(2) == 0 {
                    msg[0] = ECHO;
                    let _ = tls.write_all(msg).await;
                    let _ = tls.shutdown().await;
                    // Read the echo and then the end of the stream.
                    let _ = within(&counters, "half-close", async {
                        let _ = recv_exact(&tls, len).await;
                        let _ = tls.recv().await;
                    })
                    .await;
                } else {
                    msg[0] = HANG_UP;
                    let _ = tls.write_all(msg).await;
                    let _ = within(&counters, "hang-up", tls.recv()).await;
                }
            }
        }
    }
}

/// One self-signed certificate for `localhost`, and the configurations that
/// trust it.
struct Pki {
    cert: CertificateDer<'static>,
    key_pem: String,
}

impl Pki {
    fn new() -> Self {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        Self {
            cert: CertificateDer::from(key.cert.der().to_vec()),
            key_pem: key.key_pair.serialize_pem(),
        }
    }

    fn acceptor(&self, mode: Ktls) -> TlsAcceptor {
        let key = PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![self.cert.clone()], key)
            .unwrap();
        TlsAcceptor::from_rustls(Arc::new(config))
            .ktls(mode)
            .unwrap()
    }

    /// A client for one TLS version, with that version's cipher suites only:
    /// `Ktls::Auto` asks the kernel about every suite of the provider. With
    /// `resume` it takes up earlier sessions; without, every handshake is a
    /// full one.
    fn connector(
        &self,
        version: &'static rustls::SupportedProtocolVersion,
        mode: Ktls,
        resume: bool,
    ) -> TlsConnector {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.cipher_suites.retain(|s| s.version() == version);
        let mut roots = RootCertStore::empty();
        roots.add(self.cert.clone()).unwrap();
        let mut config = ClientConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[version])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        if !resume {
            config.resumption = rustls::client::Resumption::disabled();
        }
        TlsConnector::from_rustls(Arc::new(config))
            .ktls(mode)
            .unwrap()
    }
}

fn open_fds() -> usize {
    // macOS lists a process's own descriptors in /dev/fd.
    let dir = if cfg!(target_os = "macos") {
        "/dev/fd"
    } else {
        "/proc/self/fd"
    };
    std::fs::read_dir(dir).unwrap().count()
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

    // Kernel TLS takes part where the kernel has it: half the acceptors and
    // half the connectors ask for it, so a connection is encrypted by the
    // kernel on both sides, on one, or on none.
    let support = ktls_support();
    let kernel = support.is_available();
    if kernel {
        eprintln!("soak: kernel TLS takes part: {support}");
    } else {
        assert!(
            std::env::var_os("GIONG_KTLS_REQUIRED").is_none(),
            "GIONG_KTLS_REQUIRED is set, but {support}"
        );
        eprintln!("soak: in-process TLS only: {support}");
    }
    let pki = Pki::new();
    let mut modes = vec![Ktls::Off];
    if kernel {
        modes.push(Ktls::Auto);
    }
    let acceptors: Vec<TlsAcceptor> = modes.iter().map(|&mode| pki.acceptor(mode)).collect();
    let connectors: Vec<TlsConnector> = modes
        .iter()
        .flat_map(|&mode| {
            [
                (&rustls::version::TLS13, true),
                (&rustls::version::TLS13, false),
                (&rustls::version::TLS12, true),
                (&rustls::version::TLS12, false),
            ]
            .map(|(version, resume)| pki.connector(version, mode, resume))
        })
        .collect();
    let offloaded_before = ktls_stats().offloaded;

    let fds_before = open_fds();
    // One listener per worker, bound up front so that every worker knows the
    // others.
    let listeners: Vec<_> = (0..workers)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let addrs: Vec<SocketAddr> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();
    let listeners = Arc::new(Mutex::new(
        listeners.into_iter().map(Some).collect::<Vec<_>>(),
    ));

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
                         closes {:>8}  cancelled: handshakes {:>7}  recvs {:>7}  writes {:>7}  \
                         kernel {:>8}",
                        rss,
                        counters.connections.load(Ordering::Relaxed),
                        counters.round_trips.load(Ordering::Relaxed),
                        counters.clean_closes.load(Ordering::Relaxed),
                        counters.cancelled_handshakes.load(Ordering::Relaxed),
                        counters.cancelled_recvs.load(Ordering::Relaxed),
                        counters.cancelled_writes.load(Ordering::Relaxed),
                        ktls_stats().offloaded,
                    );
                }
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let idle = Arc::new(Mutex::new(Vec::new()));
    {
        let (counters, idle) = (counters.clone(), idle.clone());
        giong::Builder::new()
            .workers(workers)
            .recv_buffers(RECV_BUFFERS, 4096)
            .build()
            .unwrap()
            .run(move |worker| {
                let (counters, idle) = (counters.clone(), idle.clone());
                let (acceptors, connectors) = (acceptors.clone(), connectors.clone());
                let listener = listeners.lock().unwrap()[worker].take().unwrap();
                let addrs = addrs.clone();
                async move {
                    let listener = TcpListener::from_std(listener);
                    let acceptor = giong::spawn({
                        let counters = counters.clone();
                        async move {
                            let rng = Rng::new(worker as u64 + 1000);
                            loop {
                                // Clients on other workers may still be
                                // connecting right after the deadline.
                                let left = deadline.saturating_duration_since(Instant::now())
                                    + Duration::from_secs(2);
                                let tcp = match timeout(left, listener.accept()).await {
                                    Ok(Ok(tcp)) => tcp,
                                    Ok(Err(err)) => {
                                        counters.unexpected("accept", err);
                                        continue;
                                    }
                                    Err(_) => return,
                                };
                                let acceptor =
                                    acceptors[rng.below(acceptors.len() as u64) as usize].clone();
                                let counters = counters.clone();
                                giong::spawn(async move {
                                    match acceptor.accept(tcp).await {
                                        Ok(tls) => serve(tls, deadline, counters).await,
                                        // Clients give handshakes up.
                                        Err(_) => Counters::bump(&counters.refused),
                                    }
                                });
                            }
                        }
                    });

                    let mut tasks = Vec::new();
                    let clients = clients();
                    for i in 0..clients {
                        let rng = Rng::new((worker * clients + i) as u64 + 1);
                        tasks.push(giong::spawn(client(
                            addrs.clone(),
                            connectors.clone(),
                            deadline,
                            rng,
                            counters.clone(),
                        )));
                    }
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

    let c = &counters;
    let offloaded = ktls_stats().offloaded - offloaded_before;
    eprintln!(
        "soak: done after {secs}s on {workers} workers: {} connections ({} resumed sessions, \
         {} stream ends encrypted by the kernel), {} echoes, {} clean closes, {} cancelled \
         handshakes, {} cancelled receives ({} taken up again), {} cancelled writes, \
         {} handshakes the server gave up, {} connects refused by the kernel",
        c.connections.load(Ordering::Relaxed),
        c.resumed.load(Ordering::Relaxed),
        offloaded,
        c.round_trips.load(Ordering::Relaxed),
        c.clean_closes.load(Ordering::Relaxed),
        c.cancelled_handshakes.load(Ordering::Relaxed),
        c.cancelled_recvs.load(Ordering::Relaxed),
        c.resumed_recvs.load(Ordering::Relaxed),
        c.cancelled_writes.load(Ordering::Relaxed),
        c.refused.load(Ordering::Relaxed),
        c.connects_refused.load(Ordering::Relaxed),
    );

    // 1. Every scenario behaved as designed, and each one took place.
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
    assert!(c.connections.load(Ordering::Relaxed) > 0);
    assert!(c.resumed.load(Ordering::Relaxed) > 0);
    assert!(c.clean_closes.load(Ordering::Relaxed) > 0);
    assert!(c.cancelled_handshakes.load(Ordering::Relaxed) > 0);
    assert!(c.cancelled_recvs.load(Ordering::Relaxed) > 0);
    assert!(c.resumed_recvs.load(Ordering::Relaxed) > 0);
    assert!(c.cancelled_writes.load(Ordering::Relaxed) > 0);
    if kernel {
        assert!(offloaded > 0, "no connection went to the kernel");
    }

    // 2. Every worker's driver went back to idle.
    for (worker, stats, quiet) in idle.lock().unwrap().iter() {
        assert!(quiet, "worker {worker} did not go idle: {stats}");
    }

    // 3. Descriptors: nothing left once the runtime is gone, and no growth
    //    during the run beyond the connections that happen to be open.
    assert_eq!(open_fds(), fds_before, "descriptors leaked");
    let samples = samples.lock().unwrap();
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
