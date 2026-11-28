#!/usr/bin/env bash
# Where giong's 64 KiB uploads lose to Tokio's hand-written loop on bare
# metal (the lab's `payload` row: 0.75 of tokio-raw on the 2026-11-27
# rental, while the two are level on a 4-vCPU VM, so the gap needs a machine
# whose loopback costs what bare metal's does).
#
#   bench/lab/profile-upload.sh [seconds] [sizes]
#
# For each server (giong on io_uring, giong on epoll, tokio-raw) and each
# upload size (default 65536 and 1048576 bytes): the lab's load generator
# posts bodies of that size to /upload over 16 connections, server on
# SERVER_CPU, load on LOAD_CPUS, as the lab's `payload` row. Over the
# measured window:
#   perf-stat-*.txt     the server's syscalls, io_uring submissions and
#                       completions, cycles, instructions, context switches,
#                       and the loopback's softirq time (the `-a` run)
#   perf-*.data/.txt    a profile of the whole machine with call graphs
#                       (`perf report --no-children --sort dso,sym`)
#   loadgen-*.json      the load generator's own result (req/s, latency)
# Divide by the requests in loadgen-*.json for per-upload costs.
# Needs: root, perf for the running kernel, cargo. Output: OUT (default
# bench/results/upload-profile/<host>-<date>/).
set -euo pipefail
cd "$(dirname "$0")"
SECS=${1:-10}
SIZES=${2:-65536 1048576}
SERVER_CPU=${SERVER_CPU:-2}
LOAD_CPUS=${LOAD_CPUS:-4-7}
LOAD_THREADS=${LOAD_THREADS:-4}
WARMUP=${WARMUP:-3}
OUT=${OUT:-../results/upload-profile/$(hostname)-$(date -u +%Y%m%d)}
mkdir -p "$OUT"
LOADGEN=loadgen/target/release/lab-loadgen
(cd loadgen && cargo build --release -q)
(cd ../giong && cargo build --release -q)
(cd ../tokio-raw && cargo build --release -q)
{
  uname -a
  lscpu | grep -E 'Model name|^CPU\(s\)|Thread|NUMA node\(s\)'
  cat /sys/devices/system/cpu/smt/active 2>/dev/null | sed 's/^/smt active: /'
  sysctl net.core.rmem_max net.core.wmem_max net.ipv4.tcp_rmem net.ipv4.tcp_wmem
  ip link show lo | head -1
} > "$OUT/env.txt" 2>&1

server() { # name -> command
  case $1 in
    giong) echo "../giong/target/release/bench-giong 3006" ;;
    giong-epoll) echo "env GIONG_DRIVER=epoll ../giong/target/release/bench-giong 3006" ;;
    tokio) echo "../tokio-raw/target/release/bench-tokio-raw 3005" ;;
  esac
}

for size in $SIZES; do
  for s in giong giong-epoll tokio; do
    read -r -a cmd <<<"$(server "$s")"
    port=${cmd[-1]}; unset 'cmd[-1]'
    taskset -c "$SERVER_CPU" "${cmd[@]}" >/dev/null 2>&1 &
    pid=$!
    for _ in $(seq 1 100); do
      (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break
      sleep 0.1
    done
    tag="$s-$size"
    taskset -c "$LOAD_CPUS" "$LOADGEN" --addr "127.0.0.1:$port" --threads "$LOAD_THREADS" \
      --duration $((SECS * 2 + 4)) --warmup "$WARMUP" --conns 16 --mix /upload --body "$size" \
      > "$OUT/loadgen-$tag.json" &
    lg=$!
    sleep "$WARMUP"
    # Server counters, then the whole machine (loopback work runs in
    # softirq context, often on the loadgen's CPUs).
    perf stat -x, -o "$OUT/perf-stat-$tag.txt" -p "$pid" \
      -e task-clock,context-switches,cycles,instructions,raw_syscalls:sys_enter \
      -e io_uring:io_uring_submit_req,io_uring:io_uring_complete \
      -- sleep "$SECS"
    perf record -q -a -g -o "$OUT/perf-$tag.data" -- sleep "$SECS" 2>/dev/null
    wait "$lg"
    kill "$pid"; wait "$pid" 2>/dev/null || true
    perf report -i "$OUT/perf-$tag.data" --no-children --sort dso,sym --stdio 2>/dev/null \
      | head -150 > "$OUT/perf-$tag.txt"
    echo "$tag: $(tail -1 "$OUT/loadgen-$tag.json")"
  done
done
echo "results in $OUT (perf-*.data are large: keep the .txt reports in the repository)"
