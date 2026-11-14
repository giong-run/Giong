# API check: room for gRPC, macOS, HTTP/3 and Windows after 1.0

| | |
|---|---|
| Date | 2026-11-14 |
| Code | `main` at `d9e9ee0` (0.4.0, plus kTLS and HTTP/2 server and client) |
| Question | Does anything in the public API tie giong to Linux or to HTTP/2 in a way that would force a breaking change to add gRPC (1.1), macOS (1.2), HTTP/3 (1.3) or Windows (1.4)? |
| Scope | The public items of `giong`, `giong-tls`, `giong-http` and `giong-web`, and of `giong-postgres`, `giong-mysql`, `giong-redis` and `giong-mcp` where they pick a transport |
| Method | Reading the public items and their documentation. No code was changed: what to do about each finding belongs to the API review before 1.0 |
| Reviewer | Claude, an AI agent working on this repository |

## Summary

Nothing found forces a 2.0. The enums that will grow are `#[non_exhaustive]` (`DriverKind`,
`Version`, `Body`, `Ktls`, `error::Code`, the HTTP client's `ErrorKind`). Buffers are passed by
value and handed back, which is how both io_uring and Windows' I/O completion ports work, and
the epoll driver already shows that the same API runs on a readiness interface like kqueue.
The HTTP version is a value on `Request`, not a separate kind of handler, and `giong-web` never
names a transport or a version.

What is left is small: one new signature that exposes the Linux ABI, some Unix-only methods on
types every platform will have, documentation that promises a Linux mechanism where it should
promise an outcome, and pieces that gRPC and HTTP/3 need and that can be added later without
breaking anything.

## Settle before the 1.0 freeze

These are cheap to change now and awkward or breaking afterwards.

1. **`TcpStream::send_with_control` takes raw Linux control messages.** Its `control` buffer
   is `cmsghdr` records padded as `CMSG_SPACE` says. macOS lays them out differently and
   Windows uses `WSACMSGHDR`, so the same bytes cannot mean the same thing there; QUIC will
   want control messages on UDP as well (segment size, ECN). Only `giong-tls`'s kernel TLS
   uses it, and it was added after 0.4.0, so it has not been released. Options: hide it
   (`#[doc(hidden)]`) or put a typed, non-exhaustive control message in its place.
2. **Unix-only methods on types every platform has.** `fs::OpenOptions::mode(u32)` and
   `custom_flags(i32)`, `fs::Metadata::mode()`, `fs::FileType::mode()` and
   `fs::DirEntry::ino()` cannot exist on Windows. Leaving them out of a Windows build breaks
   no one on Linux, but it means portable-looking code stops compiling there. The standard
   library keeps these in `std::os::unix` extension traits; doing the same, or at least
   documenting them as Unix only, settles the rule before 1.0. macOS is Unix, so only 1.4 is
   affected.
3. **Documentation that promises a Linux mechanism.**
   - `TcpListener::bind` and `UdpSocket::bind` promise `SO_REUSEPORT` "so the kernel spreads
     connections across cores", and for UDP that "all datagrams of one sender address go to
     the same worker". On macOS `SO_REUSEPORT` does not spread connections, and Windows has no
     such option, so there the workers will share one listening socket instead. For QUIC the
     UDP promise is the wrong one: a connection survives a change of the client's address, so
     its packets must be routed by connection ID. Promise the outcome (every worker can bind
     the same address, and connections are spread across workers) and keep `SO_REUSEPORT` as
     a note about Linux.
   - `Builder::pin_cores` defaults to `true` and says each worker is pinned to its own core.
     macOS has no hard CPU affinity. Say "where the system allows it".
   - `Builder::shutdown_on_signals` names SIGINT and SIGTERM. On Windows the same setting
     would answer Ctrl+C, Ctrl+Break and the console closing; say that it means "the usual
     ways to ask a process to stop".
   - `Handler::stream_body` says that answering before the body is read to its end closes the
     connection. That is true of HTTP/1.1 only: over HTTP/2 the stream is reset with
     `NO_ERROR` and the connection carries on. gRPC relies on the HTTP/2 behaviour.
4. **`Transport` in `giong-http` is public and not sealed.** Anyone can implement it, so a
   method added later without a default breaks them. QUIC streams will not go through it, so
   this is not a blocker for HTTP/3; decide at the freeze whether it stays open.

## Can be added after 1.0

None of these needs a change to what exists, as long as the types stay as they are.

5. **Trailers, for gRPC.** gRPC puts its status in trailers. Today the server reads and drops
   request trailers (HTTP/2 and chunked), responses cannot carry any, and the client accepts
   response trailers and drops them. What is needed is a way to end a streamed response body
   with trailers (on `BodySender`, say), and to read trailers once a `BodyStream` or a client
   `StreamingResponse` has ended. These are new methods; `Body` is non-exhaustive should a new
   variant be wanted. Keep it that way at the freeze.
6. **Streaming request bodies in the client, for gRPC.** `RequestBuilder::body` takes a
   `Vec<u8>`, and `send_streaming` streams only the response. Client-streaming and
   bidirectional calls need a request body sent as it is produced and a response that can be
   read before the request body ends: a new method on `RequestBuilder`.
7. **Full duplex on the server.** Over HTTP/2 the server keeps receiving a request body while
   a streamed response is being sent, and resets the stream only once the response has
   ended, so a handler can answer at once and go on reading. No test pins this down yet; one
   should come with gRPC. A long-lived stream that is quiet between messages also runs into
   `request_timeout`, which applies to each piece of a streamed body: gRPC needs its own
   deadline (`grpc-timeout`) instead, a new setting.
8. **HTTP/3 in `giong-http`.** `Version` can take an `Http3` variant. The server has one entry
   point per transport (`serve`, `serve_unix`, `serve_tls`), so HTTP/3 is a new one that
   takes a `UdpSocket`. `Request::version_minor` reports `1` for HTTP/2 and would for HTTP/3
   too: an HTTP/1 idea worth deprecating at the freeze, but not a break.
9. **UDP for QUIC.** Missing today, all additive: segmentation offload on send (GSO) and
   receive (GRO), ECN, the local address a datagram arrived on (and choosing the source
   address of a reply), path MTU probing, socket buffer sizes, and many datagrams per system
   call. `Datagram` has private fields, so its metadata can grow as methods. GRO has to be
   asked for, since it changes what one receive returns, and coalesced datagrams can exceed
   the 8 KiB pool buffers, so it needs a receive path of its own or larger buffers per
   socket. Routing packets to the worker that owns their connection is design work for 1.3,
   not an API change.
10. **TLS for QUIC.** `TlsAcceptor` and `TlsConnector` wrap a `TcpStream`, which suits HTTP/1.1
    and HTTP/2. QUIC uses rustls's own QUIC support with the same `ServerConfig` and
    `ClientConfig`, which `giong_tls::rustls` already exposes; an accessor for the config an
    acceptor holds would let one acceptor serve both. `Ktls::Auto` already keeps connections in
    the process where the kernel cannot take them, which is every connection on macOS and
    Windows. `TlsConnector::with_system_roots` reads Linux file paths today; on macOS and
    Windows it can read the keychain and the certificate store behind the same signature.
11. **Raw handles.** The I/O types implement `AsRawFd` only. Windows needs `AsRawSocket` and
    `AsRawHandle`, which are new trait implementations; adding `AsFd` alongside `AsRawFd` at
    the freeze costs nothing.
12. **Unix domain sockets.** `net::UnixListener`, `net::UnixStream`, `Server::serve_unix`,
    `giong_mcp::Config::unix` and the database clients' socket paths all exist on macOS. A
    Windows build can leave them out at first.

## Checked and fine

- `DriverChoice` names io_uring in its fields (`io_uring_error`, `io_uring_error_code`,
  `kernel_workarounds`). It is non-exhaustive, so other drivers leave them empty and can add
  their own.
- `Builder::ring_entries` and `Builder::recv_buffers` are tuning settings that the epoll
  driver already ignores or emulates; other drivers can do the same. `RecvBuf`, a buffer from
  the worker's pool, is emulated on epoll today. On Windows a zero-byte receive keeps idle
  connections from holding a pool buffer each.
- `observe::Snapshot`, `Pulse`, `SourceStats` and `PollProfile`, and `giong_tls::KtlsStats`,
  are non-exhaustive.
- `giong` refuses to build off Linux with a `compile_error!`; lifting it is the first step of
  1.2, not an API change.

## Outside the question, seen on the way

For the API review before 1.0: `JoinError`, `sync::mpsc::TrySendError`,
`sync::oneshot::TryRecvError`, and the `Tls` enums of `giong-postgres`, `giong-mysql` and
`giong-redis`, are not `#[non_exhaustive]`. `giong-tls` re-exports `rustls`, so a new major
version of rustls is a new major version of `giong-tls`.
