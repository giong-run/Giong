#!/bin/bash
# Throughput and latency of giong-http's `defaults` example in a container with
# a CPU quota, with the worker count giong picks (auto) or a forced one.
#
#   bench/results/containers/cpu-quota.sh <cpus> <workers|auto>
#
# Needs Docker, wrk, curl, and `cargo build --release -p giong-http --example
# defaults` (or EX=<dir with the example>). Reads nr_throttled from the
# container's cgroup v1 cpu.stat. Results: cpu-quota.txt.
C=$1; W=$2; EX="${EX:-$(cd "$(dirname "$0")/../../.." && pwd)/target/release/examples}"
ENV=""; [ "$W" != auto ] && ENV="-e GIONG_WORKERS=$W"
docker rm -f gg >/dev/null 2>&1
docker run -d --name gg --network host --security-opt seccomp=unconfined --cpus $C $ENV -v $EX/defaults:/app:ro ubuntu:24.04 /app 127.0.0.1:18080 >/dev/null
for i in $(seq 100); do curl -s localhost:18080/ >/dev/null && break; sleep 0.2; done
id=$(docker inspect -f '{{.Id}}' gg); T=/sys/fs/cgroup/cpu/docker/$id/cpu.stat
t0=$(awk '/nr_throttled/{print $2}' $T)
out=$(wrk -t2 -c64 -d10s --latency http://127.0.0.1:18080/ 2>&1)
t1=$(awk '/nr_throttled/{print $2}' $T)
echo "cpus=$C workers=$W rps=$(echo "$out" | awk '/Requests\/sec/{print $2}') p50=$(echo "$out" | awk '$1=="50%"{print $2}') p99=$(echo "$out" | awk '$1=="99%"{print $2}') throttled_periods=$((t1-t0))"
docker rm -f gg >/dev/null
