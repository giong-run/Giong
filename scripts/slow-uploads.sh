#!/usr/bin/env bash
# Slow uploads against giong-http with its defaults, in a memory-limited
# container: the server must stay up and keep answering.
#
#   scripts/slow-uploads.sh [SERVER]
#
# SERVER is the `defaults` example of giong-http (by default
# target/release/examples/defaults; `cargo build --release -p giong-http
# --example defaults` builds it). For each run in RUNS (workers:driver), it
# starts the server in a Docker container with MEMORY of memory and no swap,
# then scripts/slow-uploads.py opens H1 HTTP/1.1 and H2 HTTP/2 connections
# that each send a 1,000,000-byte body one byte short and hold it, while a
# well-behaved client keeps asking. A run fails if the container is
# OOM-killed or stops, or if a well-behaved request is not answered with 200.
#
# Without the bound on requests being received (Server::
# max_buffered_request_bytes), 130 such uploads got one worker in 128 MiB
# OOM-killed (docs/DEPLOYMENT.md, "Sizing memory").
#
# IMAGE (default ubuntu:24.04) is the container image, which only has to run
# the binary. RUNS defaults to "1:epoll 4:epoll 1:io_uring 4:io_uring"; the
# io_uring runs use a seccomp profile that allows it (Docker's default does
# not) and no locked-memory limit.
set -euo pipefail
cd "$(dirname "$0")/.."

server="$(realpath "${1:-target/release/examples/defaults}")"
image="${IMAGE:-ubuntu:24.04}"
# Pulled first, with retries: `docker run` would pull it once, without any.
docker image inspect "$image" > /dev/null 2>&1 ||
    scripts/ci-net.sh retry -n 5 -t 300 -- docker pull -q "$image" > /dev/null
memory="${MEMORY:-128m}"
runs="${RUNS:-1:epoll 4:epoll 1:io_uring 4:io_uring}"
h1="${H1:-300}"
h2="${H2:-150}"
port="${PORT:-18080}"
name=giong-slow-uploads

[ -x "$server" ] || { echo "no server binary at $server" >&2; exit 2; }

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
        --memory "$memory" --memory-swap "$memory" "${extra[@]}" \
        -e GIONG_WORKERS="$workers" -e GIONG_DRIVER="$driver" \
        -v "$server:/srv/server:ro" "$image" /srv/server "127.0.0.1:$port" >/dev/null
    # Up when it answers.
    for _ in $(seq 100); do
        if python3 -c "import socket; socket.create_connection(('127.0.0.1', $port), 0.2)" 2>/dev/null; then
            break
        fi
        sleep 0.1
    done

    echo "== $workers worker(s), $driver, $memory: $h1 HTTP/1.1 and $h2 HTTP/2 uploads"
    client=0
    python3 scripts/slow-uploads.py "$port" "$h1" "$h2" || client=$?

    state="$(docker inspect -f '{{.State.Running}} {{.State.OOMKilled}}' "$name")"
    # cgroup v2 (memory.peak), or v1 (max_usage_in_bytes).
    peak="$(docker exec "$name" sh -c 'cat /sys/fs/cgroup/memory.peak 2>/dev/null ||
        cat /sys/fs/cgroup/memory/memory.max_usage_in_bytes' 2>/dev/null || echo 0)"
    echo "container: running=${state% *} oom_killed=${state#* }, peak $((peak >> 20)) MiB"
    if [ "$state" != "true false" ]; then
        echo "FAILED: the server did not survive"
        docker logs "$name" 2>&1 | tail -20
        failed=1
    elif [ "$client" != 0 ]; then
        echo "FAILED: a well-behaved request was not answered"
        docker logs "$name" 2>&1 | tail -20
        failed=1
    else
        docker logs "$name" 2>&1 | grep -m1 GIONG-E1120 || true
    fi
    docker rm -f "$name" >/dev/null
done
exit "$failed"
