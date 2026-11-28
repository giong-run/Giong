//! Round trips over one loopback TCP connection, both ends on one giong
//! worker, with and without a deadline. Each round trip is a 16-byte
//! `write_all` and a one-off `read` on each side: the reads are the
//! operations a deadline is linked to on io_uring.
//!
//!   bench-deadline <mode> <round trips>
//!
//! Modes:
//!   plain      no deadline
//!   per-call   each round trip inside its own `Scope::with_timeout` (a
//!              deadline per request)
//!   whole      the whole loop inside one `Scope::with_timeout` (every read
//!              linked, the scope polled at every wake-up)
//!
//! Prints nanoseconds per round trip.

use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream};
#[cfg(feature = "task")]
use giong::task::Scope;

async fn round_trip(client: &TcpStream) {
    client.write_all(&[7u8; 16][..]).await.0.unwrap();
    let (n, _) = client.read(vec![0u8; 64]).await;
    assert_eq!(n.unwrap(), 16);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "plain".into());
    let n: u64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(200_000);
    let ns = giong::block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = giong::spawn(async move {
            let conn = listener.accept().await.unwrap();
            loop {
                let (got, buf) = conn.read(vec![0u8; 64]).await;
                let got = got.unwrap();
                if got == 0 {
                    return;
                }
                conn.write_all(buf[..got].to_vec()).await.0.unwrap();
            }
        });
        let client = Rc::new(TcpStream::connect(addr).await.unwrap());
        client.set_nodelay(true).unwrap();
        for _ in 0..2_000 {
            round_trip(&client).await;
        }
        let start = Instant::now();
        match mode.as_str() {
            "plain" => {
                for _ in 0..n {
                    round_trip(&client).await;
                }
            }
            #[cfg(feature = "task")]
            "per-call" => {
                for _ in 0..n {
                    Scope::with_timeout(Duration::from_secs(10))
                        .run(async {
                            round_trip(&client).await;
                            Ok::<_, std::io::Error>(())
                        })
                        .await
                        .unwrap();
                }
            }
            #[cfg(feature = "task")]
            "whole" => {
                Scope::with_timeout(Duration::from_secs(3600))
                    .run(async {
                        for _ in 0..n {
                            round_trip(&client).await;
                        }
                        Ok::<_, std::io::Error>(())
                    })
                    .await
                    .unwrap();
            }
            other => panic!("unknown mode {other}"),
        }
        let ns = start.elapsed().as_nanos() as f64 / n as f64;
        drop(Rc::try_unwrap(client).ok());
        server.await.unwrap();
        let _ = Duration::ZERO;
        ns
    });
    println!("{ns:.0}");
}
