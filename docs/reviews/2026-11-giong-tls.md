# Security review: giong-tls and the code that uses it

| | |
|---|---|
| Date | 2026-11-12 |
| Code | `main` at `359338b`, before the fixes below |
| Scope | `giong-tls` (server and client side), and every place another crate starts or uses TLS: the HTTPS server and client in `giong-http`, the TLS upgrade in `giong-postgres` and `giong-mysql` |
| Out of scope | rustls, ring and webpki themselves (see [Dependencies](#dependencies)) |
| Reviewer | Claude, an AI agent working on this repository |

**This is not an independent review.** The roadmap asks for an independent look at
`giong-tls` before anyone relies on it, and the same kind of agent wrote much of this
code. Read this as a careful self-review with tests, and still get an outside review
before trusting `giong-tls` with something that matters.

## Summary

`giong-tls` adds no cryptography of its own. It drives rustls over giong's sockets:
it moves records between rustls and the socket, keeps the record stream in order
when several tasks share a connection, and loads certificates and trust roots. The
defaults are safe, nothing in the workspace turns certificate checking off, and
`giong-tls` has no `unsafe` code.

The review found two medium issues, both in code that is new in 0.3.0 and had not been
released, and one low one. All three are fixed, each with a test that fails without the
fix.

| # | Severity | Where | Issue | Status |
|---|---|---|---|---|
| 1 | Medium | `giong-http` client | A response body that ends where the connection ends was accepted over HTTPS when the TCP connection closed without a TLS `close_notify`, so anyone on the path could cut it short | Fixed |
| 2 | Medium | `giong-mysql` | Part of a packet sent after the plaintext greeting was kept and read after the TLS handshake as if it had come through TLS | Fixed |
| 3 | Low | `giong-tls` | A task waiting to send could be listed many times while another task was sending | Fixed |

## Findings

### 1. Truncated HTTPS bodies were accepted (medium, fixed)

`TlsStream::recv` returns an empty buffer at the end of the stream, whether the peer
ended it with a `close_notify` alert or the TCP connection simply closed. Only the alert
is authenticated: anyone who can inject packets on the path can close the TCP connection
early. When the end of the stream is what marks the end of the data, that difference
matters (a truncation attack).

The `giong-http` client reads a response with neither `Content-Length` nor chunked
encoding until the connection closes (`Framing::UntilClose`). Over HTTPS it accepted the
body as complete when the TCP connection closed without `close_notify`, so an attacker
could hand the program a shortened body, a JSON document cut after a field or a file
missing its end, as a successful response.

Responses with `Content-Length` or chunked encoding were never affected (a short body is
an error), and neither was the server, which never reads a request body until the
connection closes. `giong-postgres` and `giong-mysql` frame every message.

**Fix.** `TlsStream::received_close_notify()` says whether the peer ended the stream
with the alert, and the documentation of `recv` explains when to check it. The client
now fails such a response with `GIONG-E1106` (a message naming `close_notify`) when the
alert is missing. Tests: `close_notify_is_told_apart_from_a_cut_connection` in
`giong-tls/tests/echo.rs`, `an_https_body_that_ends_with_the_connection_needs_close_notify`
in `giong-http/tests/client.rs`.

`recv` itself still returns an empty buffer in both cases: many peers close without the
alert, and data with its own framing does not need it.

### 2. Bytes slipped into the MySQL TLS stream (medium, fixed)

MySQL starts in plaintext: the server sends a greeting, the client asks for TLS, and the
handshake follows on the same connection. Anything received before the handshake and
read after it would be treated as if it had come through TLS, which lets an attacker on
the path inject data into the authenticated stream. PostgreSQL clients had this flaw as
CVE-2021-23222.

Before asking for TLS, the client checked that no *whole* packet followed the greeting.
Part of a packet (fewer than four bytes, or a header and part of its payload) passed the
check, stayed in the read buffer, and was joined to the first bytes decrypted after the
handshake.

**Fix.** The client now refuses the connection if any byte at all is held after the
greeting. Test: `bytes_after_the_greeting_cannot_slip_into_the_tls_stream` in
`giong-mysql/tests/mysql/auth.rs` (runs without a MySQL server).

`giong-postgres` was already right: it requires the answer to its TLS request to be
exactly one byte.

### 3. Duplicate wakers while waiting to send (low, fixed)

When one task is writing TLS records to the socket, others that want to send wait in a
list and are woken when it finishes. A waiting task that was polled again (for example
inside a `select`-style loop) was added to the list each time, so the list could grow
for as long as a slow send lasted. It is now listed once.

## What was checked and found sound

- **Defaults.** TLS 1.2 and 1.3 only, rustls's default cipher suites (forward secret,
  AEAD only), `ring` for cryptography. No 0-RTT data on either side (it can be replayed),
  no key log, no renegotiation (rustls does not support it).
- **Certificate checking.** The client checks the chain and the name through rustls and
  webpki, sends SNI for DNS names and checks IP addresses against IP subject names.
  Nothing in the workspace implements `ServerCertVerifier` or otherwise turns checking
  off. `giong-http` refuses redirects from HTTPS to HTTP; `giong-postgres` and
  `giong-mysql` verify by default for remote hosts and refuse the modes that silently fall
  back to plaintext.
- **Handshake.** Every handshake has a time limit (10 s by default) on both sides, and the
  HTTPS server runs each one in its own task, so a slow or silent client holds one
  connection slot, not the accept loop.
- **Memory.** Plaintext is drained from rustls as each received chunk is fed in, so the
  plaintext held is bounded by the size of one chunk; rustls bounds its own buffers.
  Outgoing data waits in rustls's bounded buffer and is flushed as it fills.
- **Hostile input.** New tests send records split into single bytes, a forged record after
  a good handshake (an error, never data), and 40 random inputs instead of a handshake
  (each connection closed within the handshake timeout, the server unharmed).
- **Concurrency and cancellation.** A connection belongs to one worker; its state is in
  `Cell`s and `RefCell`s, and no borrow is held across an `.await`. One task may receive
  while others send, and records are never interleaved. A write cancelled in the middle of
  a record marks the connection broken instead of sending a corrupt stream (existing
  tests cover both).
- **Trust roots.** `with_system_roots` reads `SSL_CERT_FILE` and `SSL_CERT_DIR` the way
  OpenSSL does, then the usual distribution paths; certificates rustls cannot parse are
  skipped, and finding none is an error rather than an empty trust store.

## Recommendations (not changed in this release)

- **Revocation.** No CRL or OCSP checking, as in rustls's default configuration. A
  revoked certificate is trusted until it expires. Short-lived certificates are the usual
  answer.
- **Mutual TLS over TLS 1.2.** A client certificate is sent unencrypted in a TLS 1.2
  handshake, which reveals the client's identity to anyone watching. Prefer TLS 1.3 where
  client certificates identify people or tenants (a custom `ClientConfig` through
  `TlsConnector::new` can require it).
- **Handshake floods.** Each handshake costs the server a signature, and `Server` accepts
  any number of connections unless `max_connections` is set. Set it, and rate-limit
  connections at the edge for a public service.
- **ALPN.** `TlsAcceptor::from_pem` offers only `http/1.1`. A client that offers ALPN
  without `http/1.1` is refused, as RFC 7301 says. Other protocols need a `ServerConfig`
  of their own (`TlsAcceptor::new`).
- **Key material in memory.** Private keys live in rustls and ring structures; giong-tls
  does not add zeroization beyond what they do.
- **Reporting.** The repository has no security policy yet. Enabling GitHub's private
  vulnerability reporting and adding a `SECURITY.md` would give people a private way to
  report issues.

## Dependencies

`cargo audit` (RustSec advisory database, 1,490 advisories, 2026-11-12): no
vulnerabilities and no warnings among the 79 crates in `Cargo.lock`. The TLS stack:
rustls 0.23.46, ring 0.17.14, rustls-webpki 0.103.16, rustls-pki-types 1.16.0,
webpki-roots 1.0.12. The rustls source was read for the paths giong-tls relies on:
`read_tls` and the received-plaintext limit, the send-side buffer limit, `close_notify`,
and the record-sequence limits.

## Method

1. Read `giong-tls/src/lib.rs` in full, then every caller: `giong-http` (`serve_tls`,
   the `Transport` implementation, the client's connection code), `giong-postgres` and
   `giong-mysql` (`connect.rs`).
2. Read the rustls 0.23.46 source for each rustls call whose edge cases matter here.
3. Wrote the tests named above against real sockets, and confirmed that the tests for
   findings 1 and 2 fail on the code before the fixes.
4. Ran `cargo audit`.
