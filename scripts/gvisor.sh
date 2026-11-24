#!/usr/bin/env bash
# giong under gVisor (runsc), the user-space kernel of GKE Sandbox and of many
# sandboxes that run code for AI agents: no io_uring, so the epoll driver, and
# a network stack of its own, which records SO_REUSEPORT when a socket binds.
#
#   scripts/gvisor.sh [TEST-BINARY-LIST]
#
# TEST-BINARY-LIST is a file with one test binary per line (CI's
# target/test-binaries.txt); by default the giong and giong-http ones are
# built. The tests that bind one port from several workers run in a
# container under runsc, and so does giong-http's `defaults` example with four
# workers, which must answer. Needs Docker and sudo; installs runsc (pinned
# below, checked against its SHA-512) and registers it with Docker unless
# Docker already has a `runsc` runtime.
#
# Left out, as tests of Linux that gVisor does not imitate: udp's
# dropping_a_socket_closes_it (SO_COOKIE), net's backlog test (somaxconn is
# 1024 under gVisor), and the send_with_control tests (control messages).
set -euo pipefail
cd "$(dirname "$0")/.."

release=20261116
sha512=c87d792d5842efb70c38ee822124cfe0821a547715e7abcf3f2b14997cd02784edaa0b91e238ec5c27a25a1b9e00bf98113398eb62a0778d8d20d4e222f8b0e0
image="${IMAGE:-ubuntu:24.04}"
# Pulled first, with retries: `docker run` would pull it once, without any.
docker image inspect "$image" > /dev/null 2>&1 ||
    scripts/ci-net.sh retry -n 5 -t 300 -- docker pull -q "$image" > /dev/null

if ! docker info --format '{{json .Runtimes}}' | grep -q '"runsc"'; then
    tmp="$(mktemp -d)"
    scripts/ci-net.sh retry -n 4 -t 300 -- curl -fsSL --retry 3 --retry-all-errors \
        --connect-timeout 20 -o "$tmp/gvisor.tar.bz2" \
        "https://storage.googleapis.com/gvisor/releases/release/$release/x86_64/gvisor.tar.bz2"
    echo "$sha512  $tmp/gvisor.tar.bz2" | sha512sum -c -
    tar -xjf "$tmp/gvisor.tar.bz2" -C "$tmp"
    sudo install -m 755 "$tmp/runsc" /usr/local/bin/runsc
    sudo cp -r "$tmp/gvisor-bin" /usr/local/bin/
    sudo /usr/local/bin/runsc install
    sudo systemctl restart docker
    rm -rf "$tmp"
fi

list="${1:-}"
if [ -z "$list" ]; then
    list=target/gvisor-test-binaries.txt
    # The same command and features as CI's shared release build, so that it
    # is found up to date there; then only giong's and giong-http's binaries.
    cargo test --release --workspace --all-features --no-run --message-format=json |
        python3 -c '
import json, os, sys
mine = {os.getcwd(), os.path.join(os.getcwd(), "giong-http")}
for line in sys.stdin:
    msg = json.loads(line)
    if (msg.get("reason") == "compiler-artifact" and msg.get("executable")
            and msg["profile"]["test"] and os.path.dirname(msg["manifest_path"]) in mine):
        print(msg["executable"])
' > "$list"
fi
cargo build --release --workspace --all-features --example defaults

# The tests that share ports between workers or bind them again at once,
# the runtime's own tests, and giong-http's servers. Any directory: CI's shared
# release build keeps its test binaries outside target/release/deps.
pick() { grep -E "/($1)-[0-9a-f]+$" "$list" || { echo "no test binary $1 in $list" >&2; exit 2; }; }
tests="$(pick 'giong|balance|multi|proc_readers|udp|unix|shutdown|server|streaming')"

docker run --rm --runtime=runsc -v "$PWD:$PWD" -w "$PWD" \
    -e GIONG_TEST_EXPECT_DRIVER=epoll -e GIONG_TEST_GVISOR=1 "$image" sh -ec "
        for t in $(echo $tests); do
            echo \"== \$t\"
            \"\$t\" -q --skip dropping_a_socket_closes_it
        done"

echo "== the defaults example, four workers on one port"
name=giong-gvisor
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --runtime=runsc --name "$name" -p 127.0.0.1:18082:18082 -e GIONG_WORKERS=4 \
    -v "$PWD/target/release/examples/defaults:/srv/server:ro" "$image" /srv/server 0.0.0.0:18082 >/dev/null
ok=0
for _ in $(seq 50); do
    if [ "$(curl -s -m 1 http://127.0.0.1:18082/ || true)" = ok ]; then ok=1; break; fi
    sleep 0.2
done
logs="$(docker logs "$name" 2>&1)"
running="$(docker inspect -f '{{.State.Running}}' "$name")"
docker rm -f "$name" >/dev/null
if [ "$ok" != 1 ] || [ "$running" != true ]; then
    echo "FAILED: the server did not come up with four workers (running=$running)"
    echo "$logs" | tail -20
    exit 1
fi
echo "answered, all four workers up"
