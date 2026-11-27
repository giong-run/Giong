//! A closed-loop HTTP/3 load generator on the `h3` crate, quinn and Tokio: CONNS
//! connections, each from its own UDP socket, with STREAMS requests in flight on each; a
//! stream sends its next request when the answer to the last one is in. Counts the requests
//! answered between the warm-up and the end, and their latency from sending to the last
//! byte. Prints one JSON object.
//!
//!     h3-client --addr 10.0.0.2:3012 --ca cert.pem [--conns 32] [--streams 8]
//!               [--threads 16] [--duration 10] [--warmup 2] [--path /json]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bench_h3::Hist;
use bytes::{Buf, Bytes};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

struct Args {
    addr: SocketAddr,
    ca: String,
    conns: usize,
    streams: usize,
    threads: usize,
    duration: f64,
    warmup: f64,
    path: String,
}

fn args() -> Args {
    let mut a = Args {
        addr: "127.0.0.1:3012".parse().unwrap(),
        ca: String::new(),
        conns: 32,
        streams: 8,
        threads: 16,
        duration: 10.0,
        warmup: 2.0,
        path: "/json".into(),
    };
    let mut it = std::env::args().skip(1);
    let usage = || -> ! {
        eprintln!("usage: h3-client --addr IP:PORT --ca cert.pem [--conns 32] [--streams 8] [--threads 16] [--duration 10] [--warmup 2] [--path /json]");
        std::process::exit(2)
    };
    while let Some(flag) = it.next() {
        let v = it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--addr" => a.addr = v.parse().unwrap_or_else(|_| usage()),
            "--ca" => a.ca = v,
            "--conns" => a.conns = v.parse().unwrap_or_else(|_| usage()),
            "--streams" => a.streams = v.parse().unwrap_or_else(|_| usage()),
            "--threads" => a.threads = v.parse().unwrap_or_else(|_| usage()),
            "--duration" => a.duration = v.parse().unwrap_or_else(|_| usage()),
            "--warmup" => a.warmup = v.parse().unwrap_or_else(|_| usage()),
            "--path" => a.path = v,
            _ => usage(),
        }
    }
    if a.ca.is_empty() || a.conns == 0 || a.streams == 0 || a.threads == 0 {
        usage();
    }
    a
}

struct Shared {
    start: Instant,
    from: Duration,
    to: Duration,
    stop: AtomicBool,
    errors: AtomicU64,
    connect_errors: AtomicU64,
    hist: Mutex<Hist>,
}

type Sender = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;

async fn one(sender: &mut Sender, uri: &http::Uri) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let req = http::Request::get(uri.clone()).body(())?;
    let mut stream = sender.send_request(req).await?;
    stream.finish().await?;
    let resp = stream.recv_response().await?;
    while let Some(mut chunk) = stream.recv_data().await? {
        chunk.advance(chunk.remaining());
    }
    if resp.status() != 200 {
        return Err("status".into());
    }
    Ok(())
}

async fn stream_loop(mut sender: Sender, uri: http::Uri, sh: Arc<Shared>) {
    let mut hist = Hist::default();
    while !sh.stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let ok = one(&mut sender, &uri).await;
        let done = sh.start.elapsed();
        if done >= sh.to {
            break;
        }
        match ok {
            Ok(()) if done >= sh.from => hist.add(t0.elapsed().as_micros() as u64),
            Ok(()) => {}
            Err(_) => {
                if done >= sh.from {
                    sh.errors.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    sh.hist.lock().unwrap().merge(&hist);
}

async fn connection(a: Arc<Args>, client: quinn::ClientConfig, sh: Arc<Shared>) {
    let bind: SocketAddr = if a.addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
    let Ok(mut endpoint) = quinn::Endpoint::client(bind) else {
        sh.connect_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    endpoint.set_default_client_config(client);
    let conn = match endpoint.connect(a.addr, "localhost") {
        Ok(c) => match c.await {
            Ok(c) => c,
            Err(e) => {
                if sh.connect_errors.load(Ordering::Relaxed) == 0 {
                    eprintln!("connect: {e}");
                }
                sh.connect_errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        },
        Err(e) => {
            if sh.connect_errors.load(Ordering::Relaxed) == 0 {
                eprintln!("connect: {e}");
            }
            sh.connect_errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let Ok((mut driver, sender)) = h3::client::new(h3_quinn::Connection::new(conn)).await else {
        sh.connect_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });
    let uri: http::Uri = format!("https://localhost{}", a.path).parse().expect("--path");
    let tasks: Vec<_> = (0..a.streams)
        .map(|_| tokio::spawn(stream_loop(sender.clone(), uri.clone(), sh.clone())))
        .collect();
    drop(sender);
    for t in tasks {
        let _ = t.await;
    }
    drive.abort();
    endpoint.close(0u32.into(), b"done");
}

fn main() {
    let a = Arc::new(args());
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(&a.ca).expect("--ca") {
        roots.add(cert.expect("--ca")).expect("--ca");
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(bench_h3::provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC TLS");
    let client = quinn::ClientConfig::new(Arc::new(crypto));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(a.threads)
        .enable_all()
        .build()
        .expect("runtime");
    let sh = Arc::new(Shared {
        start: Instant::now(),
        from: Duration::from_secs_f64(a.warmup),
        to: Duration::from_secs_f64(a.warmup + a.duration),
        stop: AtomicBool::new(false),
        errors: AtomicU64::new(0),
        connect_errors: AtomicU64::new(0),
        hist: Mutex::new(Hist::default()),
    });
    rt.block_on(async {
        let conns: Vec<_> = (0..a.conns)
            .map(|_| tokio::spawn(connection(a.clone(), client.clone(), sh.clone())))
            .collect();
        tokio::time::sleep(sh.to.saturating_sub(sh.start.elapsed())).await;
        sh.stop.store(true, Ordering::Relaxed);
        // Streams still waiting for an answer at the end are not counted; give them a
        // moment, then leave.
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            for c in conns {
                let _ = c.await;
            }
        })
        .await;
    });
    let h = sh.hist.lock().unwrap();
    println!(
        "{{\"conns\":{},\"streams\":{},\"duration_s\":{},\"requests\":{},\"rps\":{:.1},\"p50_us\":{},\"p90_us\":{},\"p99_us\":{},\"p999_us\":{},\"max_us\":{},\"errors\":{},\"connect_errors\":{}}}",
        a.conns,
        a.streams,
        a.duration,
        h.count(),
        h.count() as f64 / a.duration,
        h.quantile(0.5),
        h.quantile(0.9),
        h.quantile(0.99),
        h.quantile(0.999),
        h.max(),
        sh.errors.load(Ordering::Relaxed),
        sh.connect_errors.load(Ordering::Relaxed)
    );
    std::process::exit(0);
}
