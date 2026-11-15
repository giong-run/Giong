# The API freeze for 1.0

| | |
|---|---|
| Date | 2026-11-15 |
| Code | `main` after 0.6.0 (`a018dcf`) |
| Sources | the items the 0.6 reviews of every crate left for the freeze; [the API portability check](2026-11-api-portability.md); a service built by an agent from the published documentation of 0.6.0 alone |
| Rule | [Agent-first](../../ROADMAP.md#agent-first): one obvious, strict way; errors that explain themselves; no unbounded defaults; nothing public that ties giong to Linux or HTTP/2 in a way 1.x would have to break |
| Decided by | Claude, an AI agent working on this repository |

Each item is one of: **changed** in 0.7.0 (breaking, listed in the changelog), **kept**
as it is on purpose, or **later**, an addition 1.x can make without breaking anything.
What is frozen after 0.7.0 is guarded by `cargo-semver-checks` in CI.

## giong

| Item | Decision |
|---|---|
| `bind` set `SO_REUSEPORT`, so a second process could bind the same port and take half the connections | **Changed.** `TcpListener::bind`, `bind_balanced` and `UdpSocket::bind` refuse an address another process holds. The first bind of a port in a process probes it with a socket that does not share it; the workers of the process then share the port. Sharing between processes is done by passing sockets to `from_std`. The docs promise the outcome, not the socket option, which is what macOS and Windows need later. |
| `bind`, `bind_balanced`, `UdpSocket::bind`, `UdpSocket::connect` took `std::net::ToSocketAddrs`: a host name blocked the worker in `getaddrinfo`, and the module had two traits of that name | **Changed.** They take giong's sealed `ToSocketAddrs`, and refuse a host name with `GIONG-E0100`. No `*_host` variants: `lookup_host` and then `bind` is the one way. |
| `UdpSocket::bind` set `SO_REUSEADDR` with `SO_REUSEPORT` | **Changed** to `SO_REUSEPORT` only, which keeps other users off the port. |
| `stats()` and `Stats` public but hidden | **Changed** to `observe::worker_stats()` and `observe::WorkerStats`, documented and `#[non_exhaustive]`. |
| `Sleep` is `Send`, unlike other futures | **Kept**, and documented as part of the API: giong-compat users rely on it. |
| `UdpSocket::read_from` cannot report a cut datagram | **Kept.** `recv_from` reports it (`Datagram::is_truncated`); a reporting form of `read_from` is an addition if asked for. |
| The channel errors are exhaustive | **Kept.** Empty, full and closed are a closed set, matched exhaustively, as in std and Tokio. |
| `TcpStream::send_with_control` takes Linux control-message bytes | **Changed** to hidden (`#[doc(hidden)]`), outside the semver promise; it exists for giong-tls. A typed form comes with the other systems. |
| Unix-only file methods (`OpenOptions::mode`, `custom_flags`, `Metadata::mode`, `FileType::mode`, `DirEntry::ino`) | **Kept.** macOS has them; on Windows (1.4) they can be left out of the build, which breaks no Linux or macOS code. |
| Error messages linked `blob/main/docs/errors/` | **Changed** to the release's tag. |
| The epoll notice is a plain stderr line | **Kept**; printed once, before any log line a service writes. |
| `observe::Source::stats` returns `(&str, u64)` pairs only | **Kept.** Gauges or durations can come as a provided method beside it. |
| Profiling is one switch per runtime | **Kept.** giong-mcp refuses to profile while the service does; an owned profiling session can be added beside the switch. |
| `into_std`, `AsFd`, channel `poll_recv`, `Receiver::close`, a blocking-pool size setting, `Mutex`, `Semaphore`, `Notify`, `watch`, `broadcast`, `interval`, more file operations, `spawn_blocking` | **Later**, all additions. |

## giong-http

| Item | Decision |
|---|---|
| Cleartext HTTP/2 (prior knowledge) on by default | **Kept**: gRPC clients and proxies speak to backends this way; `http2_prior_knowledge(false)` turns it off. |
| `Request::version_minor` reported `1` for HTTP/2 | **Changed**: removed; `Request::version` is the one way. |
| Header values are bytes in requests and text in responses | **Kept.** Responses refuse obs-text, which RFC 9110 deprecates; strict is the point. |
| `Response::header` took the value as `&str`, unlike giong-web's `with_header` | **Changed** to `impl AsRef<str>`. |
| `try_header` and `try_new` used `GIONG-E1101`, a client code | **Changed** to a code of their own, `GIONG-E1119`. |
| `try_header` returns `Result<Response, Error>`, giong-web's `try_with_header` returns `Result<Self, Self>` | **Kept.** The fallback differs: a response without the header cannot be sent as if nothing happened, while a web error without an extra header still answers. |
| An absolute-form target must name the `Host` host exactly | **Kept**: stricter than RFC 9112 on purpose. |
| The HTTP/2 memory budget follows `max_body_size` | **Kept**; a setting of its own is an addition. |
| `max_connections` defaulted to no limit, against the rule that no default is unbounded | **Changed** to 10,000 per `serve` call. |
| `Handler` was not implemented for `Rc<H>`, so one app on two listeners needed a wrapper, and a closure around it turned streaming off | **Changed**: `impl<H: Handler> Handler for Rc<H>`. |
| `Transport` is public and not sealed | **Kept** open for servers with their own transport; every method added later has a default. |
| Errors answered below the handler (`400`, `413`, `431`) have no body or log line | **Later**: a provided `Handler` method for them is an addition. |
| A closed connection does not drop its handlers, an HTTP/2 reset does | **Later**: behaviour, not API. |
| Client address filter, `BodySender::closed`, trailers, client request streaming | **Later**, additions (the last two for gRPC). |

## giong-web

| Item | Decision |
|---|---|
| Errors from other crates mapped by message prefix; the blanket `From<E: Error>` | **Kept**: `?` on any error is the one way, and the codes are read reliably. |
| `Error::from_status` falls back to `502` | **Kept**: a status from upstream that is not an error status is a bad gateway. |
| The strict query rule, `401` under a nested auth layer, 32-hex request ids, refusing a catch-all that starts with `/` | **Kept**, as settled in 0.6.0. |
| `json()` panicked on a status outside `200..=599` | **Changed** to an internal error. |
| `Router::merge`, forms, per-route body limits | **Later**, additions. |

## giong-tls

| Item | Decision |
|---|---|
| `recv` returned `Vec<u8>`, an allocation per call and a copy under kernel TLS | **Changed** to `Received`, which derefs to `[u8]` and is an `IoBuf`, so kernel TLS can hand over its receive buffer later without breaking anyone. |
| `write_all` took only `Vec<u8>`; `from_pem` offered ALPN `http/1.1` | Changed in 0.6.0. |
| `tcp()` gives out the raw socket | **Kept**, documented as for addresses and socket options only. |
| `rustls` is re-exported | **Kept**: a new major version of rustls is a new major version of giong-tls. |

## giong-postgres, giong-mysql, giong-redis

| Item | Decision |
|---|---|
| `Tls` exhaustive in giong-postgres and giong-redis | **Changed** to `#[non_exhaustive]`, as giong-mysql's was in 0.6.0. |
| giong-postgres's `FromSql` depends on which types arrive in binary | **Kept** the list, and **promised** it in the docs: no type moves to binary within 1.x. `Format` and `Encoded` stay exhaustive: the protocol has two formats. |
| giong-mysql's `insert` failed after writing when no id was generated | **Changed** to `Result<Option<u64>, Error>`. |
| giong-redis's `Value` exhaustive | **Changed** to `#[non_exhaustive]`. |
| giong-redis's `mget` needed two type arguments | **Changed** to one. |
| giong-redis's `ToArg` | Changed in 0.6.0 (`write_arg`), and kept. |
| Pool settings (`wait_timeout` after `new`, panicking once cloned) | **Kept**, the same in all three pools; new settings are new methods. |

## giong-mcp and giong-compat

| Item | Decision |
|---|---|
| `start_from_env` returns `FromEnv`; TCP needs a token; tool JSON follows semver | Decided in 0.6.0, kept. |
| `McpServer::url` returned `unix:/path/mcp` for a socket | **Changed** to `Option<String>`. |
| `Compat`'s name clashes with `tokio_util::compat::Compat` | **Kept**; import it under another name where both are used. |
| `Compat::with_unread` | **Later**, an addition. |

## Still open

The freeze is declared once real use in a service has reported back: what that turns up
may add items here before 1.0.0-rc.1.
