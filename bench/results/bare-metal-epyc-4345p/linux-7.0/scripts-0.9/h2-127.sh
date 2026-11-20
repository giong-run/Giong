#!/bin/bash
# PR #127 (3bec259) vs 0.8.0 (b6d9b4b) and #125 (003d9e5) with bench/h2.sh on
# br-2, 3 interleaved rounds, while the epoll soak and the sim runs are
# paused with SIGSTOP (and resumed with SIGCONT afterwards).
set -u
. /root/.cargo/env
ulimit -n 1048576
L=/root/h2-127; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
C="b6d9b4b 003d9e5 3bec2590292f6a5d1a325d8f810811c7c37c1f7c"
for c in $C; do
  [ -d /root/h2c-${c:0:7} ] || (cd /root && git clone -q https://github.com/giong-run/Giong.git h2c-${c:0:7} && cd h2c-${c:0:7} && git fetch -q origin perf/h2-handler-queue && git checkout -q --detach $c)
  (cd /root/h2c-${c:0:7}/bench && nice -n 19 bash -c '(cd giong-http && cargo build --release -q) && (cd axum-tls && cargo build --release -q) && (cd axum && cargo build --release -q --features h2 --target-dir target/h2) && (cd hold && cargo build --release -q)') >>$L/build.log 2>&1
done
log "built"
# The soak's test binary, and the sim's cargo with its running test binary
# (a stopped cargo starts no next binary).
simcargo=$(pgrep -f 'cargo test --release -- --nocapture')
pids=$(pgrep -f '[s]oak-[0-9a-f]* --ignored'; echo $simcargo; for p in $simcargo; do pgrep -P $p; done)
pids=$(echo $pids)
log "pausing: $pids ($(ps -o pid=,args= -p ${pids// /,} | cut -c1-80 | tr '\n' ';'))"
kill -STOP $pids
echo "$(date -u +%FT%TZ) paused for the PR #127 h2 check: $pids" >>/root/soak72/progress.log
sleep 5
for r in 1 2 3; do
  for c in $C; do
    (cd /root/h2c-${c:0:7}/bench && OUT=$L/${c:0:7}-r$r ./h2.sh 1 5 >$L/${c:0:7}-r$r.log 2>&1)
    log "${c:0:7} round $r done"
  done
done
kill -CONT $pids
echo "$(date -u +%FT%TZ) resumed after the PR #127 h2 check" >>/root/soak72/progress.log
log "resumed $pids"
