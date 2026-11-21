#!/usr/bin/env bash
# Where balanced listeners put new connections when the workers are busy in
# uneven ways (bench/placement, 2 workers).
#
#   bench/placement.sh [rounds] [seconds-per-measurement]
#
# Method
#   * The server is bench/placement on SERVER_CPUS, two workers, one per
#     core. A request is an 8-byte frame asking for some microseconds of CPU
#     work, done on the worker that owns the connection; the reply says
#     which worker that was. The client runs on LOAD_CPUS.
#   * skew: one hot connection asks for 200 us of work back to back, which
#     keeps its worker about 97% busy; then 300 idle connections arrive one
#     per millisecond; then 4 probe threads each open a connection, make 20
#     requests of 20 us, close it and start again, for the measured
#     seconds. By counts of tasks the hot worker looks least busy (it has
#     one connection, the other has 300), so the probes go there; by how
#     busy the workers have been they go to the other one. Run back to back
#     ("closed") and paced at one request per 250 us per probe ("paced",
#     16,000 requests per second in all).
#   * mixed: 24 connections arrive 10 ms apart in a random order (seeded
#     per round), 6 hot (100 us of work back to back) and 18 light (20 us
#     every 20 ms). Reported: hot requests per second (work done) and the
#     light requests' latency.
#   * Modes: "plain" (TcpListener::bind, the kernel's hash decides) and
#     "balanced" (TcpListener::bind_balanced) of this checkout; with
#     BEFORE=<path of another giong checkout>, also "before": balanced
#     listeners of that checkout, with this bench copied into it.
#
# Requires: cargo. Results: bench/results/placement/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-4}
SERVER_CPUS=${SERVER_CPUS:-0,1}
LOAD_CPUS=${LOAD_CPUS:-2,3}
PORT=${PORT:-3077}
OUT=${OUT:-results/placement}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd placement && cargo build --release -q)
declare -A BIN=([plain]=placement/target/release/bench-placement [balanced]=placement/target/release/bench-placement)
MODES="plain balanced"
if [ -n "${BEFORE:-}" ]; then
  mkdir -p "$BEFORE/bench/placement"
  cp -r placement/Cargo.toml placement/src "$BEFORE/bench/placement/"
  (cd "$BEFORE/bench/placement" && cargo build --release -q)
  BIN[before]="$BEFORE/bench/placement/target/release/bench-placement"
  MODES="plain before balanced"
fi

PID=
cleanup() { if [ -n "$PID" ]; then kill -TERM "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; fi; PID=; }
trap cleanup EXIT

# One measurement: prints the client's first line (key=value pairs).
measure() { # mode, client args...
  local mode=$1; shift
  if [ "$mode" = plain ]; then export PLAIN=1; else unset PLAIN; fi
  taskset -c "$SERVER_CPUS" "${BIN[$mode]}" server 2 "$PORT" 2>>"$OUT/server.log" &
  PID=$!
  sleep 0.5
  local out
  out=$(taskset -c "$LOAD_CPUS" "${BIN[$mode]}" "$@")
  cleanup
  echo "$out" >> "$OUT/client.log"
  echo "$out" | head -1
}

echo "case,mode,round,metrics" > "$CSV"
: > "$OUT/client.log"; : > "$OUT/server.log"
for round in $(seq 1 "$ROUNDS"); do
  for mode in $MODES; do
    for case in closed paced; do
      gap=0; [ "$case" = paced ] && gap=250
      echo "== skew-$case $mode round $round" >> "$OUT/client.log"
      echo "skew-$case,$mode,$round,\"$(measure "$mode" skew "$PORT" "$SECS" "$gap")\"" >> "$CSV"
    done
    echo "== mixed $mode round $round" >> "$OUT/client.log"
    echo "mixed,$mode,$round,\"$(measure "$mode" mixed "$PORT" "$SECS" "$round")\"" >> "$CSV"
  done
done

python3 - "$CSV" "$ROUNDS" "$SECS" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
path, rounds, secs = sys.argv[1:]
rows = list(csv.DictReader(open(path)))
def kv(s): return {k: float(v) for k, v in (p.split("=") for p in s.split())}
print(f"bench/placement.sh: 2 workers, {rounds} rounds of {secs} s, medians of the rounds")
cases = {"skew-closed": ["probe_rps", "p50_us", "p99_us", "p999_us", "hot_rps"],
         "skew-paced": ["probe_rps", "p50_us", "p99_us", "p999_us", "hot_rps"],
         "mixed": ["hot_rps", "light_p50_us", "light_p99_us", "light_max_us"]}
modes = [m for m in ("plain", "before", "balanced") if any(r["mode"] == m for r in rows)]
for case, keys in cases.items():
    print(f"\n{case}")
    print(f"{'mode':<10}" + "".join(f"{k:>14}" for k in keys))
    for m in modes:
        vals = [kv(r["metrics"]) for r in rows if r["case"] == case and r["mode"] == m]
        print(f"{m:<10}" + "".join(f"{statistics.median(v[k] for v in vals):14.0f}" for k in keys))
PY
