#!/bin/bash
# Soaks of main (the /root/b/main checkout from build.sh), never on a machine that is
# measuring at the same time:
#
#   SECS=10800 bash soak.sh <suite>:<cpus> [<suite>:<cpus> ...]
#   e.g. on br-2: bash soak.sh core:0-3,8-11 core-epoll:4-7,12-15
#
#   core        tests/soak.rs: TCP echo with clean closes, aborts, half-closes and cancelled
#               receives, UDP, files, cross-worker traffic; checks descriptors, memory and
#               that every driver is idle at the end
#   core-epoll  the same on the epoll driver (GIONG_DRIVER=epoll)
#   tls         giong-tls/tests/soak.rs; tls-epoll on epoll
#   grpc        giong-grpc/tests/soak.rs
#   h3          giong-http/tests/h3_soak.rs (HTTP/3 over giong-quic)
#
# The suites run at the same time, each pinned to its CPUs (one worker per CPU), for SECS
# seconds (default 3 hours). They are built first, one after the other, and run as test
# binaries, so no cargo lock is shared while they run. Results: /root/r15/out/soak/.
D=$(cd "$(dirname "$0")" && pwd)
. "$D/env.sh"
SECS=${SECS:-10800}
O=$R15/out/soak
mkdir -p "$O"
cd "$B/main" || exit 2
performance
dmesg >"$O/dmesg-before-$(hostname).txt" 2>&1
echo "# soak.sh $(date -u +%FT%TZ) on $(hostname) $(uname -r): main $(git rev-parse HEAD), GIONG_SOAK_SECS=$SECS, $*" >>"$O/conditions.txt"
pids=()
for spec in "$@"; do
  suite=${spec%%:*} cpus=${spec#*:}
  envs=() pkg=giong test=soak feat=()
  case $suite in
    core) ;;
    core-epoll) envs=(GIONG_DRIVER=epoll) ;;
    tls) pkg=giong-tls ;;
    tls-epoll) pkg=giong-tls envs=(GIONG_DRIVER=epoll) ;;
    grpc) pkg=giong-grpc ;;
    h3) pkg=giong-http test=h3_soak feat=(--all-features) ;;
    *) log "soak: unknown suite $suite"; continue ;;
  esac
  bin=$(cargo test --release -p "$pkg" "${feat[@]}" --test "$test" --no-run --message-format=json 2>>"$O/build.log" |
    jq -r 'select(.executable != null and .target.name == "'"$test"'") | .executable' | tail -1)
  [ -x "$bin" ] || { log "soak: build of $suite failed (see $O/build.log)"; continue; }
  dir=$B/main; [ "$pkg" != giong ] && dir=$B/main/$pkg
  (
    cd "$dir"
    log "soak $suite: start on CPUs $cpus ($bin)"
    env "${envs[@]}" GIONG_SOAK_SECS="$SECS" taskset -c "$cpus" "$bin" --ignored --nocapture --test-threads 1 >"$O/$suite.log" 2>&1
    rc=$?
    log "soak $suite: exit $rc: $(grep 'test result' "$O/$suite.log" | tail -1)"
  ) &
  pids+=($!)
done
wait "${pids[@]}"
dmesg >"$O/dmesg-after-$(hostname).txt" 2>&1
diff "$O/dmesg-before-$(hostname).txt" "$O/dmesg-after-$(hostname).txt" | grep '^>' >"$O/dmesg-new-$(hostname).txt"
log "soak: all done; $(wc -l <"$O/dmesg-new-$(hostname).txt") new kernel log lines"
