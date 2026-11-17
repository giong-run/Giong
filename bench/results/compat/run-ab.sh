#!/bin/bash
# How raw.csv was made: bench/compat's binary built at two commits, run
# interleaved in one session (Compat only for the old build; giong's own API
# and Tokio do not change with giong-compat).
#   OLD=/path/to/old/bench-compat NEW=/path/to/new/bench-compat run-ab.sh <out-dir> [runs]
set -eu
OLD=${OLD:?build bench/compat at the old commit}
NEW=${NEW:?build bench/compat at the new commit}
OUT=$1; mkdir -p $OUT; CSV=$OUT/raw.csv
echo "build,variant,workload,round,rate,cpu" > $CSV
for round in $(seq 1 ${2:-9}); do
  for workload in recv send rr; do
    for variant in giong compat compat-bufread tokio; do
      [ "$variant" = compat-bufread ] && [ "$workload" != recv ] && continue
      for b in old new; do
        bin=$OLD; [ $b = new ] && bin=$NEW
        [ $b = old ] && [ $variant != compat ] && [ $variant != compat-bufread ] && continue
        echo "$b,$variant,$workload,$round,$($bin $variant $workload)" >> $CSV
      done
    done
  done
  echo "round $round" >&2
done
