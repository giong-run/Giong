#!/usr/bin/env bash
# Idle connections against giong-http with its defaults, in a memory-limited
# container: past what fits, the server must refuse new connections instead
# of being killed for memory, and keep serving the ones it has.
#
#   scripts/idle-connections.sh [SERVER]
#
# SERVER is the `defaults` example of giong-http (by default
# target/release/examples/defaults). For each run in RUNS (workers:driver),
# it starts the server in a Docker container with MEMORY of memory and no
# swap, then scripts/idle-connections.py opens up to CONNS idle keep-alive
# connections until the server refuses them with `503` (GIONG-E1121,
# Server::min_free_memory) and checks that the held ones are still served
# and that new ones are taken again once those close. A run fails if the
# container is OOM-killed or stops, or if the client's checks fail.
#
# The default memory is too small for the default connection limit (10,000
# per worker): about 6 KiB per idle connection, kernel socket memory
# included, which cgroup v2 counts. Without the check, the server is killed
# at about 7,000 connections in 48 MiB.
#
# IMAGE, RUNS and the io_uring settings are as in scripts/slow-uploads.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

server="$(realpath "${1:-target/release/examples/defaults}")"
image="${IMAGE:-ubuntu:24.04}"
memory="${MEMORY:-48m}"
runs="${RUNS:-1:epoll 4:epoll 1:io_uring 4:io_uring}"
conns="${CONNS:-30000}"
port="${PORT:-18081}"
nofile="${NOFILE:-65536}"
name=giong-idle-connections

[ -x "$server" ] || { echo "no server binary at $server" >&2; exit 2; }
# The client holds every connection open.
ulimit -n "$(ulimit -Hn)"

failed=0
for run in $runs; do
    workers="${run%%:*}"
    driver="${run#*:}"
    extra=()
    if [ "$driver" = io_uring ]; then
        extra=(--security-opt seccomp=unconfined --ulimit memlock=-1)
    fi
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker run -d --name "$name" --network host \
        --memory "$memory" --memory-swap "$memory" --ulimit "nofile=$nofile:$nofile" "${extra[@]}" \
        -e GIONG_WORKERS="$workers" -e GIONG_DRIVER="$driver" \
        -v "$server:/srv/server:ro" "$image" /srv/server "127.0.0.1:$port" >/dev/null
    for _ in $(seq 100); do
        if python3 -c "import socket; socket.create_connection(('127.0.0.1', $port), 0.2)" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done

    echo "== $workers worker(s), $driver, $memory: up to $conns idle connections"
    client=0
    python3 scripts/idle-connections.py "$port" "$conns" || client=$?

    state="$(docker inspect -f '{{.State.Running}} {{.State.OOMKilled}}' "$name")"
    peak="$(docker exec "$name" sh -c 'cat /sys/fs/cgroup/memory.peak 2>/dev/null ||
        cat /sys/fs/cgroup/memory/memory.max_usage_in_bytes' 2>/dev/null || echo 0)"
    echo "container: running=${state% *} oom_killed=${state#* }, peak $((peak >> 20)) MiB"
    if [ "$state" != "true false" ]; then
        echo "FAILED: the server did not survive"
        docker logs "$name" 2>&1 | tail -20
        failed=1
    elif [ "$client" != 0 ]; then
        echo "FAILED: see the client's lines above"
        docker logs "$name" 2>&1 | tail -20
        failed=1
    else
        docker logs "$name" 2>&1 | grep -m1 GIONG-E1121 || true
    fi
    docker rm -f "$name" >/dev/null
done
exit "$failed"
