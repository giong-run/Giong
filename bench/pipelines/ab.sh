#!/usr/bin/env bash
# Interleaved A/B runs of one bench-pipelines case.
#
#   bench/pipelines/ab.sh <rounds> <case> <case-args> -- name:VAR=v,VAR=v name2: ...
#
# Each round runs every variant once, starting from a different one each
# round. Raw lines go to $OUT/raw.csv (appended); the median per variant of
# wall seconds and CPU microseconds per operation is printed at the end.
set -euo pipefail
cd "$(dirname "$0")"
ROUNDS=$1; CASE=$2; shift 2
ARGS=()
while [ "$1" != "--" ]; do ARGS+=("$1"); shift; done
shift
VARIANTS=("$@")
OUT=${OUT:-../results/pipelines}
mkdir -p "$OUT"
cargo build --release -q
BIN=target/release/bench-pipelines
tmp=$(mktemp)
n=${#VARIANTS[@]}
for ((r = 0; r < ROUNDS; r++)); do
  for ((k = 0; k < n; k++)); do
    v=${VARIANTS[$(((k + r) % n))]}
    name=${v%%:*}; vars=${v#*:}
    env_args=(VARIANT="$name")
    if [ -n "$vars" ]; then IFS=, read -r -a kv <<<"$vars"; env_args+=("${kv[@]}"); fi
    line=$(env "${env_args[@]}" "$BIN" "$CASE" "${ARGS[@]}")
    echo "$line" | tee -a "$tmp"
  done
done
cat "$tmp" >>"$OUT/raw.csv"
python3 - "$tmp" <<'PY'
import sys, statistics, collections
rows = collections.defaultdict(list)
for line in open(sys.argv[1]):
    f = line.strip().split(",")
    rows[(f[0], f[1])].append((float(f[3]), float(f[4]), f[5] if len(f) > 5 else ""))
for (case, var), rs in rows.items():
    secs = [r[0] for r in rs]; cpu = [r[1] for r in rs]
    print(f"{case} {var}: secs median {statistics.median(secs):.3f} (min {min(secs):.3f} max {max(secs):.3f}), "
          f"cpu/op median {statistics.median(cpu):.3f} us (min {min(cpu):.3f} max {max(cpu):.3f}), n={len(rs)}")
PY
rm -f "$tmp"
