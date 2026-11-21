#!/usr/bin/env bash
# Does steering each TCP connection to the worker on the CPU that received
# its SYN (GIONG_STEER=cpu) lower the CPU cost per request?
#
#   sudo bench/steer.sh [rounds] [seconds]
#
# Needs root (network namespace, veth, RPS). The client runs in namespace
# "steer-cli" behind a veth pair. RPS on the server side of the pair spreads
# receive processing over SERVER_CPUS by flow hash, the way RSS spreads a
# real NIC's queues, so the receive path of each connection runs on one
# server CPU. The server runs one pinned worker per SERVER_CPU.
#
# For each round it runs both variants back to back (order alternates) and
# records req/s, p99, and the busy time of the server CPUs per request
# (all non-idle time from /proc/stat, so softirq work is counted wherever it
# runs). Raw rows go to $OUT/raw.csv.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-10}
SERVER_CPUS=${SERVER_CPUS:-0,1}
LOAD_CPUS=${LOAD_CPUS:-2,3}
CONNS=${CONNS:-128}
ENDPOINT=${ENDPOINT:-json}
OUT=${OUT:-results/steer}
NS=steer-cli
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
[ -f "$CSV" ] || echo "round,variant,endpoint,conns,req_per_s,p99_us,server_cpu_us_per_req" >"$CSV"

mask() { # "0,1" -> hex mask
  local m=0 c
  IFS=, read -r -a cs <<<"$1"
  for c in "${cs[@]}"; do m=$((m | (1 << c))); done
  printf '%x' "$m"
}

setup() {
  ip netns del "$NS" 2>/dev/null || true
  ip link del st-s 2>/dev/null || true
  ip netns add "$NS"
  ip link add st-s type veth peer name st-c
  ip link set st-c netns "$NS"
  ip addr add 10.78.0.1/24 dev st-s
  ip link set st-s up
  ip netns exec "$NS" ip addr add 10.78.0.2/24 dev st-c
  ip netns exec "$NS" ip link set st-c up
  ip netns exec "$NS" ip link set lo up
  mask "$SERVER_CPUS" >/sys/class/net/st-s/queues/rx-0/rps_cpus
  ip netns exec "$NS" sh -c "echo $(mask "$LOAD_CPUS") >/sys/class/net/st-c/queues/rx-0/rps_cpus"
}
teardown() { ip netns del "$NS" 2>/dev/null || true; ip link del st-s 2>/dev/null || true; }
trap teardown EXIT

busy_ticks() { # sum of non-idle ticks over SERVER_CPUS
  local total=0 c
  IFS=, read -r -a cs <<<"$SERVER_CPUS"
  for c in "${cs[@]}"; do
    total=$((total + $(awk -v c="cpu$c" '$1 == c { print $2 + $3 + $4 + $7 + $8 + $9 }' /proc/stat)))
  done
  echo "$total"
}

run_one() {
  local round=$1 variant=$2 env=()
  [ "$variant" = steer ] && env=(GIONG_STEER=cpu)
  env "${env[@]}" BENCH_HOST=10.78.0.1 taskset -c "$SERVER_CPUS" giong/target/release/bench-giong &
  local pid=$!
  for _ in $(seq 50); do ip netns exec "$NS" bash -c 'exec 3<>/dev/tcp/10.78.0.1/3006' 2>/dev/null && break; sleep 0.1; done
  ip netns exec "$NS" taskset -c "$LOAD_CPUS" wrk -t2 -c"$CONNS" -d2s "http://10.78.0.1:3006/$ENDPOINT" >/dev/null
  local t0 t1 out
  t0=$(busy_ticks)
  out=$(ip netns exec "$NS" taskset -c "$LOAD_CPUS" wrk -t2 -c"$CONNS" -d"${SECS}s" --latency "http://10.78.0.1:3006/$ENDPOINT")
  t1=$(busy_ticks)
  kill "$pid"; wait "$pid" 2>/dev/null || true
  local rps p99 p99us cpu
  rps=$(awk '/Requests\/sec/ { print $2 }' <<<"$out")
  p99=$(awk '$1 == "99%" { print $2 }' <<<"$out")
  p99us=$(awk -v v="$p99" 'BEGIN { n = v + 0; if (v ~ /ms$/) n *= 1000; else if (v ~ /[0-9]s$/) n *= 1000000; print n }')
  cpu=$(awk -v t="$((t1 - t0))" -v hz="$(getconf CLK_TCK)" -v r="$rps" -v s="$SECS" 'BEGIN { printf "%.3f", t / hz * 1e6 / (r * s) }')
  echo "$round,$variant,$ENDPOINT,$CONNS,$rps,$p99us,$cpu" >>"$CSV"
  printf '%-6s round %s: %10s req/s  p99 %8s us  server CPU %s us/req\n' "$variant" "$round" "$rps" "$p99us" "$cpu"
}

(cd giong && cargo build --release -q)
setup
for r in $(seq "$ROUNDS"); do
  if [ $((r % 2)) -eq 1 ]; then run_one "$r" hash; run_one "$r" steer; else run_one "$r" steer; run_one "$r" hash; fi
  sleep 1
done
