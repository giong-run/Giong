#!/usr/bin/env bash
# CPU instructions giong-http's server runs per request, counted by callgrind.
#
#   bench/instructions.sh [requests]
#
#   ENDPOINTS="json-tls json" BINS="/tmp/before /tmp/after" bench/instructions.sh 20000
#
# Wall-clock rates move by a few percent from run to run, and by far more on
# a shared or virtual machine, so a change of 1 or 2% per request cannot be
# seen with wrk there. An instruction count can: two runs of the same binary
# agree within about 0.1%. It leaves out cache misses and the kernel's share,
# so it complements bench/tls.sh and bench/run.sh rather than replacing them.
#
# For each binary (BINS, default: bench/giong-http built here) and endpoint:
# the server runs under callgrind with one worker; h2load sends a warm-up of
# 2,000 keep-alive requests on 10 connections, the counters are zeroed, then
# the measured requests go, and the count is divided by them. The 10
# connections are opened once per batch, so their handshakes are not counted.
#
# Endpoints:
#   json-tls  GET /json over HTTPS (rustls; kernel TLS off), HTTP/1.1
#   json      GET /json over plain HTTP/1.1
#   json-h2   GET /json over HTTP/2 with prior knowledge
#
# To compare two commits, build bench/giong-http at each, copy the binaries
# aside and pass both in BINS: the counts are only comparable between builds
# made with the same compiler.
#
# Requires: valgrind, h2load (nghttp2-client), openssl, curl, python3.
# Results: bench/results/instructions/summary.txt
set -euo pipefail
cd "$(dirname "$0")"

N=${1:-20000}
OUT=${OUT:-results/instructions}
read -r -a EPS <<<"${ENDPOINTS:-json-tls json json-h2}"
mkdir -p "$OUT"

if [ -z "${BINS:-}" ]; then
  echo "building..." >&2
  (cd giong-http && cargo build --release -q)
  BINS="giong-http/target/release/bench-giong-http"
fi
read -r -a BIN_LIST <<<"$BINS"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 2 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost" 2>/dev/null

declare -A URL=(
  [json-tls]="https://127.0.0.1:3010/json"
  [json]="http://127.0.0.1:3009/json"
  [json-h2]="http://127.0.0.1:3009/json"
)
declare -A FLAGS=([json-tls]="--h1" [json]="--h1" [json-h2]="")
for ep in "${EPS[@]}"; do
  [ -n "${URL[$ep]:-}" ] || { echo "unknown endpoint $ep (known: ${!URL[*]})" >&2; exit 2; }
done

echo "binary,endpoint,requests,instructions_per_request" > "$OUT/raw.csv"
for bin in "${BIN_LIST[@]}"; do
  for ep in "${EPS[@]}"; do
    env=(GIONG_WORKERS=1 GIONG_KTLS=off)
    [ "$ep" = json-tls ] && env+=(TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem")
    rm -f "$TMP"/cg.*
    env -u TLS_CERT -u TLS_KEY "${env[@]}" valgrind --tool=callgrind \
      --callgrind-out-file="$TMP/cg.%p" "$bin" >/dev/null 2>"$TMP/valgrind.log" &
    pid=$!
    for _ in $(seq 300); do
      curl -sk -o /dev/null "${URL[$ep]}" && break
      sleep 0.2
    done
    # shellcheck disable=SC2086
    h2load ${FLAGS[$ep]} -n 2000 -c 10 "${URL[$ep]}" >/dev/null 2>&1
    callgrind_control -z "$pid" >/dev/null 2>&1
    # shellcheck disable=SC2086
    h2load ${FLAGS[$ep]} -n "$N" -c 10 "${URL[$ep]}" >"$TMP/h2load.log" 2>&1
    callgrind_control -d "$pid" >/dev/null 2>&1
    sleep 1
    ok=$(awk '/^requests:/{print $8}' "$TMP/h2load.log")
    # Under valgrind the server does not stop on SIGTERM.
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    dump=$(ls -S "$TMP"/cg.* | head -1)
    ir=$(awk '/^summary:|^totals:/{print $2; exit}' "$dump")
    if [ "$ok" != "$N" ]; then
      echo "$bin $ep: only $ok of $N requests succeeded" >&2
      exit 1
    fi
    per=$(python3 -c "print(round($ir / $N))")
    echo "$bin,$ep,$N,$per" >> "$OUT/raw.csv"
    echo "$bin $ep: $per instructions per request" >&2
  done
done
awk -F, '{printf "%-50s %-9s %9s %25s\n", $1, $2, $3, $4}' "$OUT/raw.csv" | tee "$OUT/summary.txt"
