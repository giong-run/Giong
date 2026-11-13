#!/usr/bin/env bash
# Balanced listeners against plain SO_REUSEPORT listeners, with few
# connections and a CPU-heavy route, on giong-http's benchmark server.
#
#   bench/balance.sh [rounds] [seconds-per-measurement]
#
# Method
#   * The server is bench/giong-http on SERVER_CPUS (one worker per core),
#     started once with plain listeners (TcpListener::bind) and once with
#     balanced ones (BALANCED=1: TcpListener::bind_balanced).
#   * wrk on LOAD_CPU opens CONNS connections (6 by default, two per worker)
#     and asks for /compute?n=N (fib(N), 22 by default) back to back. Each
#     round is a new wrk run, so the kernel spreads a new set of connections:
#     with plain listeners the spread is a hash of the addresses and changes
#     from round to round, which is the point.
#   * Prints, per mode, the median, lowest and highest requests per second of
#     the rounds and the median p99 latency.
#
# Requires: cargo, wrk. Results: bench/results/balance/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-10}
SECS=${2:-4}
SERVER_CPUS=${SERVER_CPUS:-0,1,2}
LOAD_CPU=${LOAD_CPU:-3}
CONNS=${CONNS:-6}
N=${N:-22}
OUT=${OUT:-results/balance}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd giong-http && cargo build --release -q)
SERVER=giong-http/target/release/bench-giong-http

PID=
cleanup() { if [ -n "$PID" ]; then kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; fi; }
trap cleanup EXIT

echo "mode,round,rps,p99_us" > "$CSV"
for mode in plain balanced; do
  if [ "$mode" = balanced ]; then export BALANCED=1; else unset BALANCED; fi
  taskset -c "$SERVER_CPUS" "$SERVER" >/dev/null 2>&1 &
  PID=$!
  sleep 1
  for round in $(seq 1 "$ROUNDS"); do
    out=$(taskset -c "$LOAD_CPU" wrk -t1 -c"$CONNS" -d"${SECS}s" --latency \
      "http://127.0.0.1:3009/compute?n=$N" 2>&1)
    rps=$(echo "$out" | awk '/Requests\/sec/ {print $2}')
    p99=$(echo "$out" | awk '/ 99%/ {v=$2; if (v ~ /ms$/) {sub(/ms$/,"",v); v*=1000} else if (v ~ /us$/) {sub(/us$/,"",v)} else if (v ~ /s$/) {sub(/s$/,"",v); v*=1000000}; print v}')
    echo "$mode,$round,$rps,$p99" >> "$CSV"
  done
  cleanup
  PID=
done

python3 - "$CSV" "$CONNS" "$N" "$ROUNDS" "$SECS" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
path, conns, n, rounds, secs = sys.argv[1:]
rows = list(csv.DictReader(open(path)))
print(f"{conns} connections, /compute?n={n}, {rounds} rounds of {secs} s")
print(f"{'mode':<9} {'req/s median':>13} {'min':>9} {'max':>9} {'p99 us median':>14}")
for mode in ("plain", "balanced"):
    r = [float(x["rps"]) for x in rows if x["mode"] == mode]
    p = [float(x["p99_us"]) for x in rows if x["mode"] == mode]
    print(f"{mode:<9} {statistics.median(r):13.0f} {min(r):9.0f} {max(r):9.0f} {statistics.median(p):14.0f}")
PY
