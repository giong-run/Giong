#!/usr/bin/env bash
# Same-host messaging between two processes: shared memory with io_uring
# futex wake-ups against a Unix socket and TCP over loopback (bench/ipc).
#
#   bench/ipc.sh [rounds]
#
# CPU_A and CPU_B pin the two processes (default 0 and 1). Needs Linux 6.7+
# for IORING_OP_FUTEX_WAIT. Rows go to $OUT/raw.csv:
#   round,transport,mode,spin_us,count,per_s,us_per_op,cpu_us_per_op
# cpu_us_per_op is the CPU time of both processes per round trip (ping) or
# per message (stream).
set -euo pipefail
cd "$(dirname "$0")"
ROUNDS=${1:-5}
CPU_A=${CPU_A:-0}
CPU_B=${CPU_B:-1}
OUT=${OUT:-results/ipc}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
[ -f "$CSV" ] || echo "round,transport,mode,spin_us,count,per_s,us_per_op,cpu_us_per_op" >"$CSV"
(cd ipc && cargo build --release -q)
B=ipc/target/release/bench-ipc
for r in $(seq "$ROUNDS"); do
  for t in shm unix tcp; do
    echo "$r,0,$(BENCH_IPC_SPIN_US=0 $B "$t" ping 200000 "$CPU_A" "$CPU_B")" | awk -F, -v OFS=, '{ print $1, $3, $4, $2, $5, $6, $7, $8 }' >>"$CSV"
    echo "$r,50,$(BENCH_IPC_SPIN_US=50 $B "$t" ping 1000000 "$CPU_A" "$CPU_B")" | awk -F, -v OFS=, '{ print $1, $3, $4, $2, $5, $6, $7, $8 }' >>"$CSV"
    echo "$r,0,$(BENCH_IPC_SPIN_US=0 $B "$t" stream 20000000 "$CPU_A" "$CPU_B")" | awk -F, -v OFS=, '{ print $1, $3, $4, $2, $5, $6, $7, $8 }' >>"$CSV"
  done
done
python3 - "$CSV" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
keys = sorted({(r["mode"], r["spin_us"], r["transport"]) for r in rows})
print(f"{'mode':7} {'spin':>4} {'transport':9} {'per s':>12} {'us/op':>8} {'cpu us/op':>9}  (medians)")
for k in keys:
    rs = [r for r in rows if (r["mode"], r["spin_us"], r["transport"]) == k]
    med = lambda f: statistics.median(float(r[f]) for r in rs)
    print(f"{k[0]:7} {k[1]:>4} {k[2]:9} {med('per_s'):12.0f} {med('us_per_op'):8.3f} {med('cpu_us_per_op'):9.3f}")
PY
