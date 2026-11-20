#!/usr/bin/env bash
# Core-primitive micro-benchmarks: spawn, polls, channels, oneshots, timers
# and cross-thread wake-ups on Gióng and the other Rust runtimes.
#
#   bench/core.sh [rounds] [out-dir]
#
# Every run is pinned to two cores (CORES, default 1-2): one for the runtime
# and one for the feeding thread of `xthread` (and Tokio's extra workers).
# Prints and writes raw.csv (one line per run) and summary.txt (median,
# min and max ns per operation, wall clock and CPU).
set -euo pipefail
cd "$(dirname "$0")/core"
ROUNDS=${1:-5}
OUT=${2:-../results/core/vm}
CORES=${CORES:-1-2}
RUNTIMES=${RUNTIMES:-"giong tokio-ct tokio-local tokio-mt monoio glommio compio smol"}
WORKLOADS=${WORKLOADS:-"spawn yield pingpong mpsc bounded oneshot chain timers timeout xthread tcp-rr connect lookup"}
cargo build --release --quiet
mkdir -p "$OUT"
echo "runtime,workload,ops,wall_ns,cpu_ns" >"$OUT/raw.csv"
for round in $(seq "$ROUNDS"); do
    for w in $WORKLOADS; do
        for r in $RUNTIMES; do
            # monoio, glommio and compio have no resolver of their own.
            if [ "$w" = lookup ]; then
                case "$r" in monoio | glommio | compio) continue ;; esac
            fi
            taskset -c "$CORES" target/release/bench-core "$r" "$w" >>"$OUT/raw.csv"
        done
    done
    echo "round $round done" >&2
done
python3 - "$OUT/raw.csv" <<'PY' | tee "$OUT/summary.txt"
import csv, sys, statistics
rows = list(csv.DictReader(open(sys.argv[1])))
order_w, order_r, data = [], [], {}
for r in rows:
    k = (r["workload"], r["runtime"])
    if r["workload"] not in order_w: order_w.append(r["workload"])
    if r["runtime"] not in order_r: order_r.append(r["runtime"])
    ops = int(r["ops"])
    data.setdefault(k, []).append((int(r["wall_ns"]) / ops, int(r["cpu_ns"]) / ops))
print(f"{len(rows) // max(1, len(data))} rounds; ns per operation, median [min-max]")
print(f"{'workload':10} {'runtime':12} {'wall':>8} {'min':>8} {'max':>8} {'cpu':>8}")
for w in order_w:
    for r in order_r:
        v = data.get((w, r))
        if not v: continue
        wall = [a for a, _ in v]; cpu = [b for _, b in v]
        print(f"{w:10} {r:12} {statistics.median(wall):8.1f} {min(wall):8.1f} {max(wall):8.1f} {statistics.median(cpu):8.1f}")
    print()
PY
