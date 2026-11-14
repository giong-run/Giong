#!/usr/bin/env bash
# What giong-compat's `Compat` costs: giong's own API, `Compat` through the
# Tokio traits (read into a buffer, and `fill_buf`/`consume`), and Tokio's
# own TcpStream, on three workloads over loopback. See compat/src/main.rs.
#
#   bench/compat.sh [rounds]
#
# Method
#   * One measured thread pinned to MEASURE_CPU (default 2); the other end is
#     a blocking std thread pinned to PEER_CPU (default 3), the same for
#     every variant.
#   * recv/send: TOTAL_MIB (default 2048) MiB; MiB/s, and CPU time of the
#     measured thread per KiB (user + system, CLOCK_THREAD_CPUTIME_ID).
#   * rr: ROUNDS (default 50000) round trips of 64 bytes; round trips/s and
#     CPU microseconds per round trip.
#   * Runs are interleaved; the report shows the median of all rounds and the
#     min/max. Loopback bulk transfers vary a lot from run to run on a
#     virtual machine: compare medians, and read the spread.
#
# Requires: cargo, python3. Results: bench/results/compat/.
set -euo pipefail
cd "$(dirname "$0")"

RUNS=${1:-7}
OUT=${OUT:-results/compat}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd compat && cargo build --release -q)
BIN=compat/target/release/bench-compat

echo "variant,workload,round,rate,cpu" > "$CSV"
for round in $(seq 1 "$RUNS"); do
  for workload in recv send rr; do
    for variant in giong compat compat-bufread tokio; do
      [ "$variant" = compat-bufread ] && [ "$workload" != recv ] && continue
      echo "$variant,$workload,$round,$("$BIN" "$variant" "$workload")" >> "$CSV"
    done
  done
  echo "round $round/$RUNS done" >&2
done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys

rows = list(csv.DictReader(open(sys.argv[1])))
units = {"recv": ("MiB/s", "CPU ns/KiB"), "send": ("MiB/s", "CPU ns/KiB"),
         "rr": ("round trips/s", "CPU us/round trip")}
for workload in ["recv", "send", "rr"]:
    rate_u, cpu_u = units[workload]
    print(f"\n{workload}: {rate_u} and {cpu_u}, median [min-max] of {len({r['round'] for r in rows})} runs")
    for variant in ["giong", "compat", "compat-bufread", "tokio"]:
        sel = [r for r in rows if r["workload"] == workload and r["variant"] == variant]
        if not sel:
            continue
        rate = [float(r["rate"]) for r in sel]
        cpu = [float(r["cpu"]) for r in sel]
        print(f"  {variant:<15}{statistics.median(rate):>10.0f} [{min(rate):.0f}-{max(rate):.0f}]"
              f"{statistics.median(cpu):>12.2f} [{min(cpu):.2f}-{max(cpu):.2f}]")
PY
