#!/bin/bash
# The one-machine part of the run, in tmux, after build.sh: bash local.sh <parts...>
# On br-1 once run.sh is done: h2 tls fs mem httpc. On br-2 at the same time: spin placement lab.
#
#   h2         bench/h2.sh at main: HTTP/2 throughput and memory, giong-http against axum
#   tls        bench/tls.sh at main: axum-tls, giong-http-tls, giong-http-ktls; json, users,
#              handshake, 1 MB bodies and files
#   fs         bench/fs.sh at main
#   mem        memory per idle connection across builds: bench/tls-idle.sh (HTTP/1.1 and h2
#              over TLS) with v1.5.1, main and PR #487; h2.sh's memory part in each checkout
#   httpc      bench/http-client.sh in the v1.5.1 and main checkouts (#480)
#   spin       the spin governor (PR #186, closed) against the fixed spin of its base
#              5a4cdb3 and no spin, CLIENT=blocking and busy: bench/spin.sh of the PR
#   placement  PR #180 against its base d4cc995: bench/placement.sh of the PR, BEFORE=base
#   lab        bench/lab/lab.py at main, every scenario
#
# spin, placement and lab pin servers and load generators to CPUs of their own; with SMT on,
# CPU n and n+8 share a core, so these parts turn SMT off for their run (and on again after).
D=$(cd "$(dirname "$0")" && pwd)
. "$D/env.sh"
O=$R15/out
mkdir -p "$O"
performance
smt() { [ -w /sys/devices/system/cpu/smt/control ] && echo "$1" >/sys/devices/system/cpu/smt/control && log "SMT $1: $(nproc) CPUs online"; }
for part in "$@"; do
  log "local part $part: start on $(hostname)"
  case $part in
    h2) (cd "$B/main/bench" && OUT=$O/h2 ./h2.sh 3 5 >"$O/h2.log" 2>&1; log "h2 exit $?") ;;
    tls)
      (cd "$B/main/bench" && SERVERS="axum-tls giong-http-tls giong-http-ktls" ENDPOINTS="json users handshake bytes1m file1m" \
        OUT=$O/tls ./tls.sh 3 5 >"$O/tls.log" 2>&1; log "tls exit $?")
      ;;
    fs) (cd "$B/main/bench" && OUT=$O/fs ./fs.sh >"$O/fs.log" 2>&1; log "fs exit $?") ;;
    mem)
      bins="v1.5.1=$B/bins/v151-giong-http main=$B/bins/main-giong-http"
      [ -x "$B/bins/pr487-giong-http" ] && bins="$bins pr487=$B/bins/pr487-giong-http"
      (cd "$B/main/bench" &&
        BINS="$bins" OUT=$O/mem/tls-idle-http1 ./tls-idle.sh 3 8000 >"$O/mem-tls-http1.log" 2>&1
        log "mem: tls-idle http/1.1 exit $?"
        BINS="$bins" PROTOCOL=h2 OUT=$O/mem/tls-idle-h2 ./tls-idle.sh 3 8000 >"$O/mem-tls-h2.log" 2>&1
        log "mem: tls-idle h2 exit $?")
      for c in v151 main pr487; do
        [ -d "$B/$c/bench" ] || continue
        [ $c = pr487 ] && [ ! -x "$B/bins/pr487-giong-http" ] && continue
        (cd "$B/$c/bench" && PARTS=memory MEM_SERVERS=giong-http OUT=$O/mem/h2-$c ./h2.sh 3 >"$O/mem-h2-$c.log" 2>&1)
        log "mem: h2.sh memory at $c exit $?"
      done
      ;;
    httpc)
      for c in v151 main; do
        (cd "$B/$c/bench" && OUT=$O/httpc-$c ./http-client.sh 3 5 >"$O/httpc-$c.log" 2>&1)
        log "httpc at $c exit $?"
      done
      ;;
    spin)
      smt off
      for client in blocking busy; do
        (cd "$B/pr186/bench" && BASE_REF=5a4cdb35c629c5c76604738feab3f9f32788d79a CLIENT=$client \
          OUT=$O/spin-$client ./spin.sh 3 10 >"$O/spin-$client.log" 2>&1)
        log "spin $client exit $?"
      done
      smt on
      ;;
    placement)
      smt off
      (cd "$B/pr180/bench" && BEFORE=$B/pr180-base OUT=$O/placement ./placement.sh 5 4 >"$O/placement.log" 2>&1)
      log "placement exit $?"
      smt on
      ;;
    lab)
      smt off
      (cd "$B/main/bench" && python3 lab/lab.py --rounds 3 --out "$O/lab" >"$O/lab.log" 2>&1)
      log "lab exit $?"
      smt on
      ;;
    *) log "unknown part $part" ;;
  esac
  log "local part $part: done"
done
