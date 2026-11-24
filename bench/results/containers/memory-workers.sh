#!/bin/bash
# Memory of giong-http's `defaults` example by worker count, idle and after
# load, on io_uring (seccomp unconfined) or epoll (Docker's default profile).
#
#   bench/results/containers/memory-workers.sh <workers> <unconfined|default>
#
# Needs Docker, wrk, curl, and `cargo build --release -p giong-http --example
# defaults` (or EX=<dir with the example>). Reads the container's cgroup v1
# memory.usage_in_bytes. Results: memory-workers.txt.
W=$1; S=$2; shift 2
EX="${EX:-$(cd "$(dirname "$0")/../../.." && pwd)/target/release/examples}"
SEC=""; [ "$S" = unconfined ] && SEC="--security-opt seccomp=unconfined"
docker rm -f gg >/dev/null 2>&1
docker run -d --name gg --network host $SEC -e GIONG_WORKERS=$W "$@" -v $EX/defaults:/app:ro ubuntu:24.04 /app 127.0.0.1:18080 >/dev/null
for i in $(seq 100); do curl -s localhost:18080/ >/dev/null && break; sleep 0.2; done # -s localhost:18080/ >/dev/null; do sleep 0.2; done
rss(){ docker exec gg grep VmRSS /proc/1/status | awk '{print $2}'; }
cg(){ id=$(docker inspect -f "{{.Id}}" gg); echo $(( $(cat /sys/fs/cgroup/memory/docker/$id/memory.usage_in_bytes) / 1024 )); }
idle=$(rss); idlecg=$(cg)
out=$(wrk -t4 -c256 -d8s http://127.0.0.1:18080/ 2>&1)
rps=$(echo "$out" | awk '/Requests\/sec/{print $2}')
after=$(rss); aftercg=$(cg)
drv=$(docker logs gg 2>&1 | grep -c "epoll driver")
echo "workers=$W seccomp=$S $* | idle_rss_kib=$idle idle_cg_kib=$idlecg | rps=$rps | after_rss_kib=$after after_cg_kib=$aftercg | fallback_msgs=$drv"
docker rm -f gg >/dev/null
