# giong-quic: design and status

Prototype on the `feat/quic-prototype` branch. Nothing here lands on `main`
before `v1.0.0` (0.5 to 1.0 adds no features). Plan: sections 4, 5 (1.3
HTTP/3) and 11-13 of the 1.1-1.4 plan.

## 1. What the decision benchmark says (cloud, November 2026)

`bench/quic.sh` runs one server core against a quinn client on the other
three cores of a 4-vCPU cloud VM (Linux 6.18, loopback). Five servers share
one quinn-proto engine (`bench/quic/src/engine.rs`) or are quinn itself. Raw
data: `bench/results/quic/cloud-4vcpu/`. Medians of 5 rounds, server CPU per
unit of work (the client limited the request rate in most runs, so the CPU
columns are the comparable ones):

| Server | rr: CPU per request | bulk: CPU per KiB | bulk Gbit/s |
|---|---|---|---|
| quinn 0.11 on Tokio | 6.10 µs | 1564 ns | 5.21 |
| quinn-proto + Tokio + quinn-udp (GSO/GRO) | 4.92 µs | 1429 ns | 5.66 |
| quinn-proto + giong, released UDP API | 4.96 µs | 4325 ns | 1.88 |
| quinn-proto + giong + offload prototype (io_uring) | 5.00 µs | 1455 ns | 5.18 |
| same on giong's epoll driver | 5.31 µs | 1451 ns | 4.62 |

And with no I/O at all (`bench/quic/src/bin/inmem.rs`, client and server in
one thread, memory hand-over), the quinn-proto server costs 580-810 ns per
KiB. A callgrind profile of that run puts AES-GCM at about 45% of the
server's user-space instructions; the rest is quinn-proto's packet assembly,
ACK processing and copies (memcpy alone is 11% of the whole run).

Reading:

1. **The I/O layer alone buys nothing measurable here.** giong with
   io_uring is within ±2% of Tokio with quinn-udp on both workloads, and
   epoll is as good as io_uring for bulk. Per KiB, roughly half of the
   server's CPU is the kernel's UDP path (on loopback that includes part of
   the receiver's work, which a NIC would take), half is quinn-proto.
2. **GSO and GRO are not optional**: without them giong spends 3x the CPU
   per byte. The offload prototype on `UdpSocket` closes the gap fully.
3. **quinn's architecture costs about 19% per small request** (task per
   connection and stream, a mutex per connection, channels): the same
   quinn-proto driven inline on one thread needs 4.9-5.0 µs instead of
   6.1 µs, on Tokio or giong alike. giong-quic's API keeps that win whatever
   engine runs underneath.
4. **What a native engine can win per byte is bounded**: at most the
   non-crypto part of quinn-proto (about 55% of 580-810 ns, so roughly 25%
   of the server's total CPU per KiB on loopback) plus the copies between
   socket buffers and quinn-proto's buffers. Larger GSO batches (Linux takes
   64 segments; quinn uses 10) and zero-copy sends are levers either engine
   can use. Bare metal decides how large the kernel share really is.

The bet in section 11 of the plan therefore stands or falls on bare metal
and on the CPU per byte of a native engine, not on io_uring against epoll.
The feasibility guard (quinn-proto behind the same API if the native engine
misses the checkpoint) is the right shape: users get the 19% architecture
win and GSO/GRO either way.

## 2. Architecture

- **One endpoint per worker.** Each worker binds its own `UdpSocket` on the
  same address (giong shares the port with `SO_REUSEPORT`); the kernel
  spreads clients over workers by their address. A connection lives on one
  worker; its state is in an `Rc<RefCell<..>>`; there is no lock, channel or
  atomic on the path.
- **Routing after migration.** When a client's address changes (NAT
  rebinding, Wi-Fi to cellular), its packets may hash to another worker.
  Every connection ID a giong server issues names its worker
  (`native::cid`, QUIC-LB style: 8 bytes, worker index in bytes 1-2). Step
  one: the receiving worker forwards the datagram to the owner over a
  cross-worker queue (`Handle::spawn_on`-like, measured). Step two, only
  with a benchmark: a `SO_REUSEPORT` eBPF program reads the same bytes and
  picks the socket directly. The main risk (plan, section 5): eBPF needs
  `CAP_BPF`/`CAP_SYS_ADMIN` to attach and is unavailable in some containers,
  so forwarding stays as the fallback, and Initial packets (whose DCID the
  client picked) keep the address hash.
- **Encryption in place.** The native engine writes frames straight into
  the send buffer of a GSO batch, then seals each packet there
  (`native::packet::write_long` + `seal`): no copy between a protocol buffer
  and a socket buffer. Received packets are opened inside the receive buffer
  (`native::packet::open`); stream data handed to the user is a slice of
  that buffer, not a copy.
- **Batched sends.** One `send_segments_to` (GSO) per flight of up to 64
  datagrams to one peer; several flights per io_uring submission.
- **Crypto stays with rustls and ring**: TLS 1.3 handshake through
  `rustls::quic::Connection`, packet and header keys through
  `rustls::quic::{PacketKey, HeaderProtectionKey}`. Nothing is written by
  hand there.

## 3. The API swap

The public API (`Endpoint`, `Connection`, `SendStream`, `RecvStream`,
`ServerConfig`, `ClientConfig`, `TransportConfig`) names no quinn-proto
type; configuration takes rustls configs directly; errors are giong error
codes `GIONG-E3601`..`E3604`. `src/proto.rs` is the only file that knows
quinn-proto. The native engine will implement the same internal surface
(`bind`, `accept`, `connect`, `stream`, `read`, `write`, `finish`, `stop`,
`close`) and replace it by a cargo feature first, then by default once it
passes the gates below.

Open API questions for HTTP/3: reads that hand out a slice of the receive
buffer (`RecvBuf`-like) instead of `Bytes`; writes that take an `IoBuf` by
value; unidirectional streams and datagrams (RFC 9221).

## 4. The referee and the checkpoint

The QUIC interop runner (github.com/quic-interop/quic-interop-runner)
against quinn, quiche, ngtcp2 and msquic. It needs Docker and an endpoint
image (`run_endpoint.sh` with the runner's test cases as `TESTCASE`), which
this cloud container cannot run; it goes in a CI job on the branch. Cases in
order: handshake, transfer, retry, resumption, http3, multiconnect,
chacha20, keyupdate, then rebind-port, rebind-addr, connectionmigration.

Checkpoint (plan, section 11): about four weeks of work on the native
engine. If by then it does not pass handshake, transfer, retry, resumption
and http3 against the four implementations and match quinn-proto in this
benchmark, HTTP/3 ships on quinn-proto behind this API, and the native
engine follows in a later 1.x.

Other oracles: RFC test vectors (RFC 9000 appendix A, RFC 9001 appendix A,
RFC 9369 appendix A), differential fuzzing of packet parsing against
quinn-proto, h3spec for HTTP/3, the simulation driver for loss, reordering
and migration under seeds.

## 5. Status

Done on the branch:

- `UdpSocket` offloads behind `giong`'s `unstable-udp-offload` feature:
  `send_segments_to` (GSO + ECN), `set_gro` with `Datagram::segment_size` /
  `segments`, `set_recv_ecn` with `Datagram::ecn`, `gso_supported`; io_uring
  and epoll; tests in `tests/udp_offload.rs`; codes `GIONG-E0110`/`E0111`.
- giong-quic API on quinn-proto: own client and server, quinn interop test,
  GRO/GSO/ECN used when available.
- Native: varints (RFC 9000 A.1 examples), packet numbers (A.2, A.3),
  headers for v1 and v2, in-place seal/open checked byte for byte against
  the RFC 9369 server Initial, worker-routable connection IDs; fuzz target
  `fuzz/packet` and a random smoke test.

Next, in order: frames (ACK, CRYPTO, STREAM, flow control) with fuzz
targets; the handshake through `rustls::quic::Connection`; loss recovery
and NewReno then CUBIC (RFC 9002); the interop endpoint image; then
forwarding between workers and the eBPF steering measurement.

Not decided: giong-quic's MSRV. quinn-proto 0.11.19 needs Rust 1.85
(giong's libraries: 1.80); either pin an older quinn-proto for the fallback
or give giong-quic its own MSRV.
