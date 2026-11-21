#!/usr/bin/env bash
# giong-proto against prost: decode, decode and read every field, forward a
# received message, and encode a message built in code, on five message
# shapes. See proto/src/main.rs.
#
#   bench/proto.sh
#
# Method
#   * One thread pinned to MEASURE_CPU (default 2), release build with LTO.
#   * Each measurement: calibrate a batch to ~10 ms, warm up one round, then
#     ROUNDS (default 10) rounds of ROUND_MS (default 300) ms; ns per call
#     for each round in raw.csv.
#   * prost 0.13 with its defaults (String, Vec<u8>, HashMap), as tonic uses
#     it. Both read the same bytes, and the harness checks they read the
#     same values and that each one's encoding reads back as the other's.
#   * "decode" alone favours giong-proto, which reads repeated fields only
#     when they are iterated: compare "decode+read" for a fair total.
#
# Requires: cargo, python3. Results: bench/results/proto/.
set -euo pipefail
cd "$(dirname "$0")"

OUT=${OUT:-results/proto}
CPU=${MEASURE_CPU:-2}
mkdir -p "$OUT"

echo "building..." >&2
(cd proto && cargo build --release -q)

{
  echo "date: $(date -u +%Y-%m-%dT%H:%MZ)"
  echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | sed 's/^ //')"
  echo "cores: $(nproc)"
  echo "kernel: $(uname -r)"
  echo "rustc: $(rustc --version)"
  echo "pinned to cpu: $CPU"
  echo "commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
} > "$OUT/machine.txt"

taskset -c "$CPU" proto/target/release/proto-bench > "$OUT/raw.csv"

python3 - "$OUT/raw.csv" > "$OUT/summary.txt" <<'PY'
import collections, csv, statistics, sys
rows = collections.defaultdict(list)
size = {}
for r in csv.DictReader(open(sys.argv[1])):
    rows[(r["case"], r["op"], r["impl"])].append(float(r["ns_per_op"]))
    size[r["case"]] = int(r["bytes"])
order = ["decode", "decode+read", "forward", "encode", "allocs"]
cases = list(dict.fromkeys(c for c, _, _ in rows))
print("giong-proto vs prost: median ns per call over all rounds (min-max), and prost/giong.")
print("A ratio above 1 means giong-proto is faster. 'allocations' is heap")
print("allocations per decode-and-read call.\n")
print(f"{'case':16} {'bytes':>7} {'op':12} {'prost ns':>22} {'giong-proto ns':>22} {'ratio':>7}")
for c in cases:
    for op in order:
        p, g = rows.get((c, op, "prost")), rows.get((c, op, "giong"))
        if not p or not g:
            continue
        if op == "allocs":
            print(f"{c:16} {size[c]:7} {'allocations':12} {int(p[0]):>22} {int(g[0]):>22}")
            continue
        fmt = lambda v: f"{statistics.median(v):10.1f} ({min(v):.0f}-{max(v):.0f})"
        print(f"{c:16} {size[c]:7} {op:12} {fmt(p):>22} {fmt(g):>22} {statistics.median(p) / statistics.median(g):6.2f}x")
PY
cat "$OUT/summary.txt"
