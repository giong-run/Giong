#!/usr/bin/env bash
# Memory per idle HTTPS connection of giong-http with rustls in the process,
# from the server's resident memory (what the kernel's socket memory is not
# part of).
#
#   bench/tls-idle.sh [rounds] [connections]
#   BINS="before=/path/to/bench-giong-http after=giong-http/target/release/bench-giong-http" bench/tls-idle.sh
#   PROTOCOL=h2 bench/tls-idle.sh    # HTTP/2 connections (ALPN h2) instead of HTTP/1.1
#
# Method: one worker (GIONG_WORKERS=1) pinned to SERVER_CPU, an hour of idle
# timeout and room for the connections (GIONG_MAX_CONNS); tls-idle/hold.py on
# the other CPUs opens the connections one by one (full TLS 1.3 handshake,
# one keep-alive GET /json each, OpenSSL client) and reports the growth of the
# server's RssAnon and VmRSS per connection. Binaries are measured in
# interleaved rounds, a fresh server each time. Certificate: ECDSA P-256.
#
# Requires: cargo, python3, openssl; `ulimit -n` above the connection count.
# Results: bench/results/tls-idle/ (or $OUT): raw.csv, summary.txt.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-3}
CONNS=${2:-8000}
SERVER_CPU=${SERVER_CPU:-3}
LOAD_CPU=${LOAD_CPU:-0-2}
PROTOCOL=${PROTOCOL:-http/1.1}
OUT=${OUT:-results/tls-idle}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 2 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost" 2>/dev/null

if [ -z "${BINS:-}" ]; then
  (cd giong-http && cargo build --release -q)
  BINS="giong-http=giong-http/target/release/bench-giong-http"
fi
read -r -a PAIRS <<<"$BINS"

echo "binary,round,connections,rss_anon_kib_per_conn,vmrss_kib_per_conn,vmrss_after_mib" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for pair in "${PAIRS[@]}"; do
    name=${pair%%=*}
    bin=${pair#*=}
    env TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem" GIONG_KTLS=off GIONG_WORKERS=1 \
      GIONG_MAX_CONNS=$((CONNS + 1000)) GIONG_IDLE_TIMEOUT_SECS=3600 \
      taskset -c "$SERVER_CPU" "$bin" >/dev/null 2>&1 &
    pid=$!
    sleep 1
    line=$(taskset -c "$LOAD_CPU" python3 -I tls-idle/hold.py 3010 "$CONNS" "$pid" "$PROTOCOL")
    echo "$name,$round,$line" >> "$CSV"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    sleep 1
    echo "round $round/$ROUNDS: $name $line" >&2
  done
done

python3 - "$CSV" "$PROTOCOL" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
names = list(dict.fromkeys(r["binary"] for r in rows))
print(f"{rows[0]['connections']} idle HTTPS connections ({sys.argv[2]}), one worker; median of the rounds")
print(f"{'binary':<16}{'anon KiB/conn':>15}{'RSS KiB/conn':>14}{'RSS after MiB':>15}")
for n in names:
    rs = [r for r in rows if r["binary"] == n]
    med = lambda k: statistics.median(float(r[k]) for r in rs)
    print(f"{n:<16}{med('rss_anon_kib_per_conn'):>15.2f}{med('vmrss_kib_per_conn'):>14.2f}{med('vmrss_after_mib'):>15.1f}")
PY
