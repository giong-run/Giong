# giong-quic

QUIC (RFC 9000) for the [giong](https://github.com/giong-run/Giong) runtime: endpoints, connections, bidirectional and unidirectional streams, and 0-RTT, with TLS 1.3 from [rustls](https://github.com/rustls/rustls). It carries giong-http's HTTP/3 server (`Server::serve_h3`) and client, behind giong-http's `unstable-http3` feature.

Every worker runs its own endpoint on the same port, and a connection stays on one worker for its whole life, so there is no lock between connections. Connection IDs name their worker: when a client's address changes (a NAT rebinding) and the kernel delivers its packets to another worker, that worker passes them on.

```rust,no_run
use giong_quic::{Endpoint, ServerConfig};

fn tls() -> rustls::ServerConfig {
    // A rustls configuration that allows TLS 1.3 and names an ALPN protocol.
    unimplemented!()
}

fn main() -> std::io::Result<()> {
    giong::block_on(async {
        let ep = Endpoint::server("0.0.0.0:4433".parse().unwrap(), ServerConfig::new(tls())?)?;
        while let Some(conn) = ep.accept().await {
            giong::spawn(async move {
                // Each stream is answered with its own bytes.
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let req = recv.read_to_end(64 * 1024).await?;
                    send.write_all(&req).await?;
                    send.finish()?;
                }
                std::io::Result::Ok(())
            });
        }
        Ok(())
    })
}
```

Errors are `io::Error`s carrying a giong error code (`GIONG-E3601` to `GIONG-E3605`) that says what happened and how to fix it; see [docs/errors](https://github.com/giong-run/Giong/tree/main/docs/errors).

The engine underneath is quinn-proto. Nothing of it appears in the API, so a native engine built for io_uring can replace it later without a change for users.

Linux only, like giong. Minimum Rust version 1.80.

## License

MIT or Apache-2.0, at your option.
