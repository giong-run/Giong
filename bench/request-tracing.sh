#!/usr/bin/env bash
# What request tracing costs a giong-web server: requests per second and
# latency with tracing off (the default) and on (TRACE=1, as giong-mcp's
# slow_requests turns it on), in interleaved rounds.
#
#   bench/request-tracing.sh [rounds] [seconds-per-measurement]
#
# The server (bench/giong-web, one worker) is pinned to SERVER_CPU and wrk to
# LOAD_CPU. Routes: /json (the smallest answer) and /users/{id} (a path
# parameter and a JSON body). Raw wrk output and a CSV go to
# results/request-tracing/ (or $OUT).
#
# Requires: wrk, cargo, python3.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-10}
SERVER_CPU=${SERVER_CPU:-1}
LOAD_CPU=${LOAD_CPU:-0}
CONNS=${CONNS:-50}
OUT=${OUT:-results/request-tracing}
mkdir -p "$OUT/wrk"
CSV="$OUT/raw.csv"

(cd giong-web && cargo build --release --quiet)
BIN=giong-web/target/release/bench-giong-web

echo "round,tracing,route,requests_per_sec,p50_us,p99_us,max_us" > "$CSV"
{
    echo "kernel: $(uname -r)"
    echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
    echo "rustc: $(rustc --version)"
    echo "commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "rounds=$ROUNDS secs=$SECS conns=$CONNS server_cpu=$SERVER_CPU load_cpu=$LOAD_CPU"
} > "$OUT/conditions.txt"

us() {
    # wrk prints latencies such as 61.00us, 1.20ms, 1.00s.
    python3 -c '
import sys
v = sys.argv[1]
for unit, k in (("us", 1), ("ms", 1e3), ("s", 1e6)):
    if v.endswith(unit):
        print(round(float(v[: -len(unit)]) * k, 1)); break
' "$1"
}

measure() {
    local round=$1 tracing=$2 route=$3 name=$4
    local env=()
    [[ $tracing == on ]] && env=(TRACE=1)
    env "${env[@]}" taskset -c "$SERVER_CPU" "$BIN" &
    local pid=$!
    for _ in $(seq 50); do
        curl -s -o /dev/null "http://127.0.0.1:3011/json" && break
        sleep 0.1
    done
    # Warm-up, then the measurement.
    taskset -c "$LOAD_CPU" wrk -t1 -c"$CONNS" -d2s "http://127.0.0.1:3011$route" > /dev/null
    local file="$OUT/wrk/r${round}-${tracing}-${name}.txt"
    taskset -c "$LOAD_CPU" wrk -t1 -c"$CONNS" -d"${SECS}s" --latency "http://127.0.0.1:3011$route" > "$file"
    kill -INT "$pid"
    wait "$pid" || true
    local rps p50 p99 max
    rps=$(awk '/Requests\/sec/ {print $2}' "$file")
    p50=$(us "$(awk '$1 == "50%" {print $2}' "$file")")
    p99=$(us "$(awk '$1 == "99%" {print $2}' "$file")")
    max=$(us "$(awk '$1 == "Latency" {print $4; exit}' "$file")")
    echo "$round,$tracing,$route,$rps,$p50,$p99,$max" >> "$CSV"
    echo "round $round tracing $tracing $route: $rps req/s, p50 ${p50} us, p99 ${p99} us"
}

for round in $(seq 1 "$ROUNDS"); do
    for tracing in off on; do
        measure "$round" "$tracing" /json json
        measure "$round" "$tracing" /users/42 user
    done
done

python3 - "$CSV" > "$OUT/summary.txt" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
for route in sorted({r["route"] for r in rows}):
    med = {}
    for t in ("off", "on"):
        rs = [r for r in rows if r["route"] == route and r["tracing"] == t]
        med[t] = {k: statistics.median(float(r[k]) for r in rs) for k in ("requests_per_sec", "p50_us", "p99_us")}
        lo = min(float(r["requests_per_sec"]) for r in rs)
        hi = max(float(r["requests_per_sec"]) for r in rs)
        print(f"{route} tracing {t}: {med[t]['requests_per_sec']:.0f} req/s (min {lo:.0f}, max {hi:.0f}), "
              f"p50 {med[t]['p50_us']:.0f} us, p99 {med[t]['p99_us']:.0f} us")
    d = med["on"]["requests_per_sec"] / med["off"]["requests_per_sec"] - 1
    print(f"{route}: tracing on changes requests per second by {d * 100:+.1f}% (medians)")
PY
cat "$OUT/summary.txt"
