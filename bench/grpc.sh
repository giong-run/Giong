#!/usr/bin/env bash
# gRPC benchmark: giong-grpc against tonic (on Tokio and hyper), as a server
# and as a client, with the same service on both.
#
#   bench/grpc.sh [rounds] [seconds-per-measurement]
#
# Method
#   * The service (grpc-giong/proto/bench.proto): `Unary` echoes a message,
#     `Duplex` echoes each message of a bidirectional stream, `Download`
#     streams `count` messages of `size` bytes.
#   * Workloads: `unary` is back-to-back unary calls with a 100-byte
#     payload, `unary10k` the same with 10,000 bytes, `duplex` one round
#     trip after the other of 100-byte messages on a stream that stays
#     open, `download` server streams of 1,000 messages of 100 bytes,
#     counted in messages.
#   * `server`: the server under test has one thread (one giong worker; a
#     current-thread Tokio runtime) on SERVER_CPU. Load comes from one
#     process per CPU of LOAD_CPUS, each with CONNS connections and TASKS
#     concurrent calls or streams, so that the server's core is the limit.
#     Both servers are loaded with both load generators (tonic's client and
#     giong-grpc's). `ops/s` is the sum of the load processes, `p50` the
#     median of theirs, `p99` the worst of theirs; `cpu%` is the server's
#     process (from /proc/<pid>/stat) and `us/op` its CPU time per call or
#     message.
#   * `client`: the client under test is one load process with one thread
#     on CLIENT_CPU, CONNS connections and TASKS tasks, against each server
#     with three threads on PEER_CPUS, so that the client's core is the
#     limit. `cpu%` and `us/op` are the client's.
#   * Both servers have their defaults, but for one setting of giong-http's:
#     `max_body_size(4 MiB)`, gRPC's message limit, which makes the HTTP/2
#     window a stream starts with 20 KiB instead of 5 KiB (it is a share of
#     the body budget), so that a 10,000-byte request needs no
#     WINDOW_UPDATE. hyper's default for tonic is 1 MiB a stream.
#   * `memory`: HOLD_CONNS connections with 100 bidirectional streams each
#     are opened (by tonic's client), one message is exchanged on each, and
#     they stay open: the server's resident memory then, less its resident
#     memory before the first connection, per stream.
#   * 2 s of warm-up, then SECS measured, ROUNDS times, the subjects taking
#     turns within a round.
#
# SIDES, WORKLOADS pick a subset, for example
#   SIDES=server WORKLOADS="duplex download" OUT=results/x bench/grpc.sh 3
#
# Requires: cargo, python3, taskset. Results: bench/results/grpc/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
SERVER_CPU=${SERVER_CPU:-0}
LOAD_CPUS=${LOAD_CPUS:-1 2 3}
CLIENT_CPU=${CLIENT_CPU:-3}
PEER_CPUS=${PEER_CPUS:-0,1,2}
CONNS=${CONNS:-2}
TASKS=${TASKS:-32}
HOLD_CONNS=${HOLD_CONNS:-10}
PORT=${PORT:-7700}
SIDES=${SIDES:-server client memory}
WORKLOADS=${WORKLOADS:-unary unary10k duplex download}
OUT=${OUT:-results/grpc}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
export CLK_TCK=$(getconf CLK_TCK)

echo "building..." >&2
for d in grpc-giong grpc-tonic; do (cd "$d" && cargo build --release -q); done

TMP=$(mktemp -d)
SERVER_PID=
cleanup() {
  [ -n "$SERVER_PID" ] && kill -9 "$SERVER_PID" 2>/dev/null || true
  pkill -9 -P $$ 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

# start_server <giong|tonic> <cpus> <threads>: on a port of its own.
start_server() {
  PORT=$((PORT + 1))
  taskset -c "$2" "grpc-$1/target/release/server" "$PORT" "$3" &
  SERVER_PID=$!
  for _ in $(seq 1 100); do
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && return
    sleep 0.05
  done
  echo "the $1 server did not start" >&2
  exit 1
}
stop_server() {
  kill -9 "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=
}
ticks() { awk '{ sub(/.*\) /, ""); print $12 + $13 }' "/proc/$1/stat"; }
rss_kb() { awk '/^VmRSS/ { print $2 }' "/proc/$1/status"; }
get() { sed -n "s/.*$2=\([0-9.]*\).*/\1/p" "$1"; }

echo "side,subject,peer,workload,round,ops,p50,p99,cpu,us_per_op" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for side in $SIDES; do
    [ "$side" = memory ] && continue
    for workload in $WORKLOADS; do
      for peer in tonic giong; do
        for subject in giong tonic; do
          if [ "$side" = server ]; then
            start_server "$subject" "$SERVER_CPU" 1
            n=0
            pids=()
            for cpu in $LOAD_CPUS; do
              n=$((n + 1))
              taskset -c "$cpu" "grpc-$peer/target/release/load" "$PORT" "$workload" \
                "$CONNS" "$TASKS" "$SECS" >"$TMP/out$n" &
              pids+=($!)
            done
            sleep 2.2
            before=$(ticks "$SERVER_PID")
            sleep "$SECS"
            after=$(ticks "$SERVER_PID")
            wait "${pids[@]}"
            stop_server
            python3 - "$side,$subject,$peer,$workload,$round" "$after" "$before" \
              "$CLK_TCK" "$SECS" "$TMP"/out[0-9]* >> "$CSV" <<'PY'
import re, statistics, sys
key, after, before, hz, secs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4]), float(sys.argv[5])
runs = [dict(re.findall(r"(\w+)=([0-9.]+)", open(p).read())) for p in sys.argv[6:]]
ops = sum(float(r["ops"]) for r in runs)
cpu_s = (after - before) / hz
print(f"{key},{ops:.0f},{statistics.median(float(r['p50_us']) for r in runs):.0f},"
      f"{max(float(r['p99_us']) for r in runs):.0f},{cpu_s * 100 / secs:.0f},{cpu_s * 1e6 / (ops * secs):.3f}")
PY
            rm -f "$TMP"/out[0-9]*
          else
            start_server "$peer" "$PEER_CPUS" 3
            taskset -c "$CLIENT_CPU" "grpc-$subject/target/release/load" "$PORT" "$workload" \
              "$CONNS" "$TASKS" "$SECS" >"$TMP/out"
            stop_server
            ops=$(get "$TMP/out" ops)
            cpu=$(get "$TMP/out" cpu)
            us=$(python3 -c "print(f'{$cpu * 1e4 / $ops:.3f}')")
            echo "$side,$subject,$peer,$workload,$round,$ops,$(get "$TMP/out" p50_us),$(get "$TMP/out" p99_us),$cpu,$us" >> "$CSV"
          fi
        done
      done
    done
  done
  echo "round $round/$ROUNDS done" >&2
done

MEM="$OUT/memory.csv"
if [[ " $SIDES " == *" memory "* ]]; then
  echo "subject,round,streams,idle_kb,held_kb,bytes_per_stream" > "$MEM"
  streams=$((HOLD_CONNS * 100))
  for round in $(seq 1 "$ROUNDS"); do
    for subject in giong tonic; do
      start_server "$subject" "$SERVER_CPU" 1
      idle=$(rss_kb "$SERVER_PID")
      taskset -c "$CLIENT_CPU" grpc-tonic/target/release/load "$PORT" hold \
        "$HOLD_CONNS" "$streams" 1 >"$TMP/hold" &
      hold=$!
      for _ in $(seq 1 200); do
        grep -q ready "$TMP/hold" && break
        sleep 0.1
      done
      grep -q ready "$TMP/hold" || { echo "the streams did not open" >&2; exit 1; }
      sleep 1
      held=$(rss_kb "$SERVER_PID")
      kill -9 "$hold" 2>/dev/null || true
      wait "$hold" 2>/dev/null || true
      stop_server
      echo "$subject,$round,$streams,$idle,$held,$(( (held - idle) * 1024 / streams ))" >> "$MEM"
    done
  done
fi

python3 - "$CSV" "$MEM" "$ROUNDS" "$SECS" "$CONNS" "$TASKS" "$LOAD_CPUS" <<'PY' | tee "$OUT/summary.txt"
import csv, os, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
med = statistics.median
loads = len(sys.argv[7].split())
print(f"{sys.argv[3]} rounds of {sys.argv[4]} s; median, min and max of the rounds; "
      f"{sys.argv[5]} connections and {sys.argv[6]} tasks per load process")
for side, what in (("server", f"one server thread, loaded by {loads} processes of `load`"),
                   ("client", "one client thread, against `peer` with three threads")):
    if not any(r["side"] == side for r in rows): continue
    print(f"\n{side}: {what}")
    other = "load" if side == "server" else "peer"
    print(f"{'workload':<10}{other:<7}{side:<8}{'ops/s median':>13}{'min':>10}{'max':>10}{'p50 us':>8}{'p99 us':>8}{'cpu%':>6}{'us/op':>8}{'vs tonic':>10}")
    for w in ("unary", "unary10k", "duplex", "download"):
        for peer in ("tonic", "giong"):
            base = None
            for subject in ("tonic", "giong"):
                sel = [r for r in rows if (r["side"], r["workload"], r["peer"], r["subject"]) == (side, w, peer, subject)]
                if not sel: continue
                ops = [float(r["ops"]) for r in sel]
                if subject == "tonic": base = med(ops)
                ratio = f"{(med(ops) / base - 1) * 100:+.1f}%" if base and subject != "tonic" else ""
                print(f"{w:<10}{peer:<7}{subject:<8}{med(ops):13.0f}{min(ops):10.0f}{max(ops):10.0f}"
                      f"{med(float(r['p50']) for r in sel):8.0f}{med(float(r['p99']) for r in sel):8.0f}"
                      f"{med(float(r['cpu']) for r in sel):6.0f}{med(float(r['us_per_op']) for r in sel):8.3f}{ratio:>10}")
        print()
if os.path.exists(sys.argv[2]):
    mem = list(csv.DictReader(open(sys.argv[2])))
    if mem:
        print(f"memory: server resident memory per open bidirectional stream ({mem[0]['streams']} streams)")
        print(f"{'server':<8}{'idle KiB':>10}{'held KiB':>10}{'bytes/stream median':>21}{'min':>8}{'max':>8}")
        for subject in ("tonic", "giong"):
            sel = [r for r in mem if r["subject"] == subject]
            per = [float(r["bytes_per_stream"]) for r in sel]
            print(f"{subject:<8}{med(float(r['idle_kb']) for r in sel):10.0f}{med(float(r['held_kb']) for r in sel):10.0f}"
                  f"{med(per):21.0f}{min(per):8.0f}{max(per):8.0f}")
PY
