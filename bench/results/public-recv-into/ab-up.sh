#!/bin/bash
# Interleaved upload A/B: bench/giong before and after it used recv_into, and
# bench/tokio-raw. Server on CPU 0, bench/lab's load generator on CPUs 1-3,
# 16 connections, closed loop, 1 s warmup + 5 s. One JSON line per run.
#
#   MAIN_BIN=... BRANCH_BIN=... TOKIO_BIN=... LOADGEN=... ./ab-up.sh > runs.jsonl
set -eu
TMP=$(mktemp)
declare -A BIN=( [main]=$MAIN_BIN [branch]=$BRANCH_BIN [tokio]=$TOKIO_BIN )
declare -A PORT=( [main]=3006 [branch]=3006 [tokio]=3005 )
for round in 1 2 3; do
 for body in 1024 65536 1048576; do
  for v in main branch tokio; do
   taskset -c 0 "${BIN[$v]}" >/dev/null 2>&1 & SP=$!
   sleep 0.7
   S0=$(awk '{print $14+$15}' /proc/$SP/stat)
   taskset -c 1-3 "$LOADGEN" --addr 127.0.0.1:${PORT[$v]} --threads 3 --duration 5 --warmup 1 \
     --conns 16 --mix /upload --body $body > "$TMP" 2>/dev/null
   S1=$(awk '{print $14+$15}' /proc/$SP/stat)
   kill $SP; wait $SP 2>/dev/null || true
   python3 - "$v" "$body" "$round" "$S0" "$S1" "$TMP" <<'PY'
import json, os, sys
v, body, rnd, s0, s1, path = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), sys.argv[6]
r = json.load(open(path))
tick = os.sysconf("SC_CLK_TCK")
# The CPU ticks cover warmup and measurement (6 s); requests/s is over the 5 s.
n = r["rps"] * 6
print(json.dumps({"round": rnd, "version": v, "body": body, "rps": round(r["rps"]),
                  "mb_s": round(r["rps"] * body / 1e6), "p99_us": r["latency"]["p99_us"],
                  "errors": r["errors"],
                  "cpu_us_per_req": round((s1 - s0) / tick * 1e6 / n, 1) if n else None}))
PY
  done
 done
done
rm -f "$TMP"
