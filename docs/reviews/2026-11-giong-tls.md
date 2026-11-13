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
`giong-tls` has no `unsafe` code. (That was true of the code reviewed here. Kernel TLS,
added later, brings system calls and `unsafe` blocks: see the
[addendum](#addendum-kernel-tls-2026-11-13).)

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

## Addendum: kernel TLS (2026-11-13)

| | |
|---|---|
| Date | 2026-11-13 |
| Code | branch `feat/ktls`, on `main` at `b14ca35` (0.4.0) |
| Scope | `giong-tls/src/ktls.rs`, the parts of `giong-tls/src/lib.rs` that hand a connection to it, and `TcpStream::send_with_control` in `giong` |
| Reviewer | Claude, the same kind of agent that wrote the code: again not an independent review |

Kernel TLS (kTLS) is new and off by default. With `Ktls::Auto` or `Ktls::Required`,
rustls still does the whole handshake, then giong-tls gives the traffic keys to the
kernel (`setsockopt(SOL_TLS, TLS_TX / TLS_RX)`), and the kernel encrypts and decrypts
every record after that. Records that are not application data (alerts, session
tickets, `KeyUpdate`, TLS 1.2 renegotiation requests) come back from the kernel one at a
time with their type, and giong-tls handles them the way rustls would have, with
rustls's `KernelConnection` deriving new keys and storing tickets.

### Does the review above still hold?

- **Defaults.** Unchanged. kTLS is off unless asked for. It does not change which
  versions or cipher suites are negotiated: `Auto` only takes connections when the
  kernel can encrypt *and* decrypt every suite the configuration allows, and
  `Required` fails a connection whose suite the kernel lacks instead of narrowing
  the list. A configuration that allows 0-RTT data cannot use kTLS at all
  (`GIONG-E2009` when the mode is set), because rustls cannot hand over a connection
  that might still receive early data.
- **Certificate checking.** Unchanged: the handshake is rustls's, with the same
  verifiers. The kernel only sees keys after the handshake has succeeded.
- **Handshake.** The kTLS handshake runs under the same time limit. It uses rustls's
  unbuffered API so that no byte read from the socket is lost or replayed: it reads
  exactly up to the end of the record the handshake ends in. Data the peer sent right
  behind its last handshake flight is either already read, and then decrypted by rustls
  and kept for the first receive, or still in the socket, where the kernel decrypts it
  with the new keys (test `data_sent_with_the_last_handshake_flight_is_not_lost`).
- **Memory.** A receive returns what the kernel decrypted, and one record that is not
  application data is read at a time into a record-sized buffer. Handshake messages
  split across records are kept until complete, up to 64 KiB for one message
  (`GIONG-E2005` beyond it).
- **Hostile input.** The kernel checks each record's tag; a record that fails it ends
  the connection with `GIONG-E2005` and is never returned as data (test
  `a_tampered_record_is_an_error_not_data`). Unlike rustls, giong-tls sends no
  `bad_record_mac` alert in that case: the kernel has already put the socket in an
  error state. Control records are checked as rustls
  checks them: alerts of the wrong size or level, `KeyUpdate` that does not end its
  record or has a bad `request_update`, handshake messages that do not belong after the
  handshake, session tickets sent to a server. The limits are the same as rustls's: 4
  TLS 1.2 warning alerts, one refused renegotiation, 32 `KeyUpdate` requests without
  data in between (test `records_that_are_not_data_are_handled_as_rustls_would`).
  After a failure every later receive returns the same error.
- **Concurrency and cancellation.** The same rules as before: one task may receive
  while others send, sends are serialised, and a write cancelled in the middle of a
  record marks the stream broken (tests `kernel_recv_is_cancel_safe`,
  `a_cancelled_kernel_write_ends_sending_instead_of_cutting_a_message`,
  `one_task_may_receive_while_another_sends`).
- **Truncation (finding 1).** Still told apart: a `close_notify` alert from the kernel
  sets `received_close_notify()`, a TCP close does not (test
  `close_notify_is_told_apart_from_a_cut_connection`).
- **Plaintext before TLS (finding 2).** A stream on which `TcpStream::recv` was already
  used, as `giong-mysql` and `giong-postgres` do for their plaintext start, is never
  handed to the kernel: `Auto` keeps it in the process and `Required` fails with
  `GIONG-E2009` (test `a_stream_already_read_with_recv_stays_in_the_process_under_auto`).

### New findings

#### 4. TLS 1.2 AES-GCM nonces would have been reused at the handoff (high, fixed before release)

In TLS 1.2, AES-GCM records carry an 8-byte explicit nonce. rustls makes it
`iv XOR sequence number`; the kernel takes a starting value and adds one per record.
Handing the kernel rustls's `iv`, the obvious move, makes the kernel's first record
reuse the nonce of the `Finished` message rustls sent under the same key. Two AES-GCM
records with one key and one nonce reveal the authentication key, after which anyone
on the path can forge records.

**Fix.** `tls12_gcm_explicit_iv` starts the kernel past the aligned block of nonces
rustls can have used, so the two sequences cannot meet before the cipher's record
limit. Tests: `the_kernel_never_reuses_a_tls12_gcm_nonce_rustls_used` (the arithmetic)
and `the_kernel_never_reuses_the_nonce_of_a_tls_1_2_finished_message`, which reads the
records off the wire and compares their nonces. TLS 1.3 and ChaCha20-Poly1305 build the
nonce the same way in rustls and the kernel and are not affected.

#### 5. Key changes and record limits move to giong-tls (by design)

rustls changes keys or closes a connection before a cipher's confidentiality limit;
once the kernel sends the records, rustls no longer counts them. giong-tls counts them
instead (estimated from the bytes sent, checked against the kernel's own sequence
number when close) and, near the limit, sends a `KeyUpdate` and installs the new key
(TLS 1.3 on Linux 6.14 or newer), or otherwise sends `close_notify` and fails further
sends with `GIONG-E2012` (test
`the_sending_key_changes_or_the_stream_ends_before_the_cipher_limit`). A peer's
`KeyUpdate` is followed on Linux 6.14 and newer; on older kernels the connection ends
with `GIONG-E2011`, which is why `Auto` keeps TLS 1.3 connections in the process there.

#### 6. No silent downgrade (by design)

`Required` never falls back: a connection the kernel cannot take fails with
`GIONG-E2009` or `GIONG-E2010` and a message that says why. `Auto` does fall back, as
documented, and the fallback can be seen: `TlsStream::is_ktls()`,
`TlsAcceptor::ktls_unavailable()` and the counters in `ktls_stats()`. Falling back
changes where records are encrypted, not what is negotiated or checked.

#### 7. Key material

The keys go from rustls (whose `AeadKey` is wiped when dropped) into the struct the
kernel expects, which is wiped with volatile writes when dropped, including the copy
the kernel returns when giong-tls reads the send sequence number. The kernel keeps its
own copy for the life of the socket and wipes it when the socket closes. Anyone who
holds the socket's descriptor can read the keys back with `getsockopt`, as anyone
inside the process could already read them from rustls's memory.

#### 8. `unsafe` code

`giong-tls/src/ktls.rs` has `unsafe` blocks for `setsockopt`, `getsockopt` and
`recvmsg` on the stream's own descriptor, for building and walking control messages
with the `CMSG_*` macros, for viewing the `repr(C)` key structs as bytes (their size
and layout are asserted against the kernel's in `crypto_info_has_the_kernel_layout`),
and for the `IoBuf` implementation of a fixed-size read window. Each block has a
`SAFETY` comment. Miri cannot run system calls, so these are covered by the tests on
real kernels (6.1 and 6.18 in QEMU, plus the CI runner's kernel) rather than by Miri.

### Recommendations

- **Get the outside review the roadmap asks for**, now covering `ktls.rs` too.
- **Hardware offload** (`TLS_HW`) was not tested. A network card that takes the records
  may refuse a new key; giong-tls then ends the connection with `GIONG-E2011` or
  `GIONG-E2012` rather than continuing under a worn key.
- **Containers.** A process without `CAP_NET_ADMIN` cannot load the `tls` module;
  `Auto` then stays in the process and `Required` fails with `GIONG-E2009`, which says
  to run `modprobe tls` on the host.

### Method

1. Read the kernel's `net/tls` code for the socket options, the record-type control
   messages, what a receive does with records that are not data, how the TLS 1.2
   explicit nonce is built, and when new keys are accepted (Linux 6.14).
2. Read rustls 0.23.46's `kernel` module, `ExtractedSecrets` and the unbuffered API.
3. Ran every kTLS test on Linux 6.1.155 and 6.18.63 in QEMU with the `tls` module
   loaded (`KTLS=1 scripts/kernels.sh`), where a skipped kTLS test counts as a failure,
   and the fallback tests on a kernel without the module.
