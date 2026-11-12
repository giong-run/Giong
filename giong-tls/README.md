# giong-tls

TLS for the [giong](../README.md) runtime, built on [rustls](https://github.com/rustls/rustls) (TLS 1.2 and 1.3, cryptography from `ring`, no OpenSSL): `TlsAcceptor` for servers, `TlsConnector` for clients.

*[Đọc bằng tiếng Việt](README.vi.md)*

```rust
use giong::net::TcpListener;
use giong_tls::TlsAcceptor;

fn main() -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from_pem(
        &std::fs::read("cert.pem")?,
        &std::fs::read("key.pem")?,
    )?;

    giong::block_on(async move {
        let listener = TcpListener::bind("0.0.0.0:8443")?;
        loop {
            let tcp = listener.accept().await?;
            let acceptor = acceptor.clone();
            giong::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else { return };
                while let Ok(data) = tls.recv().await {
                    if data.is_empty() {
                        break; // the peer closed
                    }
                    if tls.write_all(data).await.0.is_err() {
                        break;
                    }
                }
                let _ = tls.shutdown().await;
            });
        }
    })
}
```

For HTTPS you do not need to write this loop yourself: turn on the `tls` feature of [`giong-http`](../giong-http/README.md) and call `Server::serve_tls`.

A client, checking the server's certificate against the certificate authorities installed on the machine:

```rust
use giong::net::TcpStream;
use giong_tls::TlsConnector;

fn main() -> std::io::Result<()> {
    let connector = TlsConnector::with_system_roots()?;

    giong::block_on(async move {
        let tcp = TcpStream::connect("example.com:443").await?;
        let tls = connector.connect("example.com", tcp).await?;
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
        tls.write_all(request.to_vec()).await.0?;
        loop {
            let data = tls.recv().await?;
            if data.is_empty() {
                break;
            }
            print!("{}", String::from_utf8_lossy(&data));
        }
        Ok(())
    })
}
```

## API

| | |
|---|---|
| `TlsAcceptor::from_pem(cert, key)` | One certificate chain and its private key in PEM form. TLS 1.2 and 1.3, the default rustls cipher suites, no client authentication, ALPN `http/1.1`. |
| `TlsAcceptor::new(config)` | Bring your own `rustls::ServerConfig` (several certificates by SNI, client authentication, other ALPN protocols). The crate re-exports `rustls` so that you use the matching version. |
| `.handshake_timeout(duration)` | The longest a handshake may take, 10 s by default. |
| `acceptor.accept(tcp)` | Runs the handshake and returns a `TlsStream`. |
| `TlsConnector::with_system_roots()` | Trusts the certificate authorities installed on the machine: `SSL_CERT_FILE` / `SSL_CERT_DIR` if set, otherwise the distribution's bundle (`/etc/ssl/certs/ca-certificates.crt` and the other usual places). A clear `NotFound` error if there are none, as in an image built `FROM scratch`. |
| `TlsConnector::with_webpki_roots()` | Mozilla's root certificates compiled into the program, for machines without a bundle. Needs the `webpki-roots` feature. |
| `TlsConnector::with_root_pem(pem)`, `with_roots(store)` | Trusts only the given authorities: a private CA, or a server's own self-signed certificate. |
| `TlsConnector::new(config)` | Bring your own `rustls::ClientConfig`. |
| `.alpn(&[b"h2", b"http/1.1"])`, `.client_auth(cert, key)` | Protocols to offer through ALPN; a client certificate for servers that ask for one (mutual TLS). |
| `connector.connect(name, tcp)` | Runs the handshake, checking that the certificate is valid for `name` (a DNS name, sent through SNI, or an IP address). A certificate for another name, expired, or from an unknown authority fails with `InvalidData` and a message saying which. Clones of a connector share a session cache, so reconnecting resumes the session. |
| `tls.recv()` | Decrypted data. An empty buffer means the peer closed. |
| `tls.write_all(buf)` | Encrypts and sends all of `buf`, then hands the buffer back, like `TcpStream::write_all`. |
| `tls.shutdown()` | Sends `close_notify`, then closes the sending direction. |
| `tls.alpn_protocol()`, `tls.tcp()` | The protocol agreed through ALPN; the TCP socket underneath. |
| `tls.peer_certificates()`, `tls.resumed()` | The certificate chain the other side presented; whether the handshake resumed an earlier session. |

Four things to remember:

1. `recv()` is cancel safe: wrapping it in a `timeout` loses no data.
2. `write_all()` is not. If the future is dropped in the middle of a TLS record, the record stream has been cut, so every later send returns an error instead of sending corrupt data. Data the peer already sent can still be received.
3. One task may receive while another sends on the same `TlsStream`; records are never interleaved.
4. Data that arrived intact is always returned before an error that happened after it.

## Performance

Measured with `bench/tls.sh` on one core, with the same rustls and `ring` on both sides: HTTPS through `giong-http` reaches 144.8k req/s on `/json` against 81.0k for Axum + rustls; compared with plain HTTP on giong, TLS costs 4% on `/json` and 14% with an 8 KB response. Handshakes per second are level (7.1k against 7.3k), because that part is almost entirely cryptography. Details and how to read the numbers are in the [main README](../README.md#https).

## Not there yet

kTLS, reloading certificates while running (you can do it yourself through `ServerConfig::cert_resolver`), checking certificate revocation on the client side (rustls can, through `TlsConnector::new` with your own verifier). The crate has had no independent security audit; the cryptography and the protocol are rustls's.

## Tests

`cargo test -p giong-tls --all-features`: 7 server tests on real sockets with a rustls client (TLS 1.2 and 1.3, 4 MB in each direction, ALPN, `recv` cancelled over and over, two tasks sending at once, a write cancelled half way, broken or stalled handshakes); 14 client tests against giong's own server and a blocking rustls server (TLS 1.2 and 1.3 with 4 MB each way, a certificate for another name, an unknown authority, an IP address as the name, ALPN, mutual TLS with and without a client certificate, session resumption, a server that never answers or hangs up mid-handshake); 6 unit tests for finding the machine's certificates and parsing server names; 3 documentation examples. Nine end-to-end HTTPS tests live in `giong-http` (`cargo test -p giong-http --features tls`).
