//! What giong-compat's `Compat` costs, against giong's own API and Tokio.
//!
//!     bench-compat <giong|compat|compat-bufread|tokio> <recv|send|rr>
//!
//! The measured side runs on one thread pinned to `MEASURE_CPU` (default 2);
//! the other end is a blocking std thread pinned to `PEER_CPU` (default 3),
//! the same for every variant, so the differences are the measured side's.
//!
//! * `recv`: the peer sends `TOTAL_MIB` (default 2048) MiB in 256 KiB
//!   writes; the measured side reads it all. `giong` drops each `RecvBuf`
//!   (no copy), `compat` reads into a 64 KiB buffer through
//!   `tokio::io::AsyncReadExt::read` (one copy more), `compat-bufread` uses
//!   `fill_buf`/`consume` (no copy), `tokio` reads into a 64 KiB buffer.
//! * `send`: the measured side sends `TOTAL_MIB` MiB from a 64 KiB buffer:
//!   `giong` passes the buffer by value to `write_all` and gets it back,
//!   `compat` and `compat-bufread` copy it through `AsyncWriteExt::write_all`
//!   and flush at the end, `tokio` writes it.
//! * `rr`: `ROUNDS` (default 50000) round trips of a 64-byte message, which
//!   the peer echoes; TCP_NODELAY on both ends for every variant.
//!
//! `RECV_BUF_KIB` sets the size of giong's receive buffers (default 8 KiB).
//! `EXTERNAL_PEER=1` runs the other end as a child process instead of a
//! thread, so that the measured side can run alone under valgrind:
//!
//!     EXTERNAL_PEER=1 TOTAL_MIB=96 valgrind --tool=callgrind \
//!         target/release/bench-compat compat send
//!
//! Prints `<rate>,<cpu>`: MiB/s and CPU nanoseconds of the measured thread
//! per KiB for `recv`/`send`, round trips/s and CPU microseconds per round
//! trip for `rr`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::time::Instant;

const MSG: usize = 64;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn pin(cpu: usize) {
    // SAFETY: a zeroed cpu_set_t is valid; CPU_SET stays within it.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

fn thread_cpu_ns() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec to write into.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128
}

/// The other end, in a thread or (`EXTERNAL_PEER=1`, for valgrind, which
/// cannot run a second thread next to io_uring) in a child process.
enum Peer {
    Thread(std::thread::JoinHandle<()>),
    Process(std::process::Child),
}

impl Peer {
    fn wait(self) {
        match self {
            Peer::Thread(t) => t.join().unwrap(),
            Peer::Process(mut p) => assert!(p.wait().unwrap().success()),
        }
    }
}

/// Starts the peer for `workload` and returns its address.
fn peer(workload: &str, total: usize, rounds: usize) -> (SocketAddr, Peer) {
    if std::env::var_os("EXTERNAL_PEER").is_some() {
        use std::io::BufRead;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["peer", workload])
            .env_remove("EXTERNAL_PEER")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        return (line.trim().parse().unwrap(), Peer::Process(child));
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let workload = workload.to_owned();
    let handle = std::thread::spawn(move || serve_peer(listener, &workload, total, rounds));
    (addr, Peer::Thread(handle))
}

fn serve_peer(listener: TcpListener, workload: &str, total: usize, rounds: usize) {
    pin(env_usize("PEER_CPU", 3));
    {
        let (mut s, _) = listener.accept().unwrap();
        s.set_nodelay(true).unwrap();
        match workload {
            "recv" => {
                let chunk = vec![7u8; 256 * 1024];
                let mut left = total;
                while left > 0 {
                    let n = left.min(chunk.len());
                    s.write_all(&chunk[..n]).unwrap();
                    left -= n;
                }
            }
            "send" => {
                let mut buf = vec![0u8; 256 * 1024];
                let mut got = 0;
                loop {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                assert_eq!(got, total);
            }
            "rr" => {
                let mut msg = [0u8; MSG];
                for _ in 0..rounds {
                    s.read_exact(&mut msg).unwrap();
                    s.write_all(&msg).unwrap();
                }
            }
            other => panic!("unknown workload {other}"),
        }
    }
}

async fn run_giong(workload: &str, addr: SocketAddr, total: usize, rounds: usize) {
    use giong::net::TcpStream;
    let s = TcpStream::connect(addr).await.unwrap();
    s.set_nodelay(true).unwrap();
    match workload {
        "recv" => {
            let mut got = 0;
            loop {
                let data = s.recv().await.unwrap();
                if data.is_empty() {
                    break;
                }
                got += data.len();
            }
            assert_eq!(got, total);
        }
        "send" => {
            let mut buf = vec![1u8; 64 * 1024];
            for _ in 0..total / buf.len() {
                let (res, b) = s.write_all(buf).await;
                res.unwrap();
                buf = b;
            }
            s.shutdown(std::net::Shutdown::Write).unwrap();
        }
        "rr" => {
            let mut msg = vec![5u8; MSG];
            for _ in 0..rounds {
                let (res, m) = s.write_all(msg).await;
                res.unwrap();
                msg = m;
                let mut got = 0;
                while got < MSG {
                    let data = s.recv().await.unwrap();
                    assert!(!data.is_empty());
                    got += data.len();
                }
            }
        }
        _ => unreachable!(),
    }
}

async fn run_compat(workload: &str, bufread: bool, addr: SocketAddr, total: usize, rounds: usize) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let s = giong::net::TcpStream::connect(addr).await.unwrap();
    let mut c = giong_compat::Compat::new(s);
    match workload {
        "recv" if bufread => {
            let mut got = 0;
            loop {
                let n = c.fill_buf().await.unwrap().len();
                if n == 0 {
                    break;
                }
                c.consume(n);
                got += n;
            }
            assert_eq!(got, total);
        }
        "recv" => {
            let mut buf = vec![0u8; 64 * 1024];
            let mut got = 0;
            loop {
                let n = c.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                got += n;
            }
            assert_eq!(got, total);
        }
        "send" => {
            let buf = vec![1u8; 64 * 1024];
            for _ in 0..total / buf.len() {
                c.write_all(&buf).await.unwrap();
            }
            c.shutdown().await.unwrap();
        }
        "rr" => {
            let mut msg = [5u8; MSG];
            for _ in 0..rounds {
                c.write_all(&msg).await.unwrap();
                c.flush().await.unwrap();
                c.read_exact(&mut msg).await.unwrap();
            }
        }
        _ => unreachable!(),
    }
}

async fn run_tokio(workload: &str, addr: SocketAddr, total: usize, rounds: usize) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.set_nodelay(true).unwrap();
    match workload {
        "recv" => {
            let mut buf = vec![0u8; 64 * 1024];
            let mut got = 0;
            loop {
                let n = s.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                got += n;
            }
            assert_eq!(got, total);
        }
        "send" => {
            let buf = vec![1u8; 64 * 1024];
            for _ in 0..total / buf.len() {
                s.write_all(&buf).await.unwrap();
            }
            s.shutdown().await.unwrap();
        }
        "rr" => {
            let mut msg = [5u8; MSG];
            for _ in 0..rounds {
                s.write_all(&msg).await.unwrap();
                s.read_exact(&mut msg).await.unwrap();
            }
        }
        _ => unreachable!(),
    }
}

/// giong's default runtime, or with `RECV_BUF_KIB` KiB receive buffers
/// (default 8).
fn giong_runtime() -> giong::Builder {
    let builder = giong::Builder::new();
    match std::env::var("RECV_BUF_KIB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(kib) => builder.recv_buffers(512, kib << 10),
        None => builder,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (runtime, workload) = (args[1].as_str(), args[2].as_str());
    let total = env_usize("TOTAL_MIB", 2048) << 20;
    let rounds = env_usize("ROUNDS", 50_000);
    if runtime == "peer" {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        println!("{}", listener.local_addr().unwrap());
        std::io::stdout().flush().unwrap();
        return serve_peer(listener, workload, total, rounds);
    }
    let (addr, peer) = peer(workload, total, rounds);
    pin(env_usize("MEASURE_CPU", 2));

    let (start, cpu0) = (Instant::now(), thread_cpu_ns());
    match runtime {
        "giong" => giong_runtime()
            .block_on(run_giong(workload, addr, total, rounds))
            .unwrap(),
        "compat" | "compat-bufread" => giong_runtime()
            .block_on(run_compat(
                workload,
                runtime == "compat-bufread",
                addr,
                total,
                rounds,
            ))
            .unwrap(),
        "tokio" => tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap()
            .block_on(run_tokio(workload, addr, total, rounds)),
        other => panic!("unknown runtime {other}"),
    }
    let (secs, cpu) = (start.elapsed().as_secs_f64(), thread_cpu_ns() - cpu0);
    peer.wait();
    if workload == "rr" {
        println!(
            "{:.0},{:.2}",
            rounds as f64 / secs,
            cpu as f64 / 1000.0 / rounds as f64
        );
    } else {
        println!(
            "{:.0},{:.1}",
            (total >> 20) as f64 / secs,
            cpu as f64 / (total >> 10) as f64
        );
    }
}
