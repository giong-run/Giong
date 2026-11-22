//! quinn on a current-thread Tokio runtime (quinn-udp does GSO and GRO).
use std::net::SocketAddr;

use bytes::Bytes;

include!("../args.rs");

static ZEROS: [u8; 65536] = [0; 65536];

fn main() {
    let addr: SocketAddr = arg("addr", "127.0.0.1:4433".parse().unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let ep = quinn::Endpoint::server(bench_quic::server_config(), addr).unwrap();
        eprintln!("quinn listening on {addr}");
        let body = Bytes::from_static(&ZEROS);
        while let Some(inc) = ep.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let Ok(conn) = inc.await else { return };
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let body = body.clone();
                    tokio::spawn(async move {
                        let Ok(req) = recv.read_to_end(8).await else {
                            return;
                        };
                        let Ok(req) = <[u8; 8]>::try_from(req.as_slice()) else {
                            return;
                        };
                        let mut left = u64::from_be_bytes(req);
                        while left > 0 {
                            let n = left.min(body.len() as u64) as usize;
                            if send.write_chunk(body.slice(..n)).await.is_err() {
                                return;
                            }
                            left -= n as u64;
                        }
                        let _ = send.finish();
                    });
                }
            });
        }
    });
}
