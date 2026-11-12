#!/usr/bin/env bash
# Redis client benchmark: giong-redis against redis-rs and fred on Tokio,
# each on one core, against the same local redis-server.
#
#   bench/redis.sh [rounds] [seconds-per-measurement]
#
# Method
#   * redis-server (no persistence) runs on SERVER_CPUS with
#     `io-threads 3`, so that its network work is not the limit; the client
#     runs on CLIENT_CPU, so the numbers show how much one core of client
#     gets done.
#   * Workloads: `get` is GET of a 64-byte value, `set` SET of one (each
#     task its own key), `get8k` GET of an 8 KiB value, `mget` MGET of ten
#     64-byte values.
#   * Clients, by how many connections they use: with 1, giong-redis is a
#     Pool of 1 shared by every task; redis-rs (1.x) one MultiplexedConnection
#     cloned into every task; fred (10.x) one Client, with its automatic
#     pipelining. With 4, the giong-redis pool may open 4; redis-rs and fred
#     have 4 of theirs, task t using number t % 4. redis-rs and fred run on
#     a current-thread Tokio runtime.
#   * TASKS concurrent tasks send commands back to back. 2 s of warm-up,
#     then SECS measured. `cpu%` is the client's core; `srv%` the server's
#     cores together (100 per core), from /proc/stat; `us/op` is the
#     client's CPU time per command.
#
# WORKLOADS and CLIENTS pick a subset, for example
#   WORKLOADS=get CLIENTS="giong redis-rs" OUT=results/x bench/redis.sh 3
#
# Requires: cargo, redis-server (7.x), python3. Results: bench/results/redis/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
CLIENT_CPU=${CLIENT_CPU:-3}
SERVER_CPUS=${SERVER_CPUS:-0,1,2}
TASKS=${TASKS:-64}
PORT=${PORT:-6391}
WORKLOADS=${WORKLOADS:-get set get8k mget}
CLIENTS=${CLIENTS:-giong redis-rs fred giong-4 redis-rs-4 fred-4}
OUT=${OUT:-results/redis}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
export CLK_TCK=$(getconf CLK_TCK)

echo "building..." >&2
for d in redis-giong redis-tokio; do (cd "$d" && cargo build --release -q); done

TMP=$(mktemp -d)
cleanup() {
  redis-cli -p "$PORT" shutdown nosave >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT
taskset -c "$SERVER_CPUS" redis-server --port "$PORT" --bind 127.0.0.1 --save "" \
  --appendonly no --dir "$TMP" --io-threads 3 --io-threads-do-reads yes \
  --daemonize yes --logfile "$TMP/redis.log"
sleep 1
redis-cli -p "$PORT" ping >/dev/null

# Busy ticks of the server's cores, from /proc/stat.
server_ticks() {
  awk -v cpus=",$SERVER_CPUS," '$1 ~ /^cpu[0-9]+$/ && index(cpus, "," substr($1, 4) ",") {
    busy += $2 + $3 + $4 + $7 + $8 } END { print busy }' /proc/stat
}

echo "client,workload,round,ops,p50,p99,cpu,srv" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for workload in $WORKLOADS; do
    for client in $CLIENTS; do
      case $client in
        giong) cmd=(env POOL_SIZE=1 redis-giong/target/release/bench-redis-giong "$PORT") ;;
        giong-4) cmd=(env POOL_SIZE=4 redis-giong/target/release/bench-redis-giong "$PORT") ;;
        redis-rs|fred) cmd=(env POOL_SIZE=1 redis-tokio/target/release/bench-redis-tokio "$client" "$PORT") ;;
        redis-rs-4|fred-4) cmd=(env POOL_SIZE=4 redis-tokio/target/release/bench-redis-tokio "${client%-4}" "$PORT") ;;
      esac
      out_file="$TMP/out"
      taskset -c "$CLIENT_CPU" "${cmd[@]}" "$workload" "$TASKS" "$SECS" >"$out_file" &
      cpid=$!
      sleep 2.2
      before=$(server_ticks)
      sleep "$SECS"
      after=$(server_ticks)
      wait "$cpid"
      out=$(cat "$out_file")
      srv=$(( (after - before) * 100 / (CLK_TCK * SECS) ))
      get() { sed -n "s/.*$1=\([0-9.]*\).*/\1/p" <<<"$out"; }
      echo "$client,$workload,$round,$(get ops),$(get p50_us),$(get p99_us),$(get cpu),$srv" >> "$CSV"
    done
  done
  echo "round $round/$ROUNDS done" >&2
done

python3 - "$CSV" "$TASKS" "$ROUNDS" "$SECS" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
med = statistics.median
print(f"{sys.argv[2]} tasks, {sys.argv[3]} rounds of {sys.argv[4]} s; median, min and max of the rounds")
print(f"{'workload':<9}{'client':<12}{'ops/s median':>13}{'min':>9}{'max':>9}{'p50 us':>8}{'p99 us':>8}{'cpu%':>6}{'us/op':>7}{'srv%':>6}")
for w in ("get", "set", "get8k", "mget"):
    for c in ("giong", "redis-rs", "fred", "giong-4", "redis-rs-4", "fred-4"):
        sel = [r for r in rows if (r["workload"], r["client"]) == (w, c)]
        if not sel: continue
        ops = [float(r["ops"]) for r in sel]
        cpu = med(float(r["cpu"]) for r in sel)
        print(f"{w:<9}{c:<12}{med(ops):13.0f}{min(ops):9.0f}{max(ops):9.0f}"
              f"{med(float(r['p50']) for r in sel):8.0f}{med(float(r['p99']) for r in sel):8.0f}"
              f"{cpu:6.0f}{cpu * 1e4 / med(ops):7.2f}{med(float(r['srv']) for r in sel):6.0f}")
    print()
PY
