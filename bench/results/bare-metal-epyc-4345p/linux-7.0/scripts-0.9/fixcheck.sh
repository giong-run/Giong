#!/bin/bash
# Short checks of fix/refuse-low-memory (PR #125, FIX) on br-1 before the soak:
# small-box for giong-http with the bench changes of bench/bare-metal-0.9
# merged on top, and the HTTP/2 idle memory A/B against 0.8.0 (b6d9b4b).
set -u
. /root/.cargo/env
ulimit -n 1048576
FIX=${FIX:-674c42c709d27f2a8ee707d9d4fe3c844cdb9c17}
L=/root/fixcheck; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
cd /root && rm -rf giong-fix && git clone -q https://github.com/giong-run/Giong.git giong-fix && cd giong-fix
git fetch -q origin fix/refuse-low-memory bench/bare-metal-0.9 && git checkout -q --detach $FIX
git -c user.name=bench -c user.email=bench@localhost merge -q --no-edit origin/bench/bare-metal-0.9 >$L/merge.log 2>&1 || { log "merge failed"; exit 1; }
log "at $FIX + bench/bare-metal-0.9 ($(git rev-parse --short origin/bench/bare-metal-0.9)) = $(git rev-parse --short HEAD)"
cd bench
LOAD_HOST=root@10.40.96.3 BENCH_IP=10.40.96.4 SERVERS="giong-http giong-http-ktls" \
  OUT=$L/small-box ./small-box.sh 3 10 >$L/small-box.log 2>&1
log "small-box exit $?"
[ -d /root/h2-$FIX ] || (cd /root && git clone -q https://github.com/giong-run/Giong.git h2-$FIX && cd h2-$FIX && git fetch -q origin fix/refuse-low-memory && git checkout -q --detach $FIX)
for r in 1 2 3; do
  for c in b6d9b4b $FIX; do
    (cd /root/h2-$c/bench && OUT=$L/h2-${c:0:7}-r$r ./h2.sh 1 2 >$L/h2-${c:0:7}-r$r.log 2>&1)
    log "h2 ${c:0:7} round $r: $(grep -A3 '^server  *h2-idle' $L/h2-${c:0:7}-r$r/summary.txt | grep giong-http)"
  done
done
log "all done"
