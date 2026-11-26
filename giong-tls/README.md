# giong-tls

TLS for the [giong](../README.md) runtime, built on [rustls](https://github.com/rustls/rustls) (TLS 1.2 and 1.3, cryptography from `ring`, no OpenSSL): `TlsAcceptor` for servers, `TlsConnector` for clients.

```rust,no_run
use giong::net::TcpListener;
use giong_tls::{Certificates, TlsAcceptor};

fn main() -> std::io::Result<()> {
    let acceptor = TlsAcceptor::with_certificates(&Certificates::from_pem(
        &std::fs::read("cert.pem")?,
        &std::fs::read("key.pem")?,
    )?)?;

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

```rust,no_run
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
| `TlsAcceptor::with_certificates(&certs)` | The certificates the server presents, a `Certificates` set: `Certificates::from_pem(cert, key)` makes one from a certificate chain and its private key in PEM form. TLS 1.2 and 1.3, the default rustls cipher suites, no client authentication, no ALPN (add it with `.alpn(..)`). Of the suites both sides allow, the server picks AES-128-GCM first, as Go's and BoringSSL's servers do (see [Handshakes](#handshakes)); a client that lists ChaCha20-Poly1305 before AES-GCM, as clients without AES instructions do, gets ChaCha20. The set can hold several chains, each handshake getting the first one valid for the name the client asked for (SNI), and the first of all when it asked for none or none matches. `certs.add_pem(cert, key)` adds one; `certs.replace_pem(&[(cert, key), ..])` swaps the whole set while the server runs, for renewed certificates: every acceptor made from the set (or a clone of it) presents the new one from the next handshake on, connections already open are untouched, and on an error nothing changes. |
| `.require_client_cert(ca_pem)`, `.request_client_cert(ca_pem)` | Mutual TLS: every client must present a certificate issued by one of these authorities, or may present one (`tls.peer_certificates()` says who did). A certificate from another authority fails the handshake either way. On an acceptor made with `from_rustls` they fail with `GIONG-E2013`. |
| `TlsAcceptor::from_rustls(config)` | Bring your own `rustls::ServerConfig`, for what the rest does not cover. Needs the `unstable-rustls` feature, which also re-exports `rustls` so that you use the matching version; outside the semver promise, because the rustls version may rise in a minor release (see [Stability](#stability)). |
| `.handshake_timeout(duration)` | The longest a handshake may take, 10 s by default. |
| `.ktls(Ktls::Auto)` | Kernel TLS: after the handshake the kernel encrypts and decrypts (see [below](#kernel-tls)). `Ktls::Off` by default. Also on `TlsConnector`. |
| `acceptor.accept(tcp)` | Runs the handshake and returns a `TlsStream`. |
| `TlsConnector::with_system_roots()` | Trusts the certificate authorities installed on the machine: `SSL_CERT_FILE` / `SSL_CERT_DIR` if set, otherwise the distribution's bundle (`/etc/ssl/certs/ca-certificates.crt` and the other usual places). A clear `NotFound` error if there are none, as in an image built `FROM scratch`. |
| `TlsConnector::with_webpki_roots()` | Mozilla's root certificates compiled into the program, for machines without a bundle. Needs the `webpki-roots` feature. |
| `TlsConnector::with_root_pem(pem)` | Trusts only the given authorities: a private CA, or a server's own self-signed certificate. A self-signed certificate must not be marked as an authority, which `openssl req -x509` does unless given `-addext "basicConstraints=critical,CA:FALSE"`: a server that presents an authority's certificate as its own is refused (`GIONG-E2004`), trusted or not. |
| `TlsConnector::with_root_pem_any_name(pem)` | Trusts only the given authorities and does not check the host name, for servers whose certificates name none (MySQL's generated ones, `ssl-mode=VERIFY_CA`). Use it only with an authority that signs for that server alone. |
| `TlsConnector::from_rustls(config)` | Bring your own `rustls::ClientConfig`. Needs the `unstable-rustls` feature, as `TlsAcceptor::from_rustls`. |
| `.alpn(&[b"h2", b"http/1.1"])`, `.client_auth(cert, key)` | Protocols to offer through ALPN; a client certificate for servers that ask for one (mutual TLS). |
| `connector.connect(name, tcp)` | Runs the handshake, checking that the certificate is valid for `name` (a DNS name, sent through SNI, or an IP address). A certificate for another name, expired, or from an unknown authority fails with `InvalidData` and a message saying which. Clones of a connector share a session cache, so reconnecting resumes the session. |
| `tls.recv()` | Decrypted data, as a `Received`: it derefs to `&[u8]`, can be passed straight to `write_all`, and `into_vec()` gives a `Vec<u8>`. An empty one means the peer closed. |
| `tls.received_close_notify()` | Whether the peer ended the connection with `close_notify`. Without it, the TCP connection merely closed, which anyone on the path can cause: check it before trusting data that ends where the connection ends. |
| `tls.write_all(buf)` | Encrypts and sends all of `buf` (a `Vec<u8>`, `Box<[u8]>`, `&'static [u8]`, ...), then hands the buffer back, like `TcpStream::write_all`. |
| `tls.write_all_vectored(bufs)` | The same for several buffers in order (a header and a body), without copying them into one first. |
| `tls.send_file(&file, offset, len)` | Sends a range of a `giong::fs::File`. With kernel TLS the kernel encrypts it straight from the page cache (`sendfile`), the fastest way to serve a file over TLS; otherwise it is read and encrypted in 256 KiB pieces. |
| `tls.shutdown()` | Sends `close_notify`, then closes the sending direction. `Ok` also when the peer has closed its socket already (`EPIPE`, `ECONNRESET`), since nobody is left to tell. |
| `tls.alpn_protocol()`, `tls.tcp()` | The protocol agreed through ALPN; the TCP socket underneath, for its addresses and options only (sending or receiving on it would corrupt the TLS stream). |
| `tls.peer_certificates()`, `tls.resumed()` | The certificate chain the other side presented; whether the handshake resumed an earlier session. |
| `tls.is_ktls()` | Whether the kernel encrypts this connection. |
| `giong_tls::ktls_support()`, `ktls_stats()` | What the running kernel can do for kernel TLS, in one line for a log; how many connections went to the kernel, stayed in the process, and changed keys. |

Things to remember:

1. `recv()` is cancel safe: wrapping it in a `timeout` loses no data.
2. `write_all()`, `write_all_vectored()` and `send_file()` are not. If the future is dropped in the middle of a TLS record, the record stream has been cut, so every later send returns an error instead of sending corrupt data. Data the peer already sent can still be received, and `shutdown()` still closes the TCP side.
3. One task may receive while others send on the same `TlsStream`. Each write goes out whole: two tasks writing at once never interleave their data, in the process or with kernel TLS. Only one task may receive at a time.
4. Data that arrived intact is always returned before an error that happened after it.
5. An empty `recv()` is the end of the stream, with or without `close_notify`. If the end of the stream is what marks the end of your data (an HTTP response without a length), check `received_close_notify()`: otherwise someone on the path could cut the data short.
6. A key encrypts at most 2^24 records with AES-GCM. TLS 1.3 then changes keys; TLS 1.2 cannot, so the connection sends `close_notify` and every later write fails with `GIONG-E2012`, in the process and with kernel TLS. Small writes reach the limit after 16.7 million of them: open a new connection then.

## Kernel TLS

With kernel TLS (kTLS), rustls still does the handshake and checks the certificates; then giong-tls hands the connection's keys to the kernel, which encrypts what the program sends and decrypts what it receives. The program keeps the same `TlsStream` API, and the kernel can send encrypted data straight from its own buffers, which is what makes zero-copy sending possible over TLS.

```rust,no_run
use giong_tls::{Certificates, Ktls, TlsAcceptor};

fn main() -> std::io::Result<()> {
    let certs = Certificates::from_pem(&std::fs::read("cert.pem")?, &std::fs::read("key.pem")?)?;
    let acceptor = TlsAcceptor::with_certificates(&certs)?.ktls(Ktls::Auto)?;
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
| `Ktls::Auto` | The kernel takes a connection when it can do everything rustls would have done for it; otherwise the connection stays in the process. Fails because of kTLS only if the kernel then refuses keys it said it would take (`GIONG-E2010`): by then the connection cannot go back to the process. |
| `Ktls::Required` | The kernel takes every connection. `.ktls()` fails with `GIONG-E2009` if the kernel cannot, and so does a handshake on a stream that cannot be handed over. |

What the kernel needs, checked once per process on throwaway loopback sockets (`ktls_support()` says what it found):

- **Linux 5.15 or newer, except 6.0**, whose kernel TLS can crash the kernel when a connection closes: giong-tls refuses it and says why. Tested on 5.15, 6.1, Ubuntu's 6.8, 6.18 and 7.0.
- **The `tls` module.** `modprobe tls` on the host, or `tls` in `/etc/modules-load.d/`. A container cannot load it; the host has to. Distribution kernels build it as a module (tested on Ubuntu's).
- **Every cipher suite the configuration allows**, in both directions: AES-128-GCM, AES-256-GCM and ChaCha20-Poly1305 for TLS 1.2 and 1.3, which every kernel since 5.11 has.
- **For TLS 1.3, new keys on a live connection** (Linux 6.14 or newer): a peer may change its keys at any time with a `KeyUpdate`, and giong-tls changes its own before a key has encrypted 2^24 records. On older kernels `Auto` keeps configurations that allow TLS 1.3 in the process; `Required` takes them anyway, and a peer's `KeyUpdate` then ends the connection with `GIONG-E2011`. Linux 6.14+ takes every handshake record that starts with the byte 24 for a `KeyUpdate` and receives nothing more until it gets keys, also when the record is a later piece of a message split over records (a session ticket, say); giong-tls joins the messages itself and, when no `KeyUpdate` came, gives the kernel its keys back so that it goes on.

Two more cases stay in the process under `Auto` and fail under `Required`: a configuration that allows 0-RTT data, and a stream on which `TcpStream::recv` was used before the handshake (giong may hold bytes the kernel would never see; read a plaintext greeting with `TcpStream::read` instead). Once the kernel has taken a connection it behaves as before: session tickets reach the client's session cache, alerts and `close_notify` are handled as rustls handles them, renegotiation is refused, a tampered record is an error, `recv` is cancel safe, and a cancelled `write_all` stops the stream from sending.

### When it pays off

Kernel TLS is not faster by itself. Measured on bare metal (AMD EPYC 4345P, Linux 7.0, loopback, the server on one core, TLS 1.3 with AES-256-GCM, medians of five rounds; `bench/tls.sh` and `bench/http-client.sh`, recorded in [`bench/results/bare-metal-epyc-4345p/linux-7.0/ktls/`](../bench/results/bare-metal-epyc-4345p/linux-7.0/ktls/summary.txt)):

| What the server sends | rustls in the process | Kernel TLS | |
|---|---|---|---|
| **1 MiB from a file** (`send_file`, `Body::file`) | 4,484 req/s, 223 µs CPU each | **6,104 req/s, 164 µs** | **+36%** |
| 64 KiB from a file | 62,927 | 55,763 | −11% |
| 1 MiB from memory | 5,038 | 4,101 | −19% |
| 64 KiB from memory | 69,989 | 54,693 | −22% |
| A small JSON response | 414,168 | 347,206 | −16% |
| An 8 KB JSON response | 100,697 | 96,931 | −4% |
| A new connection per request | 14,306 | 13,255 | −7% |

On the receiving side (giong-http's client on one core) the kernel decrypting 64 KiB bodies gives +11% (63,273 → 70,492 req/s), 1 MiB bodies +6% with noisy rounds, and small JSON responses −8%. For comparison, nginx 1.24 on the same machine serves the 1 MiB file at 6,039 req/s with kernel TLS and `sendfile` (166 µs of CPU each), and at 3,547 with OpenSSL in the process.

Why: rustls with `ring`'s AES-GCM is fast, and data sent from memory has to be copied into pages the kernel allocates and zeroes for every record, which costs more than it saves. With `sendfile` nothing is copied: the profile of the 1 MiB file shows the ~20% that rustls spends copying (`pread`, then `send`) gone and the kernel's AVX-512 AES-GCM encrypting straight from the page cache. The next cost is page zeroing on allocation (`kernel_init_pages`, 11% of the kernel TLS server's time with `init_on_alloc=1`, Ubuntu's default); booting with `init_on_alloc=0` should remove most of it, which has not been measured. At 64 KiB per response the kernel's fixed costs still outweigh the copy saved; the crossover is somewhere between 64 KiB and 1 MiB.

So `Ktls::Off` stays the default. Use `Ktls::Auto` on a server whose large responses (hundreds of KiB and up) come from files through `send_file` or giong-http's `Body::file`, and on a client that downloads large bodies. Keep rustls in the process for small responses and for bodies built in memory. `bench/tls.sh` measures your own sizes: `SERVERS="giong-http-tls giong-http-ktls" ENDPOINTS="file64k file1m" bench/tls.sh`.

## Performance

Measured with `bench/tls.sh` on one core, with the same rustls and `ring` on both sides (4-vCPU VM, seven rounds, [`bench/results/tls/`](../bench/results/tls/README.md)): HTTPS through `giong-http` reaches 146.6k req/s on `/json` against 81.2k for Axum + rustls, and 48.7k against 18.5k with 8 KB responses. Details and how to read the numbers are in [docs/BENCHMARKS.md](../docs/BENCHMARKS.md#https); kernel TLS has its own numbers [above](#when-it-pays-off).

### Handshakes

With a new connection for every request, giong-http + giong-tls does 8.4k per second against 7.1k for Axum + rustls (+18%), at 116 µs of server CPU per connection against 136, with p99 latency 4.2 ms against 7.1. giong-tls 0.8.0 did 6.8k (142 µs). Two things changed:

- **Fewer packets.** A TLS 1.3 server's session tickets wait for its first response (or for a receive that would have to wait) instead of going out alone, `shutdown` sends `close_notify` in the same segment as the FIN, and the head and body of one write go out in one send. Both directions together: 14.4 TCP segments per connection, against 16.5 for 0.8.0 and 14.0 for Axum.
- **AES-128-GCM first.** `TlsAcceptor::with_certificates` picks it whatever order the client lists, as Go's and BoringSSL's servers do. OpenSSL's clients (wrk, curl, Python) list AES-256-GCM first, and its TLS 1.3 suite hashes with SHA-384, about a sixth of a resumed handshake's CPU time; SHA-256 runs in hardware on current processors. Bulk encryption gains too: 64 KiB responses went from 26.3k to 31.9k per second. Browsers list AES-128-GCM first already. A client that lists ChaCha20-Poly1305 before every AES-GCM suite (one without AES instructions) gets ChaCha20.

Most of the handshake gain is the cipher suite: Axum with its rustls set to the same order reaches the same 8.4k (116 µs), with p99 latency 6.2 ms. An acceptor made with `TlsAcceptor::from_rustls` uses the configuration as given. wrk resumes sessions, so these are resumed handshakes; a full handshake adds the certificate's signature, which is the same cryptography on both sides.

### Memory

An idle connection, rustls in the process, holds 6.1 KiB of heap (`bench/instr`'s `idle-https`, which counts the server's side after a handshake and one request), down from 10.8 KiB before 1.5.2; 8,000 idle connections to one worker of giong-http take 52.8 MiB of resident memory instead of 89.8 (`bench/tls-idle.sh`, [`bench/results/tls-idle/`](../bench/results/tls-idle/README.md)). For comparison, nginx 1.24 with OpenSSL took 15.1 KiB per idle HTTPS connection in the same kind of measurement in 0.7.

- **No receive buffer while idle.** rustls runs through its unbuffered API: received bytes are processed from a scratch buffer each thread shares, and a connection keeps bytes of its own only while it holds part of a record. rustls' buffered connection kept a 4 KiB buffer for the life of every connection.
- **No send buffer while idle.** Outgoing records are written through scratch buffers each thread shares (up to four of up to 16 KiB), where every stream kept its own.
- What is left is mostly the connection's task, which holds the TLS state (3.5 KiB), and rustls' own allocations for the keys and the handshake state.

Requests cost the same or less: 1.1% fewer instructions per HTTPS request (`bench/instr`'s `https`, client and server), and `bench/tls.sh` measured no loss on any endpoint (requests per second, median of five rounds, against 1cb2988: small JSON +1.5%, 8.5 KiB JSON +7.8%, new connections +3.8%, 64 KiB answers +2.3%, with less server CPU per request on all four).

## Stability

The stable API uses only giong-tls's own types and `CertificateDer` from `rustls-pki-types` 1.x, so a new rustls version, which giong-tls will take up as it comes, changes nothing for code that uses it. What takes rustls' types (`from_rustls`, `KtlsSupport::supports`, the `rustls` re-export) needs the `unstable-rustls` feature and follows the rustls version giong-tls uses. `TlsAcceptor::new`, `TlsConnector::new` and `TlsConnector::with_roots` are deprecated and go in 1.0.0.

## Post-quantum key exchange

With the `post-quantum` feature, giong-tls uses aws-lc-rs instead of *ring* for its cryptography, and both the server and the client offer X25519MLKEM768 first: the X25519 exchange combined with ML-KEM-768, so that recorded traffic stays secret even from a future quantum computer. Peers that do not know it agree on X25519 as before, in the same round trips: the client sends an X25519 share alongside. Building aws-lc-rs needs a C compiler and CMake. Configurations given to `from_rustls` keep their own provider.

## Not there yet

Checking certificate revocation on the client side (rustls can, through `TlsConnector::from_rustls` with your own verifier). All of them can be added in 1.x without breaking anything. The crate has had a [security review](../docs/reviews/2026-11-giong-tls.md) by the project's AI agent, which found and fixed two issues in its users, and a second one for kernel TLS, but no independent audit yet; the cryptography and the protocol are rustls's, and the kernel's once it takes a connection.

## Tests

`cargo test -p giong-tls --all-features`: 17 server tests on real sockets with a rustls client (TLS 1.2 and 1.3, 4 MB in each direction, the cipher suite `with_certificates` picks for clients listing suites in OpenSSL's, a browser's and a phone's order, a vectored write of a header, a large body and a trailer, a file read and encrypted in the process, ALPN and a server from PEM that offers none, `recv` cancelled over and over, and dropped at the poll that sends the server's session tickets with the stream still able to send, two tasks sending at once, a write cancelled half way and a shutdown after it, broken or stalled handshakes, records arriving one byte at a time, a forged record after the handshake, random bytes instead of a handshake, `close_notify` told apart from a cut connection); 2 tests in `tests/writes.rs` (three tasks writing 1 MiB each at once, plain and vectored, arrive whole and in order; a TLS 1.2 write past the cipher suite's record limit fails with `GIONG-E2012` instead of vanishing) and one in `tests/memory.rs` (after a 1 MiB write, a connection pair keeps less than 48 KiB more than before it); 5 server tests through the stable API alone in `tests/server.rs` (the certificate picked by the name the client asks for, and a client refusing the fallback; certificates replaced while serving, a connection opened before keeping its own; client certificates required, with ALPN kept, a client without one and one from another authority refused; requested and missing; `GIONG-E2013` on an acceptor from `from_rustls`); 16 client tests against giong's own server and a blocking rustls server (TLS 1.2 and 1.3 with 4 MB each way, a certificate for another name, an unknown authority, a self-signed certificate marked as an authority refused with the reason and the `openssl` option that fixes it, an IP address as the name, ALPN, mutual TLS with and without a client certificate, session resumption, also from a server that only reads, a server that never answers or hangs up mid-handshake); 13 unit tests for choosing a certificate by name, records a receive could not send whole going out first and in order, replacing a set all or nothing, the post-quantum key exchange coming first with the feature, finding the machine's certificates, parsing server names, a connection keeping a receive buffer only while it holds part of a record, telling clients that want ChaCha20 apart, and when keys are extracted; 6 documentation examples.

Kernel TLS has 23 tests in `tests/ktls.rs`, one in `tests/writes.rs` and 9 unit tests in `src/ktls.rs`: every cipher suite with the kernel on the server, the client or both sides, with plain and vectored writes and files sent with `sendfile` (whole, a range, and past the file's end); interoperation with blocking rustls peers; data sent in the same write as the last handshake message, and a key update sent in it too; ALPN, peer certificates and session resumption (TLS 1.3 tickets arrive through the kernel) after the handoff; key updates from either peer, and a key used up (new keys on TLS 1.3 with Linux 6.14+, `close_notify` and `GIONG-E2012` otherwise); the TLS 1.2 nonces read off the wire, so the kernel never reuses one rustls used; a tampered record; alerts, renegotiation requests and records of unexpected types, also in the middle of a handshake message split over records; a peer that cuts its records to 32 bytes, so the handoff comes in the middle of a record; a session ticket cut so that its later records start with the byte of a `KeyUpdate`, which Linux 6.14+ takes for one; `recv` cancelled over and over, and dropped as it answers a key update, a cancelled write, vectored write or file send, one task receiving while another sends, and three tasks writing at once; a stream already read with `recv`; and what `Auto` and `Required` do on a machine without the module. The fuzz target `ktls_records` (`cd giong-tls && cargo +nightly fuzz run ktls_records`) checks what the kernel path reads of the peer's records itself, the record framing during the handshake and the handshake messages after it, however the bytes are split; a unit test runs the same checks on 3,000 streams in every `cargo test`. Most of the tests need the `tls` module and pass without it, saying so, unless `GIONG_KTLS_REQUIRED` is set; `KTLS=1 scripts/kernels.sh 5.15.225 6.1.155 6.18.63` runs them on real kernels in QEMU, with and without key updates (CI also runs Ubuntu's 6.8).

A soak test (`tests/soak.rs`, ignored by default; `scripts/check.sh soak tls`, or `GIONG_SOAK_SECS=3600 cargo test --release -p giong-tls --all-features --test soak -- --ignored --nocapture`) churns TLS connections between giong's own acceptor and connector on every worker for as long as asked: TLS 1.3 and 1.2, resumed sessions and full handshakes, kernel TLS on half the stream ends where the `tls` module is loaded, clean closes checked for `close_notify` on both sides, dropped connections, handshakes given up part-way, receives cancelled and taken up again with every byte checked, and writes cancelled inside a record. Afterwards no descriptor, memory or driver state may be left over. CI runs it for two hours every night, on the runner's kernel and on Linux 6.18 in QEMU, both with kernel TLS.

Twelve end-to-end HTTPS tests live in `giong-http` (`cargo test -p giong-http --features tls`).
