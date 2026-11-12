#!/usr/bin/env bash
# How does throughput grow from one worker to N?
#
#   bench/workers.sh [rounds] [seconds-per-measurement]
#
# Same machine (the default): the server gets the first N CPUs of SERVER_CPUS,
# wrk gets LOAD_CPUS. On a small machine the load generator runs out of CPU
# first; the report says so instead of hiding it.
#
#   SERVER_CPUS=2,3,4,5,6,7 LOAD_CPUS=0,1 WORKERS="1 2 4 6" bench/workers.sh
#
# Two machines (what the numbers should come from): the server runs here,
# bound to BENCH_HOST, and wrk runs on LOAD_HOST over ssh with LOAD_THREADS
# threads. Use a physical network between them.
#
#   BENCH_HOST=10.0.0.2 LOAD_HOST=bench@10.0.0.3 LOAD_THREADS=16 \
#     SERVER_CPUS=0-15 WORKERS="1 2 4 8 16" bench/workers.sh
#
# For every server and worker count it records requests per second, p99
# latency, the CPU time the server spent per request (user + kernel, from
# /proc), and how busy the server's and the load generator's CPUs were. The
# connection count grows with the workers (CONNS_PER_WORKER, default 64), so
# each worker always sees the same load. Efficiency is req/s at N workers
# divided by N times req/s at one worker.
#
# Every Rust server here starts one worker (thread or runtime) per CPU it is
# allowed to run on, so the CPU list sets the worker count.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-3}
SECS=${2:-6}
ENDPOINT=${ENDPOINT:-json}
CONNS_PER_WORKER=${CONNS_PER_WORKER:-64}
BENCH_HOST=${BENCH_HOST:-127.0.0.1}
LOAD_HOST=${LOAD_HOST:-}
OUT=${OUT:-results/workers}
read -r -a NAMES <<<"${SERVERS:-giong giong-http monoio-tuned tokio-raw glommio}"

expand_cpus() { # "0-2,5" -> "0 1 2 5"
  local out=() part
  IFS=, read -r -a parts <<<"$1"
  for part in "${parts[@]}"; do
    if [[ $part == *-* ]]; then out+=($(seq "${part%-*}" "${part#*-}")); else out+=("$part"); fi
  done
  echo "${out[@]}"
}

ncpu=$(nproc)
if [ -n "$LOAD_HOST" ]; then
  read -r -a SERVER_LIST <<<"$(expand_cpus "${SERVER_CPUS:-0-$((ncpu - 1))}")"
  LOAD_LIST=()
  LOAD_THREADS=${LOAD_THREADS:-$(ssh "$LOAD_HOST" nproc)}
else
  half=$((ncpu / 2))
  read -r -a SERVER_LIST <<<"$(expand_cpus "${SERVER_CPUS:-$half-$((ncpu - 1))}")"
  read -r -a LOAD_LIST <<<"$(expand_cpus "${LOAD_CPUS:-0-$((half - 1))}")"
  LOAD_THREADS=${LOAD_THREADS:-${#LOAD_LIST[@]}}
fi
default_workers() { local n=1 out=(); while [ "$n" -le "${#SERVER_LIST[@]}" ]; do out+=("$n"); n=$((n * 2)); done; echo "${out[@]}"; }
read -r -a WORKERS <<<"${WORKERS:-$(default_workers)}"
TICKS=$(getconf CLK_TCK)
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
ulimit -n "$(ulimit -Hn)"

echo "building..." >&2
for d in giong giong-http tokio-raw monoio glommio; do (cd "$d" && cargo build --release -q); done

declare -A CMD=(
  [giong]="giong/target/release/bench-giong"
  [giong-spin]="env GIONG_SPIN_US=200 giong/target/release/bench-giong"
  [giong-http]="giong-http/target/release/bench-giong-http"
  [monoio]="monoio/target/release/bench-monoio"
  [monoio-tuned]="env MONOIO_RING=defer monoio/target/release/bench-monoio"
  [tokio-raw]="tokio-raw/target/release/bench-tokio-raw"
  [glommio]="glommio/target/release/bench-glommio"
)
declare -A PORT=([giong]=3006 [giong-spin]=3006 [giong-http]=3009 [monoio]=3007 [monoio-tuned]=3007
  [tokio-raw]=3005 [glommio]=3008)

cpu_ticks() { cat /proc/"$1"/task/*/stat 2>/dev/null | awk '{u+=$14; s+=$15} END{print u+0, s+0}'; }
# Busy and total jiffies over a set of CPUs, from /proc/stat.
cpu_busy() {
  local cpus=" $* "
  awk -v cpus="$cpus" '$1 ~ /^cpu[0-9]+$/ { n = substr($1, 4); if (index(cpus, " " n " ")) {
      t = 0; for (i = 2; i <= NF; i++) t += $i; idle += $5 + $6; total += t } }
      END { print total - idle, total }' /proc/stat
}
load() { # wrk arguments...
  if [ -n "$LOAD_HOST" ]; then ssh "$LOAD_HOST" wrk "$@"; else taskset -c "$(IFS=,; echo "${LOAD_LIST[*]}")" wrk "$@"; fi
}

echo "server,workers,conns,round,rps,p99,user_us,kernel_us,server_busy,load_busy" >"$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for n in "${WORKERS[@]}"; do
    cpus=("${SERVER_LIST[@]:0:$n}")
    conns=$((CONNS_PER_WORKER * n))
    for s in "${NAMES[@]}"; do
      BENCH_HOST="$BENCH_HOST" taskset -c "$(IFS=,; echo "${cpus[*]}")" ${CMD[$s]} >/dev/null 2>&1 &
      pid=$!
      sleep 1.2
      url="http://$BENCH_HOST:${PORT[$s]}/$ENDPOINT"
      load -t"$LOAD_THREADS" -c"$conns" -d2s --timeout 5s "$url" >/dev/null 2>&1
      read -r u0 k0 <<<"$(cpu_ticks "$pid")"
      read -r sb0 st0 <<<"$(cpu_busy "${cpus[@]}")"
      read -r lb0 lt0 <<<"$(cpu_busy "${LOAD_LIST[@]:-none}")"
      out=$(load -t"$LOAD_THREADS" -c"$conns" -d"${SECS}s" --timeout 5s --latency "$url")
      read -r u1 k1 <<<"$(cpu_ticks "$pid")"
      read -r sb1 st1 <<<"$(cpu_busy "${cpus[@]}")"
      read -r lb1 lt1 <<<"$(cpu_busy "${LOAD_LIST[@]:-none}")"
      reqs=$(awk '/requests in/{print $1}' <<<"$out")
      rps=$(awk '/Requests\/sec/{print $2}' <<<"$out")
      p99=$(awk '$1=="99%"{print $2}' <<<"$out")
      read -r uus kus <<<"$(awk -v r="$reqs" -v u=$((u1 - u0)) -v k=$((k1 - k0)) -v t="$TICKS" \
        'BEGIN{printf "%.3f %.3f", u/t/r*1e6, k/t/r*1e6}')"
      sbusy=$(awk -v b=$((sb1 - sb0)) -v t=$((st1 - st0)) 'BEGIN{printf "%.2f", t ? b/t : 0}')
      lbusy=$(awk -v b=$((lb1 - lb0)) -v t=$((lt1 - lt0)) 'BEGIN{printf "%.2f", t ? b/t : -1}')
      echo "$s,$n,$conns,$round,$rps,$p99,$uus,$kus,$sbusy,$lbusy" >>"$CSV"
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      sleep 0.5
    done
    echo "round $round/$ROUNDS: $n worker(s) done" >&2
  done
done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys

def to_ms(v):
    for unit, k in (("us", 1e-3), ("ms", 1.0), ("s", 1e3)):
        if v.endswith(unit): return float(v[:-len(unit)]) * k
    return float(v)

rows = list(csv.DictReader(open(sys.argv[1])))
servers, workers = [], []
for r in rows:
    if r["server"] not in servers: servers.append(r["server"])
    if int(r["workers"]) not in workers: workers.append(int(r["workers"]))
med = statistics.median
base = {}
print(f"{'workers':<8}{'server':<14}{'req/s median':>13}{'min':>9}{'max':>9}{'eff.':>6}"
      f"{'cpu us/req':>11}{'p99 ms':>8}{'server cpu':>11}{'load cpu':>9}  limited by")
for n in sorted(workers):
    for s in servers:
        sel = [r for r in rows if r["server"] == s and int(r["workers"]) == n]
        if not sel: continue
        rps = [float(r["rps"]) for r in sel]
        if n == min(workers): base[s] = med(rps)
        eff = med(rps) / (n / min(workers) * base[s]) if s in base else float("nan")
        cpu = med(float(r["user_us"]) + float(r["kernel_us"]) for r in sel)
        p99 = med(to_ms(r["p99"]) for r in sel)
        sbusy = med(float(r["server_busy"]) for r in sel)
        lbusy = med(float(r["load_busy"]) for r in sel)
        # Whichever side is close to saturated sets the number.
        if lbusy >= 0.9 and lbusy >= sbusy: limit = "load generator"
        elif sbusy >= 0.9: limit = "server"
        else: limit = "neither (network or latency)"
        load = f"{lbusy:9.0%}" if lbusy >= 0 else f"{'remote':>9}"
        print(f"{n:<8}{s:<14}{med(rps):13.0f}{min(rps):9.0f}{max(rps):9.0f}{eff:6.2f}"
              f"{cpu:11.2f}{p99:8.2f}{sbusy:11.0%}{load}  {limit}")
    print()
PY
