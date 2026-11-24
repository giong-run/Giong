#!/usr/bin/env bash
# The defaults example of giong-http in a container with 1,024 soft and 20,000 hard open
# files (containerd gives 1,024 soft and 524,288 hard), one worker; open-files.py holds
# 1,100 keep-alive connections, sends a request on each and counts answers.
# Usage: bench/results/containers/open-files.sh <path to the defaults example binary>
set -euo pipefail
bin=$1
docker rm -f giong-nofile >/dev/null 2>&1 || true
docker run -d --name giong-nofile --network host --ulimit nofile=1024:20000 -e GIONG_WORKERS=1 \
  -v "$bin":/app:ro ubuntu:24.04 /app 127.0.0.1:18080 >/dev/null
sleep 1
docker exec giong-nofile sh -c 'grep "open files" /proc/1/limits'
timeout 120 python3 "$(dirname "$0")/open-files.py" 18080 1100
docker rm -f giong-nofile >/dev/null
