#!/usr/bin/env bash
# HTTP/2 benchmark: giong-http against axum (hyper's HTTP/2), for request
# throughput and for memory per connection and per stream.
#
#   bench/h2.sh [rounds] [seconds-per-measurement]
#
# Servers (the same routes as everywhere in bench/):
#   giong-http-tls  bench/giong-http with TLS_CERT and TLS_KEY: HTTPS, HTTP/2
#                   chosen through ALPN (port 3010)
#   axum-tls        bench/axum-tls: axum-server with rustls, likewise (3011)
#   giong-http      bench/giong-http without TLS: HTTP/2 with prior knowledge
#                   (3009)
#   axum            bench/axum built with its `h2` feature, likewise (3003)
# Both TLS servers use rustls with the `ring` provider. Both axum servers
# set TCP_NODELAY (TCP_NODELAY=1), as giong does: without it, the first
# answer on a new HTTP/2 connection waits about 40 ms for a delayed ACK.
#
# Throughput, with h2load (nghttp2):
#   * One core for the server (SERVER_CPU), three for h2load (LOAD_CPU).
#   * `json` (a 27-byte body) and `users` (8.5 KB): CONNS connections with
#     STREAMS concurrent streams each.
#   * `json-mux`: few connections (MUX_CONNS) with many streams each
#     (MUX_STREAMS), the way a browser loads a page through one connection.
#   * `cpu%` is how busy the server's core was. Well below 100 means the
#     load generator was the limit, not the server.
#   * h2load reports the mean time per request, not percentiles.
#
# Memory, with bench/hold over HTTP/2 with prior knowledge (plain servers,
# one worker thread each, MEM_CONNS connections; see bench/memory.sh):
#   h2-idle      one request answered on each connection, then quiet
#   h2-pending   one request on each connection, still being handled
#                (GET /wait?ms=600000)
#   per stream   MEM_STREAMS such requests on each connection: the memory
#                beyond h2-pending, per extra stream
# Reported in KiB, the median of all rounds.
#
# Requires: h2load, curl (with HTTP/2), openssl, cargo, python3.
# Results: bench/results/h2/ (or $OUT).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
SERVER_CPU=${SERVER_CPU:-3}
LOAD_CPU=${LOAD_CPU:-0,1,2}
LOAD_THREADS=${LOAD_THREADS:-3}
CONNS=${CONNS:-48}
STREAMS=${STREAMS:-10}
MUX_CONNS=${MUX_CONNS:-3}
MUX_STREAMS=${MUX_STREAMS:-100}
MEM_CONNS=${MEM_CONNS:-5000}
MEM_STREAMS=${MEM_STREAMS:-10}
OUT=${OUT:-results/h2}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
MEM_CSV="$OUT/memory.csv"

echo "building..." >&2
(cd giong-http && cargo build --release -q)
(cd axum-tls && cargo build --release -q)
(cd axum && cargo build --release -q --features h2 --target-dir target/h2)
(cd hold && cargo build --release -q)

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 2 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost" 2>/dev/null
export TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem"

NAMES=(giong-http-tls axum-tls giong-http axum)
declare -A CMD=(
  [giong-http-tls]="giong-http/target/release/bench-giong-http"
  [axum-tls]="env TCP_NODELAY=1 axum-tls/target/release/bench-axum-tls"
  [giong-http]="env -u TLS_CERT giong-http/target/release/bench-giong-http"
  [axum]="env TCP_NODELAY=1 axum/target/h2/release/bench-axum"
)
declare -A URL=(
  [giong-http-tls]="https://127.0.0.1:3010"
  [axum-tls]="https://127.0.0.1:3011"
  [giong-http]="http://127.0.0.1:3009"
  [axum]="http://127.0.0.1:3003"
)

ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
HZ=$(getconf CLK_TCK)
rss_kb() { awk '/^VmRSS/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }

# Starts server $1 with $2 worker threads on the CPUs $3; sets PID.
start() {
  GIONG_WORKERS=$2 TOKIO_WORKER_THREADS=$2 taskset -c "$3" ${CMD[$1]} >/dev/null 2>&1 &
  PID=$!
  for _ in $(seq 100); do
    curl -sk --http2-prior-knowledge -o /dev/null "${URL[$1]}/json" 2>/dev/null && break
    curl -sk --http2 -o /dev/null "${URL[$1]}/json" 2>/dev/null && break
    sleep 0.1
  done
}

stop() {
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  sleep 0.5
}

# ------------------------------------------------------------- throughput

echo "server,endpoint,round,rps,mean_ms,cpu,failed" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for s in "${NAMES[@]}"; do
    start "$s" 1 "$SERVER_CPU"
    for ep in json users json-mux; do
      path=/${ep%-mux}
      if [ "$ep" = json-mux ]; then c=$MUX_CONNS m=$MUX_STREAMS; else c=$CONNS m=$STREAMS; fi
      args=(-t"$LOAD_THREADS" -c"$c" -m"$m" "${URL[$s]}$path")
      taskset -c "$LOAD_CPU" h2load -D 2 "${args[@]}" >/dev/null 2>&1 || true
      before=$(ticks "$PID")
      out=$(taskset -c "$LOAD_CPU" h2load -D "$SECS" "${args[@]}" 2>&1)
      after=$(ticks "$PID")
      rps=$(awk '/^finished in/ {gsub(",", "", $4); print $4}' <<<"$out")
      mean=$(awk '/^time for request:/ {print $6}' <<<"$out")
      failed=$(awk '/^requests:/ {print $10}' <<<"$out")
      cpu=$(( (after - before) * 100 / (HZ * SECS) ))
      echo "$s,$ep,$round,${rps:-0},${mean:-nan},$cpu,${failed:-nan}" >> "$CSV"
    done
    stop
    echo "round $round/$ROUNDS: $s done" >&2
  done
done

# ----------------------------------------------------------------- memory

limit=$(ulimit -n)
if (( MEM_CONNS + 100 > limit )); then
  echo "ulimit -n is $limit: lower MEM_CONNS or raise the limit" >&2
  exit 1
fi

# KiB per connection of server $1 with bench-hold mode $2 and path $3.
hold() {
  local base held log client
  start "$1" 1 0-3
  for _ in $(seq 200); do
    curl -s --http2-prior-knowledge -o /dev/null "${URL[$1]}/json" || true
  done
  sleep 1
  base=$(rss_kb "$PID")
  log=$(mktemp)
  hold/target/release/bench-hold "${URL[$1]#http://}" "$MEM_CONNS" "$2" "$3" >"$log" 2>&1 &
  client=$!
  for _ in $(seq 600); do
    grep -q ready "$log" && break
    kill -0 "$client" 2>/dev/null || break
    sleep 0.1
  done
  if ! grep -q ready "$log"; then
    echo "bench-hold failed against $1: $(cat "$log")" >&2
    echo nan
  else
    sleep 2
    held=$(rss_kb "$PID")
    python3 -c "print(f'{($held - $base) / $MEM_CONNS:.2f}')"
  fi
  kill "$client" 2>/dev/null || true
  wait "$client" 2>/dev/null || true
  rm -f "$log"
  stop
}

echo "server,round,idle,pending,pending_many" > "$MEM_CSV"
for round in $(seq 1 "$ROUNDS"); do
  for s in giong-http axum; do
    idle=$(hold "$s" h2-idle /json)
    pending=$(hold "$s" h2-pending:1 "/wait?ms=600000")
    many=$(hold "$s" "h2-pending:$MEM_STREAMS" "/wait?ms=600000")
    echo "$s,$round,$idle,$pending,$many" >> "$MEM_CSV"
    echo "memory round $round/$ROUNDS: $s done" >&2
  done
done

python3 - "$CSV" "$MEM_CSV" "$CONNS" "$STREAMS" "$MUX_CONNS" "$MUX_STREAMS" "$MEM_CONNS" "$MEM_STREAMS" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
path, mem_path, conns, streams, mux_conns, mux_streams, mem_conns, mem_streams = sys.argv[1:]
med = statistics.median

def to_ms(v):
    for unit, k in (("us", 1e-3), ("ms", 1.0), ("s", 1e3)):
        if v.endswith(unit):
            return float(v[:-len(unit)]) * k
    return float("nan")

rows = list(csv.DictReader(open(path)))
servers = list(dict.fromkeys(r["server"] for r in rows))
endpoints = list(dict.fromkeys(r["endpoint"] for r in rows))
print(f"Throughput: h2load, json and users with {conns} connections x {streams} streams,")
print(f"json-mux with {mux_conns} connections x {mux_streams} streams; one server core.")
print()
print(f"{'endpoint':<10}{'server':<16}{'req/s median':>13}{'min':>10}{'max':>10}{'mean ms':>9}{'cpu%':>6}{'failed':>8}")
for ep in endpoints:
    for s in servers:
        sel = [r for r in rows if r["server"] == s and r["endpoint"] == ep]
        if not sel: continue
        rps = [float(r["rps"]) for r in sel]
        means = [to_ms(r["mean_ms"]) for r in sel]
        failed = sum(int(r["failed"]) for r in sel if r["failed"].isdigit())
        print(f"{ep:<10}{s:<16}{med(rps):13.0f}{min(rps):10.0f}{max(rps):10.0f}{med(means):9.2f}"
              f"{med(int(r['cpu']) for r in sel):6.0f}{failed:8d}")
    print()

mem = list(csv.DictReader(open(mem_path)))
print(f"Memory: {mem_conns} connections over HTTP/2 with prior knowledge, one worker thread.")
print()
print(f"{'server':<16}{'h2-idle':>9}{'h2-pending':>12}{'per stream':>12}   KiB")
for s in dict.fromkeys(r["server"] for r in mem):
    sel = [r for r in mem if r["server"] == s and "nan" not in r.values()]
    if not sel:
        print(f"{s:<16}{'-':>9}{'-':>12}{'-':>12}")
        continue
    idle = med(float(r["idle"]) for r in sel)
    pending = med(float(r["pending"]) for r in sel)
    per_stream = med((float(r["pending_many"]) - float(r["pending"])) / (int(mem_streams) - 1) for r in sel)
    print(f"{s:<16}{idle:9.2f}{pending:12.2f}{per_stream:12.2f}")
PY
