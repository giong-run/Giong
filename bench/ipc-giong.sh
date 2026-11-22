#!/usr/bin/env bash
# giong::ipc against giong's UnixStream between two processes, one giong
# worker each, pinned to CPU_A and CPU_B (bench/ipc-giong).
#
#   bench/ipc-giong.sh [rounds]
#
# Each round runs, for both transports: ping (one 64-byte message in flight)
# with workers that sleep at once and with spin_before_park(50 us), and a
# one-way stream of 64-byte messages (the Unix socket gets 32 per write).
# Rows go to $OUT/raw.csv:
#   round,spin_us,transport,mode,count,per_s,us_per_op,cpu_us_per_op
set -euo pipefail
cd "$(dirname "$0")"
ROUNDS=${1:-5}
CPU_A=${CPU_A:-0}
CPU_B=${CPU_B:-1}
OUT=${OUT:-results/ipc-giong}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
[ -f "$CSV" ] || echo "round,spin_us,transport,mode,count,per_s,us_per_op,cpu_us_per_op" >"$CSV"
(cd ipc-giong && cargo build --release -q)
B=ipc-giong/target/release/bench-ipc-giong
run() { # round spin transport mode count
  local row
  row=$(GIONG_SPIN_US=$2 timeout 120 "$B" "$3" "$4" "$5" "$CPU_A" "$CPU_B")
  echo "$1,$2,$row" >>"$CSV"
}
for r in $(seq "$ROUNDS"); do
  for t in ipc ipc-in-place unix; do
    run "$r" 0 "$t" ping 100000
    run "$r" 50 "$t" ping 500000
    run "$r" 0 "$t" stream 10000000
    run "$r" 50 "$t" stream 10000000
  done
done
python3 - "$CSV" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
keys = sorted({(r["mode"], r["spin_us"], r["transport"]) for r in rows})
print(f"{'mode':7} {'spin':>4} {'transport':9} {'per s':>12} {'us/op':>8} {'cpu us/op':>9}  (medians of {len(rows) // max(len(keys), 1)})")
for k in keys:
    rs = [r for r in rows if (r["mode"], r["spin_us"], r["transport"]) == k]
    med = lambda f: statistics.median(float(r[f]) for r in rs)
    print(f"{k[0]:7} {k[1]:>4} {k[2]:9} {med('per_s'):12.0f} {med('us_per_op'):8.3f} {med('cpu_us_per_op'):9.3f}")
PY
