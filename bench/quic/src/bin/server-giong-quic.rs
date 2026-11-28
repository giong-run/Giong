//! giong-quic, the crate users run: one endpoint per worker, a task per
//! connection and per stream, as `server-quinn` does with quinn. The other
//! giong servers run the benchmark's own engine; this one measures the
//! released QUIC stack.
use std::net::SocketAddr;

include!("../args.rs");

static ZEROS: [u8; 65536] = [0; 65536];

fn main() {
    let addr: SocketAddr = arg("addr", "127.0.0.1:4433".parse().unwrap());
    let workers: usize = arg("workers", 1);
    let epoll: bool = arg("epoll", 0u8) == 1;
    let mut b = giong::Builder::new()
        .workers(workers)
        .recv_buffers(256, giong::net::GRO_RECV_BUFFER);
    // `--pin 0` leaves the workers to the kernel's scheduler instead of
    // pinning each to a CPU (giong's default).
    if let Some(pin) = std::env::args().position(|a| a == "--pin") {
        let on = std::env::args().nth(pin + 1).is_some_and(|v| v != "0");
        b = b.pin_cores(on);
    }
    if epoll {
        b = b.driver(giong::DriverKind::Epoll);
    }
    b.build()
        .unwrap()
        .run(move |worker| serve(worker, addr))
        .unwrap();
}

fn config() -> giong_quic::ServerConfig {
    let (certs, key) = bench_quic::certificate();
    let mut tls = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    tls.alpn_protocols = vec![bench_quic::ALPN.to_vec()];
    // The windows of bench_quic::transport().
    let transport = giong_quic::TransportConfig::default()
        .max_streams(1024, 100)
        .windows(8 << 20, 32 << 20);
    giong_quic::ServerConfig::new(tls)
        .unwrap()
        .transport(transport)
}

async fn serve(worker: usize, addr: SocketAddr) {
    let ep = giong_quic::Endpoint::server(addr, config()).unwrap();
    if worker == 0 {
        eprintln!(
            "giong-quic listening on {addr} ({:?})",
            giong::driver_choice()
        );
    }
    while let Some(conn) = ep.accept().await {
        giong::spawn(async move {
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                giong::spawn(async move {
                    let Ok(req) = recv.read_to_end(8).await else {
                        return;
                    };
                    let Ok(req) = <[u8; 8]>::try_from(req.as_slice()) else {
                        return;
                    };
                    let mut left = u64::from_be_bytes(req);
                    while left > 0 {
                        let n = left.min(ZEROS.len() as u64) as usize;
                        if send.write_all(&ZEROS[..n]).await.is_err() {
                            return;
                        }
                        left -= n as u64;
                    }
                    let _ = send.finish();
                });
            }
        });
    }
}
