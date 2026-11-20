#!/bin/bash
# HTTP/2 over kernel TLS: main 6372cd5 against 0.8.0 (b6d9b4b), bench/h2.sh 1 5
# with a giong-http-ktls server added (GIONG_KTLS=required), 3 interleaved
# rounds on br-2 while the epoll soak and the sim run are paused (SIGSTOP).
set -u
. /root/.cargo/env
ulimit -n 1048576
modprobe tls
L=/root/h2-ktls; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
C="b6d9b4b 6372cd5"
for c in $C; do
  d=/root/h2k-$c
  [ -d $d ] || (cd /root && git clone -q https://github.com/giong-run/Giong.git h2k-$c && cd h2k-$c && git checkout -q --detach $c)
  # The kTLS server, and a choice of servers (not in the repository's h2.sh).
  sed -i -e 's|^NAMES=(giong-http-tls axum-tls giong-http axum)|read -r -a NAMES <<<"${H2_SERVERS:-giong-http-tls axum-tls giong-http axum}"|' \
    -e 's|^  \[giong-http-tls\]="giong-http/target/release/bench-giong-http"|&\n  [giong-http-ktls]="env GIONG_KTLS=required giong-http/target/release/bench-giong-http"|' \
    -e 's|^  \[giong-http-tls\]="https://127.0.0.1:3010"|&\n  [giong-http-ktls]="https://127.0.0.1:3010"|' $d/bench/h2.sh
  grep -c ktls $d/bench/h2.sh >>$L/patch.log
  (cd $d/bench && nice -n 19 bash -c '(cd giong-http && cargo build --release -q) && (cd axum-tls && cargo build --release -q) && (cd axum && cargo build --release -q --features h2 --target-dir target/h2) && (cd hold && cargo build --release -q)') >>$L/build.log 2>&1
done
log "built"
simcargo=$(pgrep -f 'cargo test --release -- --nocapture')
pids=$(echo $(pgrep -f '[s]oak-[0-9a-f]* --ignored'; echo $simcargo; for p in $simcargo; do pgrep -P $p; done))
kill -STOP $pids
echo "$(date -u +%FT%TZ) paused for the HTTP/2 kTLS check: $pids" >>/root/soak72/progress.log
log "paused $pids"
sleep 5
for r in 1 2 3; do
  for c in $C; do
    (cd /root/h2k-$c/bench && H2_SERVERS="giong-http-tls giong-http-ktls" OUT=$L/$c-r$r ./h2.sh 1 5 >$L/$c-r$r.log 2>&1)
    log "$c round $r done"
  done
done
kill -CONT $pids
echo "$(date -u +%FT%TZ) resumed after the HTTP/2 kTLS check" >>/root/soak72/progress.log
log "resumed $pids"
