#!/usr/bin/env bash
# File I/O benchmark: giong (io_uring) against Tokio (thread pool) and
# blocking std. See fs/src/main.rs for the two workloads.
#
#   bench/fs.sh [rounds] [operations]
#
# Method
#   * Everything is in the page cache: this measures what one operation costs
#     in the runtime, not the disk.
#   * giong and std use one thread, pinned to CPU. Tokio hands every file
#     operation to its blocking thread pool, so it is measured twice: pinned
#     to the same single core, and free to use all cores (`tokio-all`).
#   * Runs are interleaved; the report shows the median of all rounds and the
#     min/max.
#
# Requires: cargo, python3. Results: bench/results/fs/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
OPS=${2:-200000}
CPU=${CPU:-2}
OUT=${OUT:-results/fs}
DIR=${DIR:-${TMPDIR:-/tmp}/giong-bench-fs}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd fs && cargo build --release -q)
BIN=fs/target/release/bench-fs
"$BIN" std small-files 1 1000 "$DIR" >/dev/null   # creates the files

echo "runtime,workload,tasks,round,ops_per_sec" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for workload in small-files random-read; do
    for tasks in 1 64; do
      for runtime in std giong monoio compio tokio tokio-all; do
        [ "$runtime" = std ] && [ "$tasks" != 1 ] && continue
        case "$runtime" in
          tokio-all) rate=$("$BIN" tokio "$workload" "$tasks" "$OPS" "$DIR") ;;
          *) rate=$(taskset -c "$CPU" "$BIN" "$runtime" "$workload" "$tasks" "$OPS" "$DIR") ;;
        esac
        echo "$runtime,$workload,$tasks,$round,$rate" >> "$CSV"
      done
    done
  done
  echo "round $round/$ROUNDS done" >&2
done
rm -rf "$DIR"

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys

rows = list(csv.DictReader(open(sys.argv[1])))
keys = []
for r in rows:
    k = (r["workload"], r["tasks"], r["runtime"])
    if k not in keys: keys.append(k)
print(f"{'workload':<13}{'tasks':>6}  {'runtime':<11}{'ops/s median':>13}{'min':>10}{'max':>10}")
last = None
for k in keys:
    if last and k[:2] != last: print()
    last = k[:2]
    v = [float(r["ops_per_sec"]) for r in rows if (r["workload"], r["tasks"], r["runtime"]) == k]
    print(f"{k[0]:<13}{k[1]:>6}  {k[2]:<11}{statistics.median(v):13.0f}{min(v):10.0f}{max(v):10.0f}")
PY
