//! TLS crates written for the Tokio and futures-io traits, over `Compat`,
//! talking to giong's own TLS (giong-tls) on the other side.

mod common;

use common::*;
use giong::time::timeout;
use giong_compat::Compat;
use giong_tls::rustls::pki_types::ServerName;
use giong_tls::{TlsAcceptor, TlsConnector, TlsStream};

/// Echoes with giong-tls's own API until the peer closes; returns whether
/// the peer sent `close_notify`.
async fn native_tls_echo(tls: TlsStream) -> bool {
    loop {
        let data = tls.recv().await.unwrap();
        if data.is_empty() {
            return tls.received_close_notify();
        }
        let (res, _) = tls.write_all(data).await;
        res.unwrap();
    }
}

fn localhost() -> ServerName<'static> {
    ServerName::try_from("localhost").unwrap()
}

mod tokio_traits {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn tokio_rustls_client_against_a_giong_tls_server() {
        giong::block_on(async {
            let pki = pki();
            let (c, s) = pair().await;
            let acceptor = TlsAcceptor::new(server_config(&pki, &[]));
            let server =
                giong::spawn(
                    async move { native_tls_echo(acceptor.accept(s).await.unwrap()).await },
                );
            let connector = tokio_rustls::TlsConnector::from(client_config(&pki, &[]));
            let mut tls = timeout(LIMIT, connector.connect(localhost(), Compat::new(c)))
                .await
                .unwrap()
                .unwrap();
            for len in [1, 1000, 16 * 1024, 1 << 20] {
                let msg = pattern(len);
                tls.write_all(&msg).await.unwrap();
                tls.flush().await.unwrap();
                let mut back = vec![0; len];
                timeout(LIMIT, tls.read_exact(&mut back))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(back == msg, "{len} bytes came back different");
            }
            // tokio-rustls sends close_notify, then shuts the stream down.
            tls.shutdown().await.unwrap();
            assert!(server.await.unwrap(), "no close_notify arrived");
        });
    }

    #[test]
    fn giong_tls_client_against_a_tokio_rustls_server() {
        giong::block_on(async {
            let pki = pki();
            let (c, s) = pair().await;
            let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&pki, &[b"echo/1"]));
            let server = giong::spawn(async move {
                let mut tls = acceptor.accept(Compat::new(s)).await.unwrap();
                assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"echo/1"[..]));
                let mut buf = vec![0; 8192];
                loop {
                    let n = tls.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    tls.write_all(&buf[..n]).await.unwrap();
                    tls.flush().await.unwrap();
                }
                tls.shutdown().await.unwrap();
            });
            let connector = TlsConnector::new(client_config(&pki, &[b"echo/1"]));
            let tls = connector.connect("localhost", c).await.unwrap();
            assert_eq!(tls.alpn_protocol().as_deref(), Some(&b"echo/1"[..]));
            let msg = pattern(300_000);
            let (res, _) = tls.write_all(msg.clone()).await;
            res.unwrap();
            let mut back = Vec::new();
            while back.len() < msg.len() {
                let data = timeout(LIMIT, tls.recv()).await.unwrap().unwrap();
                assert!(!data.is_empty(), "closed after {} bytes", back.len());
                back.extend_from_slice(&data);
            }
            assert!(back == msg);
            tls.shutdown().await.unwrap();
            assert!(timeout(LIMIT, tls.recv())
                .await
                .unwrap()
                .unwrap()
                .is_empty());
            assert!(tls.received_close_notify());
            server.await.unwrap();
        });
    }

    #[cfg(feature = "tls")]
    #[test]
    fn a_giong_tls_stream_through_the_tokio_traits() {
        giong::block_on(async {
            let pki = pki();
            let (c, s) = pair().await;
            let acceptor = TlsAcceptor::new(server_config(&pki, &[]));
            let server =
                giong::spawn(
                    async move { native_tls_echo(acceptor.accept(s).await.unwrap()).await },
                );
            let connector = TlsConnector::new(client_config(&pki, &[]));
            let mut tls = Compat::new(connector.connect("localhost", c).await.unwrap());
            assert!(format!("{tls:?}").contains("TlsStream"));
            let msg = pattern(1 << 20);
            tls.write_all(&msg).await.unwrap();
            tls.flush().await.unwrap();
            let mut back = vec![0; msg.len()];
            timeout(LIMIT, tls.read_exact(&mut back))
                .await
                .unwrap()
                .unwrap();
            assert!(back == msg);
            // Shutting down sends close_notify through giong-tls.
            tls.shutdown().await.unwrap();
            assert!(server.await.unwrap(), "no close_notify arrived");
            let mut rest = Vec::new();
            assert_eq!(tls.read_to_end(&mut rest).await.unwrap(), 0);
        });
    }

    /// The same through kernel TLS on both sides, so `Compat` drives
    /// giong-tls's kernel paths (`kernel_recv`, `kernel_write_all`, the
    /// kernel shutdown). Without the kernel's `tls` module it says why and
    /// passes, unless `GIONG_KTLS_REQUIRED` is set.
    #[cfg(feature = "tls")]
    #[test]
    fn a_kernel_tls_stream_through_the_tokio_traits() {
        use giong_tls::{ktls_support, Ktls};
        let support = ktls_support();
        if !support.is_available() {
            assert!(
                std::env::var_os("GIONG_KTLS_REQUIRED").is_none(),
                "GIONG_KTLS_REQUIRED is set, but {support}"
            );
            eprintln!("skipped: {support}");
            return;
        }
        giong::block_on(async {
            let pki = pki();
            let (c, s) = pair().await;
            let acceptor = TlsAcceptor::new(server_config(&pki, &[]))
                .ktls(Ktls::Required)
                .unwrap();
            let server = giong::spawn(async move {
                let tls = acceptor.accept(s).await.unwrap();
                assert!(tls.is_ktls());
                native_tls_echo(tls).await
            });
            let connector = TlsConnector::new(client_config(&pki, &[]))
                .ktls(Ktls::Required)
                .unwrap();
            let tls = connector.connect("localhost", c).await.unwrap();
            assert!(tls.is_ktls());
            let mut tls = Compat::new(tls);
            for len in [1, 1000, 16 * 1024, 1 << 20] {
                let msg = pattern(len);
                tls.write_all(&msg).await.unwrap();
                tls.flush().await.unwrap();
                let mut back = vec![0; len];
                timeout(LIMIT, tls.read_exact(&mut back))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(back == msg, "{len} bytes came back different");
            }
            tls.shutdown().await.unwrap();
            assert!(server.await.unwrap(), "no close_notify arrived");
            let mut rest = Vec::new();
            assert_eq!(tls.read_to_end(&mut rest).await.unwrap(), 0);
        });
    }
}

mod futures_traits {
    use super::*;
    use futures_util::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn futures_rustls_client_against_a_giong_tls_server() {
        giong::block_on(async {
            let pki = pki();
            let (c, s) = pair().await;
            let acceptor = TlsAcceptor::new(server_config(&pki, &[]));
            let server =
                giong::spawn(
                    async move { native_tls_echo(acceptor.accept(s).await.unwrap()).await },
                );
            let connector = futures_rustls::TlsConnector::from(client_config(&pki, &[]));
            let mut tls = timeout(LIMIT, connector.connect(localhost(), Compat::new(c)))
                .await
                .unwrap()
                .unwrap();
            let msg = pattern(500_000);
            tls.write_all(&msg).await.unwrap();
            tls.flush().await.unwrap();
            let mut back = vec![0; msg.len()];
            timeout(LIMIT, tls.read_exact(&mut back))
                .await
                .unwrap()
                .unwrap();
            assert!(back == msg);
            tls.close().await.unwrap();
            assert!(server.await.unwrap(), "no close_notify arrived");
        });
    }
}
