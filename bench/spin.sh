#!/usr/bin/env bash
# Spin before sleeping: what it costs in CPU and what it buys in latency,
# from idle to busy, with the spin of this tree and with the spin of an
# earlier one (BASE_REF, default origin/main: before the spin governor, a
# worker spun the whole duration every time).
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
#   * Modes: `none` (no spin: sleep at once), `tuned` (this tree's
#     spin_before_park(SPIN_US)), `fixed` (the same call built from BASE_REF).
#   * CLIENT=blocking (default) reads with blocking reads, so a request's
#     latency includes the client thread's own wake-up; CLIENT=busy polls a
#     non-blocking socket, so what is left is the server's. On a VM the two
#     differ (see bench/results/spin/summary.txt).
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
MODES=${MODES:-"none fixed tuned"}
BASE_REF=${BASE_REF:-origin/main}
CLIENT=${CLIENT:-blocking}
CASES=${CASES:-"1:0 1:100 1:1000 1:2000 1:5000 1:20000 50:20000 50:50000"}
TICKS=$(getconf CLK_TCK)
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd giong && cargo build --release -q)
(cd spin && cargo build --release -q)
SERVER=giong/target/release/bench-giong
LOAD=spin/target/release/spin-load
# The same server built from BASE_REF, for `fixed`.
BASE_DIR=$(mktemp -d)
git worktree add -q --detach "$BASE_DIR" "$BASE_REF"
trap 'git worktree remove --force "$BASE_DIR"' EXIT
(cd "$BASE_DIR/bench/giong" && CARGO_TARGET_DIR="$BASE_DIR/target" cargo build --release -q)
BASE_SERVER="$BASE_DIR/target/release/bench-giong"
BUSY=0
[ "$CLIENT" = busy ] && BUSY=1

cpu_ticks() { cat /proc/"$1"/task/*/stat 2>/dev/null | awk '{t+=$14+$15} END{print t+0}'; }

{
    echo "# $(date -u +%FT%TZ) $(uname -r) $(nproc) CPUs: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); server CPU 0, 1 worker; spin ${SPIN_US} us; fixed = $BASE_REF ($(git rev-parse --short "$BASE_REF")); ${CLIENT} client; ${SECS} s per case"
    echo "round,client,mode,conns,rate,sent,ok,err,rps,p50_us,p99_us,p999_us,max_us,cpu_pct"
} > "$CSV"

for round in $(seq 1 "$ROUNDS"); do
for mode in $MODES; do
    case $mode in
        none) env_line="" server=$SERVER ;;
        tuned) env_line="GIONG_SPIN_US=$SPIN_US" server=$SERVER ;;
        fixed) env_line="GIONG_SPIN_US=$SPIN_US" server=$BASE_SERVER ;;
    esac
    env $env_line taskset -c 0 "$server" >/dev/null 2>&1 &
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
            line=$(SPIN_LOAD_BUSY=$BUSY taskset -c 1-$(($(nproc) - 1)) "$LOAD" 127.0.0.1:3006 "$SECS" "$rate" "$conns")
        fi
        t1=$(cpu_ticks $pid); w1=$(date +%s.%N)
        cpu=$(awk -v t=$((t1 - t0)) -v hz="$TICKS" -v w0="$w0" -v w1="$w1" 'BEGIN{printf "%.1f", 100*t/hz/(w1-w0)}')
        echo "$round,$CLIENT,$mode,$conns,$rate,$line,$cpu" | tee -a "$CSV" >&2
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
    groups.setdefault((r['client'], int(r['conns']), int(r['rate']), r['mode']), []).append(r)
med = statistics.median
order = {'none': 0, 'fixed': 1, 'tuned': 2}
print(f"{'client':<9}{'conns':>6}{'req/s':>8}  {'mode':<7}{'CPU %':>7}{'p50 us':>8}{'p99 us':>8}{'p99.9 us':>9}{'served/s':>10}")
for k in sorted(groups, key=lambda k: (k[0], k[1], k[2], order.get(k[3], 9))):
    g = groups[k]
    f = lambda c: med(float(r[c]) for r in g)
    print(f"{k[0]:<9}{k[1]:>6}{k[2]:>8}  {k[3]:<7}{f('cpu_pct'):7.1f}{f('p50_us'):8.0f}{f('p99_us'):8.0f}{f('p999_us'):9.0f}{f('rps'):10.0f}")
print("medians over rounds; CPU = the server's user + kernel time as a share of one core")
PY
