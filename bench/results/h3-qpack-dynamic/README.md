# HTTP/3 QPACK dynamic table (server decoder)

`Server::qpack_table_size(4096)` against the default (no table), with aioquic 1.2.0 as the
client (its encoder, pylsqpack 0.3.24, uses a table when the server allows one). Each request is
`GET /` with a 63-byte `user-agent`, a 207-byte `cookie` and a 64-byte `x-trace`, all on one
connection, as a browser repeats them.

| measure | no table | table 4096 | change |
|---|---:|---:|---:|
| request field sections + encoder stream, 50 requests | 12,350 B | 1,168 B | **−90.5%** |
| server instructions per request (callgrind, round 1) | 58,773 | 50,970 | −13.3% |
| server instructions per request (callgrind, round 2) | 58,333 | 50,907 | −12.7% |

Instructions per request = (Ir after 600 requests − Ir after 100) / 500, so start-up and the
certificate made at start cancel out. Fewer bytes to receive and decrypt, and references instead
of Huffman-coded literals to decode, outweigh the table work and the 67 bytes the server's
decoder stream sends.

Costs, stated: a connection whose client uses the table holds up to the configured bytes of
table (counted as RFC 9204 counts them), so the setting stays off by default. Measured on a
4-vCPU cloud VM, Linux 6.18, loopback; raw numbers in `raw.txt`.

Reproduce: `pip install aioquic==1.2.0`, `cargo build --release -p giong-http --features
unstable-http3 --example h3`, then `./callgrind.sh <table> <n> <port>` for table 0 and 4096, n
100 and 600 (`PYTHON` names the interpreter with aioquic). `h3req.py localhost <port> 50`
against `GIONG_QPACK_TABLE=<table> target/release/examples/h3 127.0.0.1:<port>` prints the
byte counters.
