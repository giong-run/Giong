# giong-tls

TLS for the [giong](../README.md) runtime, built on [rustls](https://github.com/rustls/rustls) (TLS 1.2 and 1.3, cryptography from `ring`, no OpenSSL): `TlsAcceptor` for servers, `TlsConnector` for clients.

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
| `.ktls(Ktls::Auto)` | Kernel TLS: after the handshake the kernel encrypts and decrypts (see [below](#kernel-tls)). `Ktls::Off` by default. Also on `TlsConnector`. |
| `acceptor.accept(tcp)` | Runs the handshake and returns a `TlsStream`. |
| `TlsConnector::with_system_roots()` | Trusts the certificate authorities installed on the machine: `SSL_CERT_FILE` / `SSL_CERT_DIR` if set, otherwise the distribution's bundle (`/etc/ssl/certs/ca-certificates.crt` and the other usual places). A clear `NotFound` error if there are none, as in an image built `FROM scratch`. |
| `TlsConnector::with_webpki_roots()` | Mozilla's root certificates compiled into the program, for machines without a bundle. Needs the `webpki-roots` feature. |
| `TlsConnector::with_root_pem(pem)`, `with_roots(store)` | Trusts only the given authorities: a private CA, or a server's own self-signed certificate. |
| `TlsConnector::new(config)` | Bring your own `rustls::ClientConfig`. |
| `.alpn(&[b"h2", b"http/1.1"])`, `.client_auth(cert, key)` | Protocols to offer through ALPN; a client certificate for servers that ask for one (mutual TLS). |
| `connector.connect(name, tcp)` | Runs the handshake, checking that the certificate is valid for `name` (a DNS name, sent through SNI, or an IP address). A certificate for another name, expired, or from an unknown authority fails with `InvalidData` and a message saying which. Clones of a connector share a session cache, so reconnecting resumes the session. |
| `tls.recv()` | Decrypted data. An empty buffer means the peer closed. |
| `tls.received_close_notify()` | Whether the peer ended the connection with `close_notify`. Without it, the TCP connection merely closed, which anyone on the path can cause: check it before trusting data that ends where the connection ends. |
| `tls.write_all(buf)` | Encrypts and sends all of `buf`, then hands the buffer back, like `TcpStream::write_all`. |
| `tls.shutdown()` | Sends `close_notify`, then closes the sending direction. |
| `tls.alpn_protocol()`, `tls.tcp()` | The protocol agreed through ALPN; the TCP socket underneath. |
| `tls.peer_certificates()`, `tls.resumed()` | The certificate chain the other side presented; whether the handshake resumed an earlier session. |
| `tls.is_ktls()` | Whether the kernel encrypts this connection. |
| `giong_tls::ktls_support()`, `ktls_stats()` | What the running kernel can do for kernel TLS, in one line for a log; how many connections went to the kernel, stayed in the process, and changed keys. |

Four things to remember:

1. `recv()` is cancel safe: wrapping it in a `timeout` loses no data.
2. `write_all()` is not. If the future is dropped in the middle of a TLS record, the record stream has been cut, so every later send returns an error instead of sending corrupt data. Data the peer already sent can still be received.
3. One task may receive while another sends on the same `TlsStream`; records are never interleaved.
4. Data that arrived intact is always returned before an error that happened after it.
5. An empty `recv()` is the end of the stream, with or without `close_notify`. If the end of the stream is what marks the end of your data (an HTTP response without a length), check `received_close_notify()`: otherwise someone on the path could cut the data short.

## Kernel TLS

With kernel TLS (kTLS), rustls still does the handshake and checks the certificates; then giong-tls hands the connection's keys to the kernel, which encrypts what the program sends and decrypts what it receives. The program keeps the same `TlsStream` API, and the kernel can send encrypted data straight from its own buffers, which is what makes zero-copy sending possible over TLS.

```rust
use giong_tls::{Ktls, TlsAcceptor};

fn main() -> std::io::Result<()> {
    let acceptor = TlsAcceptor::from_pem(&std::fs::read("cert.pem")?, &std::fs::read("key.pem")?)?
        .ktls(Ktls::Auto)?;
    if let Some(why) = acceptor.ktls_unavailable() {
        eprintln!("kernel TLS is off: {why}");
    }
    // ... accept connections as above
    Ok(())
}
```

| Mode | What it does |
|---|---|
| `Ktls::Off` | The default: rustls encrypts in the process. |
| `Ktls::Auto` | The kernel takes a connection when it can do everything rustls would have done for it; otherwise the connection stays in the process. Never fails because of kTLS. |
| `Ktls::Required` | The kernel takes every connection. `.ktls()` fails with `GIONG-E2009` if the kernel cannot, and so does a handshake on a stream that cannot be handed over. |

What the kernel needs, checked once per process on throwaway loopback sockets (`ktls_support()` says what it found):

- **The `tls` module.** `modprobe tls` on the host, or `tls` in `/etc/modules-load.d/`. A container cannot load it; the host has to. Distribution kernels build it as a module (tested on Ubuntu's).
- **Every cipher suite the configuration allows**, in both directions: AES-128-GCM, AES-256-GCM and ChaCha20-Poly1305 for TLS 1.2 and 1.3, which every kernel since 5.11 has.
- **For TLS 1.3, new keys on a live connection** (Linux 6.14 or newer): a peer may change its keys at any time with a `KeyUpdate`, and giong-tls changes its own before a key has encrypted 2^24 records. On older kernels `Auto` keeps configurations that allow TLS 1.3 in the process; `Required` takes them anyway, and a peer's `KeyUpdate` then ends the connection with `GIONG-E2011`.

Two more cases stay in the process under `Auto` and fail under `Required`: a configuration that allows 0-RTT data, and a stream on which `TcpStream::recv` was used before the handshake (giong may hold bytes the kernel would never see; read a plaintext greeting with `TcpStream::read` instead). Once the kernel has taken a connection it behaves as before: session tickets reach the client's session cache, alerts and `close_notify` are handled as rustls handles them, renegotiation is refused, a tampered record is an error, `recv` is cancel safe, and a cancelled `write_all` stops the stream from sending.

## Performance

Measured with `bench/tls.sh` on one core, with the same rustls and `ring` on both sides: HTTPS through `giong-http` reaches 144.8k req/s on `/json` against 81.0k for Axum + rustls; compared with plain HTTP on giong, TLS costs 4% on `/json` and 14% with an 8 KB response. Handshakes per second are level (7.1k against 7.3k), because that part is almost entirely cryptography. Details and how to read the numbers are in the [main README](../README.md#https).

## Not there yet

Sending files straight from the page cache over kernel TLS (`sendfile`), reloading certificates while running (you can do it yourself through `ServerConfig::cert_resolver`), checking certificate revocation on the client side (rustls can, through `TlsConnector::new` with your own verifier). The crate has had a [security review](../docs/reviews/2026-11-giong-tls.md) by the project's AI agent, which found and fixed two issues in its users, and a second one for kernel TLS, but no independent audit yet; the cryptography and the protocol are rustls's, and the kernel's once it takes a connection.

## Tests

`cargo test -p giong-tls --all-features`: 11 server tests on real sockets with a rustls client (TLS 1.2 and 1.3, 4 MB in each direction, ALPN, `recv` cancelled over and over, two tasks sending at once, a write cancelled half way, broken or stalled handshakes, records arriving one byte at a time, a forged record after the handshake, random bytes instead of a handshake, `close_notify` told apart from a cut connection); 14 client tests against giong's own server and a blocking rustls server (TLS 1.2 and 1.3 with 4 MB each way, a certificate for another name, an unknown authority, an IP address as the name, ALPN, mutual TLS with and without a client certificate, session resumption, a server that never answers or hangs up mid-handshake); 6 unit tests for finding the machine's certificates and parsing server names; 3 documentation examples.

Kernel TLS has 19 tests in `tests/ktls.rs` and 7 unit tests in `src/ktls.rs`: every cipher suite with the kernel on the server, the client or both sides; interoperation with blocking rustls peers; data sent in the same write as the last handshake message; ALPN, peer certificates and session resumption (TLS 1.3 tickets arrive through the kernel) after the handoff; key updates from either peer, and a key used up (new keys on TLS 1.3 with Linux 6.14+, `close_notify` and `GIONG-E2012` otherwise); the TLS 1.2 nonces read off the wire, so the kernel never reuses one rustls used; a tampered record; alerts, renegotiation requests and records of unexpected types; `recv` cancelled over and over, a cancelled write, one task receiving while another sends; a stream already read with `recv`; and what `Auto` and `Required` do on a machine without the module. Most need the `tls` module and pass without it, saying so, unless `GIONG_KTLS_REQUIRED` is set; `KTLS=1 scripts/kernels.sh 6.1.155 6.18.63` runs them on real kernels in QEMU, with and without key updates.

Nine end-to-end HTTPS tests live in `giong-http` (`cargo test -p giong-http --features tls`).
