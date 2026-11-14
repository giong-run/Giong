#!/usr/bin/env bash
# The send workload of bench/compat.sh for three builds of bench/compat,
# interleaved in one session: giong's own API, Compat and Tokio at each.
#
#   OLD=<bench-compat at 8632eb7> V050=<at 7158372> FIX=<on #83> \
#     run.sh raw.csv [rounds]
#
# Build each with `cd bench/compat && cargo build --release` in a worktree
# of that commit. Tokio's code is the same in all three builds, so its
# spread shows how much identical binaries differ within one session.
set -euo pipefail
OUT=$1; RUNS=${2:-9}
declare -A BIN=( [old]=$OLD [v050]=$V050 [fix]=$FIX )
echo "variant,workload,round,rate,cpu" > "$OUT"
for round in $(seq 1 "$RUNS"); do
  for v in old v050 fix; do
    for variant in giong compat tokio; do
      echo "$variant-$v,send,$round,$("${BIN[$v]}" "$variant" send)" >> "$OUT"
    done
  done
  echo "round $round/$RUNS done" >&2
done
python3 - "$OUT" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
for v in sorted({r["variant"] for r in rows}):
    sel = [r for r in rows if r["variant"] == v]
    rate = [float(r["rate"]) for r in sel]; cpu = [float(r["cpu"]) for r in sel]
    print(f"  {v:<14}{statistics.median(rate):>8.0f} [{min(rate):.0f}-{max(rate):.0f}]{statistics.median(cpu):>10.1f} [{min(cpu):.1f}-{max(cpu):.1f}]")
PY
