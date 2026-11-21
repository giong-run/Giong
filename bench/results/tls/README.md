# bench/tls.sh results

`raw.csv` and `summary.txt` are the latest session (2026-11-17, 4-vCPU cloud VM, Xeon at
2.1 GHz, Linux 6.18, 7 interleaved rounds of 5 s), the one the READMEs quote. It ran
`SERVERS="axum-tls axum-tls-aes128 giong-http-tls-0.8 giong-http-tls" ENDPOINTS="json users
handshake bytes64k"` with one extra server: `giong-http-tls-0.8` is `bench/giong-http` built
from main at `8606db1` (giong-tls 0.8.0), run the same way as `giong-http-tls` (this
branch), so the two are compared within one session.

What changed between the two giong rows (giong-tls, see CHANGELOG):

- A TLS 1.3 server's session tickets wait for its first response instead of going out
  alone, `close_notify` shares the FIN's segment, and the head and body of one write share
  one send. Per `Connection: close` request, both sides together sent 16.5 TCP segments
  before and 14.4 after (axum-tls: 14.0), counted from `/proc/net/snmp`.
- `TlsAcceptor::with_certificates` picks AES-128-GCM first. wrk (OpenSSL) lists AES-256-GCM first,
  whose TLS 1.3 suite hashes with SHA-384: about a sixth of a resumed handshake's CPU time
  in `perf`. `axum-tls-aes128` is axum-tls configured the same way, to show how much of the
  handshake rate that choice is: almost all of it. giong keeps its lower latency
  (p50 1.70 ms against 2.25, p99 4.22 against 6.19).

wrk resumes the previous connection's session, so `handshake` measures resumed TLS 1.3
handshakes (X25519 and the key schedule, no certificate signature).

`2026-11-12/` is the session the READMEs quoted before (giong at `e269341`, 5 rounds,
TLS 1.3 with AES-256-GCM on every server).
