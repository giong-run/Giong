#!/usr/bin/env bash
# Spin before sleeping: what it costs in CPU and what it buys in latency,
# fixed (`Builder::spin_before_park`) and adaptive, from idle to busy.
#
#   bench/spin.sh [rounds] [seconds]
#
# Method
#   * bench/giong (`GET /json`) on CPU 0 with one worker; the load
#     (spin/src/bin/spin-load.rs) on the other CPUs.
#   * Each case sends RATE requests/s open loop over CONNS keep-alive
#     connections. "CPU" is the server's user plus kernel time over the run,
#     as a share of one core, read from /proc; latency is per request, from
#     its sending (or its scheduled time when its connection was still busy).
#   * Modes: `none` (default: sleep at once), `fixed` (spin_before_park of
#     SPIN_US), `adaptive` (spin up to SPIN_US, tuned per worker).
#   * Cases (conns:rate): idle (one silent connection), a trickle, a steady
#     single client, and many clients up to busy.
#
# Requires: cargo, python3, taskset. Results: bench/results/spin/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-3}
SECS=${2:-10}
OUT=${OUT:-results/spin}
SPIN_US=${SPIN_US:-200}
MODES=${MODES:-"none fixed adaptive"}
CASES=${CASES:-"1:0 1:100 1:1000 1:2000 1:5000 1:20000 50:20000 50:50000"}
TICKS=$(getconf CLK_TCK)
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd giong && cargo build --release -q)
(cd spin && cargo build --release -q)
SERVER=giong/target/release/bench-giong
LOAD=spin/target/release/spin-load

cpu_ticks() { cat /proc/"$1"/task/*/stat 2>/dev/null | awk '{t+=$14+$15} END{print t+0}'; }

{
    echo "# $(date -u +%FT%TZ) $(uname -r) $(nproc) CPUs: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); server CPU 0, 1 worker; spin ${SPIN_US} us; ${SECS} s per case"
    echo "round,mode,conns,rate,sent,ok,err,rps,p50_us,p99_us,p999_us,max_us,cpu_pct"
} > "$CSV"

for round in $(seq 1 "$ROUNDS"); do
for mode in $MODES; do
    case $mode in
        none) env_line="" ;;
        fixed) env_line="GIONG_SPIN_US=$SPIN_US" ;;
        adaptive) env_line="GIONG_SPIN_ADAPTIVE_US=$SPIN_US" ;;
    esac
    env $env_line taskset -c 0 "$SERVER" >/dev/null 2>&1 &
    pid=$!
    for _ in $(seq 100); do
        (exec 3<>/dev/tcp/127.0.0.1/3006) 2>/dev/null && break
        sleep 0.05
    done
    for case in $CASES; do
        conns=${case%%:*}; rate=${case##*:}
        t0=$(cpu_ticks $pid); w0=$(date +%s.%N)
        if [ "$rate" = 0 ]; then
            # Idle: one connection that sends nothing.
            (exec 3<>/dev/tcp/127.0.0.1/3006; sleep "$SECS") 
            line="0,0,0,0,0,0,0,0"
        else
            line=$(taskset -c 1-$(($(nproc) - 1)) "$LOAD" 127.0.0.1:3006 "$SECS" "$rate" "$conns")
        fi
        t1=$(cpu_ticks $pid); w1=$(date +%s.%N)
        cpu=$(awk -v t=$((t1 - t0)) -v hz="$TICKS" -v w0="$w0" -v w1="$w1" 'BEGIN{printf "%.1f", 100*t/hz/(w1-w0)}')
        echo "$round,$mode,$conns,$rate,$line,$cpu" | tee -a "$CSV" >&2
    done
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
    sleep 0.5
done; done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
lines = open(sys.argv[1]).read().splitlines()
print(lines[0])
rows = list(csv.DictReader(l for l in lines if not l.startswith('#')))
groups = {}
for r in rows:
    groups.setdefault((int(r['conns']), int(r['rate']), r['mode']), []).append(r)
med = statistics.median
print(f"{'conns':>6}{'req/s':>8}  {'mode':<10}{'CPU %':>7}{'p50 us':>8}{'p99 us':>8}{'p99.9 us':>9}{'served/s':>10}")
for k in sorted(groups):
    g = groups[k]
    f = lambda c: med(float(r[c]) for r in g)
    print(f"{k[0]:>6}{k[1]:>8}  {k[2]:<10}{f('cpu_pct'):7.1f}{f('p50_us'):8.0f}{f('p99_us'):8.0f}{f('p999_us'):9.0f}{f('rps'):10.0f}")
print("medians over rounds; CPU = the server's user + kernel time as a share of one core")
PY
