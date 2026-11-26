#!/usr/bin/env bash
# Instructions per request of a bench/giong-http build, counted by callgrind.
#
#   bench/results/idle-conn-memory/callgrind.sh <bench-giong-http binary> <path> [requests]
#   bench/results/idle-conn-memory/callgrind.sh <bench-giong-http binary> handshake [connections]
#
# One worker. With a path, plain TCP: 300 keep-alive requests of warm-up,
# counters zeroed, <requests> more (2,000 by default) on the same connection,
# one after another, counters dumped. With `handshake`, TLS (rustls in the
# process, no kernel TLS): every request on a new connection with
# `Connection: close`, each connection offered the session of the one before,
# as `bench/tls.sh`'s handshake case does (so all but the first are resumed
# TLS 1.3 handshakes); 100 connections of warm-up, then <connections> (500 by
# default). Prints the server's instructions per request.
# Requires: valgrind (callgrind_control), curl, openssl, python3.
set -euo pipefail
BIN=$1
WHAT=$2
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cat >"$TMP/client.py" <<'EOF'
import socket, ssl, sys
port, n, path = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
if path == "handshake":
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    ctx.set_alpn_protocols(["http/1.1"])
    session = None
    req = b"GET /json HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    for _ in range(n):
        s = ctx.wrap_socket(socket.create_connection(("127.0.0.1", port)),
                            server_hostname="localhost", session=session)
        s.sendall(req)
        while s.recv(65536):
            pass
        session = s.session
        s.close()
    sys.exit()
s = socket.create_connection(("127.0.0.1", port))
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
req = f"GET {path} HTTP/1.1\r\nHost: x\r\n\r\n".encode()
buf = b""
for _ in range(n):
    s.sendall(req)
    while b"\r\n\r\n" not in buf:
        buf += s.recv(1 << 20)
    head, _, rest = buf.partition(b"\r\n\r\n")
    length = 0
    for line in head.split(b"\r\n")[1:]:
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"content-length":
            length = int(value)
    while len(rest) < length:
        rest += s.recv(1 << 20)
    buf = rest[length:]
EOF

if [ "$WHAT" = handshake ]; then
  N=${3:-500}
  WARM=100
  PORT=3010
  URL=https://127.0.0.1:$PORT/json
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 2 -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost" 2>/dev/null
  ENV=(TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem" GIONG_KTLS=off)
else
  N=${3:-2000}
  WARM=300
  PORT=3009
  URL=http://127.0.0.1:$PORT/json
  ENV=(-u TLS_CERT)
fi

env "${ENV[@]}" GIONG_WORKERS=1 valgrind --tool=callgrind \
  --callgrind-out-file="$TMP/cg.%p" "$BIN" >/dev/null 2>"$TMP/valgrind.log" &
vg=$!
for _ in $(seq 100); do
  curl -sk -o /dev/null "$URL" && break
  sleep 0.5
done
python3 -I "$TMP/client.py" "$PORT" "$WARM" "$WHAT"
callgrind_control -z "$vg" >/dev/null
python3 -I "$TMP/client.py" "$PORT" "$N" "$WHAT"
callgrind_control -d "$vg" >/dev/null
sleep 1
# The server does not stop on a signal under valgrind; the dump is written.
kill -9 "$vg"
wait "$vg" 2>/dev/null || true
dump=$(ls -t "$TMP"/cg.*.1 | head -1)
total=$(grep -m1 '^summary:\|^totals:' "$dump" | awk '{print $2}')
python3 -c "print(f'$WHAT {$total / $N:.1f} instructions per request')"
