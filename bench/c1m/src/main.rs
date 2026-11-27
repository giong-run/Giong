//! Opens many connections to an HTTP server on another machine, answers one request on
//! each, and holds them quiet: what a server keeps per idle keep-alive connection.
//!
//!     c1m-hold --addr 10.0.0.2 --ports 3100-3119 --conns 1000000 --mode http1
//!              [--threads 16] [--ca cert.pem] [--path /json] [--ready ready.json]
//!
//! Modes: `http1` (HTTP/1.1), `h2c` (HTTP/2 with prior knowledge), `https` (HTTP/1.1 over
//! TLS 1.3, ALPN http/1.1), `h2tls` (HTTP/2 over TLS 1.3, ALPN h2). TLS handshakes are
//! full ones (no resumption), against the certificate in `--ca`.
//!
//! Connection i goes to port FIRST + i % (number of ports), so that the ports share the
//! load and each (address, port) pair needs fewer local ports than the machine has. Each
//! thread opens its share one after another with blocking sockets. Once a connection has
//! its answer, only its socket is kept: the client holds no TLS or HTTP/2 state, and sends
//! nothing more. The server sees what an idle client looks like.
//!
//! When every connection is open (or failed), writes a JSON line to `--ready` (and to
//! stdout), then holds the connections until it is killed.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Http1,
    H2c,
    Https,
    H2Tls,
}

struct Args {
    addr: String,
    ports: (u16, u16),
    conns: usize,
    threads: usize,
    mode: Mode,
    ca: Option<String>,
    path: String,
    ready: Option<String>,
}

fn usage() -> ! {
    eprintln!("usage: c1m-hold --addr IP --ports FIRST-LAST --conns N --mode http1|h2c|https|h2tls [--threads 16] [--ca cert.pem] [--path /json] [--ready file]");
    std::process::exit(2)
}

fn args() -> Args {
    let mut a = Args {
        addr: String::new(),
        ports: (3009, 3009),
        conns: 1000,
        threads: 16,
        mode: Mode::Http1,
        ca: None,
        path: "/json".into(),
        ready: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let v = it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--addr" => a.addr = v,
            "--ports" => {
                let (x, y) = v.split_once('-').unwrap_or((&v, &v));
                a.ports = (x.parse().unwrap_or_else(|_| usage()), y.parse().unwrap_or_else(|_| usage()));
            }
            "--conns" => a.conns = v.parse().unwrap_or_else(|_| usage()),
            "--threads" => a.threads = v.parse().unwrap_or_else(|_| usage()),
            "--mode" => {
                a.mode = match v.as_str() {
                    "http1" => Mode::Http1,
                    "h2c" => Mode::H2c,
                    "https" => Mode::Https,
                    "h2tls" => Mode::H2Tls,
                    _ => usage(),
                }
            }
            "--ca" => a.ca = Some(v),
            "--path" => a.path = v,
            "--ready" => a.ready = Some(v),
            _ => usage(),
        }
    }
    if a.addr.is_empty() || a.threads == 0 || a.ports.1 < a.ports.0 {
        usage();
    }
    if matches!(a.mode, Mode::Https | Mode::H2Tls) && a.ca.is_none() {
        usage();
    }
    a
}

fn tls_config(ca: &str, alpn: &[u8]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca).expect("--ca") {
        roots.add(cert.expect("--ca")).expect("--ca");
    }
    let mut c = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    c.alpn_protocols = vec![alpn.to_vec()];
    c.resumption = rustls::client::Resumption::disabled();
    Arc::new(c)
}

/// Reads one HTTP/1.1 response with a Content-Length.
fn http1(s: &mut impl ReadWrite, host: &str, path: &str) -> std::io::Result<()> {
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())?;
    s.flush()?;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 4096];
    loop {
        let n = s.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            if !head.starts_with("http/1.1 200") {
                return Err(std::io::Error::other("status"));
            }
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            let mut have = buf.len() - end - 4;
            while have < len {
                let n = s.read(&mut chunk)?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                have += n;
            }
            return Ok(());
        }
    }
}

/// HTTP/2: the preface, empty SETTINGS, one GET on stream 1; acknowledges the server's
/// SETTINGS and reads until stream 1 ends.
fn h2(s: &mut impl ReadWrite, host: &str, path: &str, https: bool) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    out.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]); // SETTINGS, empty
    // HPACK: :method GET (2), :scheme http (6) or https (7), :path (4) literal,
    // :authority (1) literal; neither indexed.
    let mut block = vec![0x82, if https { 0x87 } else { 0x86 }, 0x04, path.len() as u8];
    block.extend_from_slice(path.as_bytes());
    block.extend_from_slice(&[0x01, host.len() as u8]);
    block.extend_from_slice(host.as_bytes());
    let n = block.len();
    out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8, 1, 0x05, 0, 0, 0, 1]); // HEADERS, END_STREAM | END_HEADERS
    out.extend_from_slice(&block);
    s.write_all(&out)?;
    s.flush()?;
    let mut head = [0u8; 9];
    let mut payload = Vec::new();
    loop {
        s.read_exact(&mut head)?;
        let len = (head[0] as usize) << 16 | (head[1] as usize) << 8 | head[2] as usize;
        let (kind, flags) = (head[3], head[4]);
        let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
        payload.resize(len, 0);
        s.read_exact(&mut payload)?;
        match kind {
            4 if flags & 1 == 0 => {
                s.write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])?; // SETTINGS ACK
                s.flush()?;
            }
            6 if flags & 1 == 0 => {
                let mut pong = vec![0, 0, 8, 6, 1, 0, 0, 0, 0];
                pong.extend_from_slice(&payload);
                s.write_all(&pong)?;
                s.flush()?;
            }
            7 => return Err(std::io::Error::other("GOAWAY")),
            3 if stream == 1 => return Err(std::io::Error::other("RST_STREAM")),
            0 | 1 if stream == 1 && flags & 1 != 0 => return Ok(()),
            _ => {}
        }
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn open(a: &Args, i: usize, tls: &Option<Arc<rustls::ClientConfig>>) -> std::io::Result<TcpStream> {
    let nports = (a.ports.1 - a.ports.0) as usize + 1;
    let port = a.ports.0 + (i % nports) as u16;
    let addr: SocketAddr = format!("{}:{port}", a.addr).parse().map_err(std::io::Error::other)?;
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    sock.set_write_timeout(Some(Duration::from_secs(10)))?;
    let host = a.addr.as_str();
    match (a.mode, tls) {
        (Mode::Http1, _) => http1(&mut sock, host, &a.path)?,
        (Mode::H2c, _) => h2(&mut sock, host, &a.path, false)?,
        (mode, Some(cfg)) => {
            let name = ServerName::try_from("localhost").unwrap();
            let conn = rustls::ClientConnection::new(cfg.clone(), name).map_err(std::io::Error::other)?;
            let mut stream = rustls::StreamOwned::new(conn, sock);
            if mode == Mode::Https {
                http1(&mut stream, host, &a.path)?;
            } else {
                h2(&mut stream, host, &a.path, true)?;
            }
            // Keep the socket only: the TLS state goes, and nothing more is sent.
            sock = stream.sock;
        }
        _ => unreachable!(),
    }
    Ok(sock)
}

fn main() {
    let a = Arc::new(args());
    let tls = match a.mode {
        Mode::Https => Some(tls_config(a.ca.as_deref().unwrap(), b"http/1.1")),
        Mode::H2Tls => Some(tls_config(a.ca.as_deref().unwrap(), b"h2")),
        _ => None,
    };
    let start = Instant::now();
    let opened = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let first_error = Arc::new(std::sync::Mutex::new(String::new()));
    let workers: Vec<_> = (0..a.threads)
        .map(|t| {
            let (a, tls, opened, errors, first_error) =
                (a.clone(), tls.clone(), opened.clone(), errors.clone(), first_error.clone());
            std::thread::spawn(move || {
                let mut held = Vec::with_capacity(a.conns / a.threads + 1);
                let mut i = t;
                while i < a.conns {
                    match open(&a, i, &tls) {
                        Ok(s) => {
                            held.push(s);
                            let n = opened.fetch_add(1, Ordering::Relaxed) + 1;
                            if n % 100_000 == 0 {
                                eprintln!("{n} open after {:.1} s", start.elapsed().as_secs_f64());
                            }
                        }
                        Err(e) => {
                            if errors.fetch_add(1, Ordering::Relaxed) == 0 {
                                *first_error.lock().unwrap() = format!("connection {i}: {e}");
                            }
                        }
                    }
                    i += a.threads;
                }
                held
            })
        })
        .collect();
    let held: Vec<Vec<TcpStream>> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let secs = start.elapsed().as_secs_f64();
    let n = opened.load(Ordering::Relaxed);
    let line = format!(
        "{{\"conns\":{},\"held\":{},\"errors\":{},\"first_error\":\"{}\",\"secs\":{:.1},\"per_s\":{:.0}}}",
        a.conns,
        n,
        errors.load(Ordering::Relaxed),
        first_error.lock().unwrap().replace('"', "'"),
        secs,
        n as f64 / secs
    );
    println!("{line}");
    if let Some(path) = &a.ready {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, format!("{line}\n")).expect("--ready");
        std::fs::rename(&tmp, path).expect("--ready");
    }
    // Hold until killed.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
        let _ = &held;
    }
}
