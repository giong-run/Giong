#!/usr/bin/env bash
# Network latency while CPU-heavy jobs run: the compute lane against running
# the jobs inline on the workers, the same lane without its lower priority,
# and Tokio's spawn_blocking. See lanes/src/main.rs for the workload.
#
#   bench/lanes.sh [rounds] [seconds]
#
# Method
#   * One process: an echo server with one worker per CPU, and 8 client
#     threads that each send a 64-byte ping every millisecond and time the
#     echo. Every scenario keeps one CPU-bound job (about 2 ms) in flight per
#     CPU, so the CPU work offered is the same.
#   * `lane-nice0` undoes the lane's nice +10 inside each job, which needs
#     root or CAP_SYS_NICE; without it that scenario fails and is skipped.
#   * Scenarios are interleaved; the report shows the median of all rounds
#     and the min/max for each column.
#
# Requires: cargo, python3. Results: bench/results/lanes/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-10}
OUT=${OUT:-results/lanes}
SCENARIOS=${SCENARIOS:-"idle lane lane-nice0 inline tokio-idle tokio"}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd lanes && cargo build --release -q)
BIN=lanes/target/release/bench-lanes

{
    echo "# $(date -u +%FT%TZ) $(uname -r) $(nproc) CPUs: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
    echo "round,scenario,pings,p50_us,p99_us,p999_us,max_us,jobs_per_s"
} > "$CSV"
port=9700
for round in $(seq 1 "$ROUNDS"); do
    for s in $SCENARIOS; do
        port=$((port + 1))
        echo "round $round: $s" >&2
        if line=$("$BIN" "$s" "$SECS" "$port" 2>/dev/null); then
            echo "$round,$line" >> "$CSV"
        else
            echo "  $s failed, skipped" >&2
        fi
    done
done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = [r for r in csv.reader(l for l in open(sys.argv[1]) if not l.startswith('#'))][1:]
order = []
by = {}
for r in rows:
    by.setdefault(r[1], []).append([float(x) for x in r[2:]])
    if r[1] not in order:
        order.append(r[1])
cols = ["pings", "p50 us", "p99 us", "p99.9 us", "max us", "jobs/s"]
print(open(sys.argv[1]).readline().strip())
print(f"{'scenario':<11}" + "".join(f"{c:>22}" for c in cols))
for s in order:
    vals = by[s]
    cells = []
    for i in range(len(cols)):
        xs = [v[i] for v in vals]
        cells.append(f"{statistics.median(xs):>8.0f} ({min(xs):.0f}-{max(xs):.0f})")
    print(f"{s:<11}" + "".join(f"{c:>22}" for c in cells))
print(f"rounds: {len(by[order[0]])}")
PY
