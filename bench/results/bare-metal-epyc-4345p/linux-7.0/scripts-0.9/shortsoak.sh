#!/bin/bash
# Short RC soak: tests/soak.rs for SECS (default 4 h) on commit WANT, both
# drivers at once in separate processes and checkouts. Never pause it.
set -u
. /root/.cargo/env
ulimit -n 1048576
WANT=${WANT:?commit}
SECS=${SECS:-14400}
L=/root/shortsoak-$WANT; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f"; done
for d in io_uring epoll; do
  (cd /root && rm -rf ss-$d && git clone -q https://github.com/giong-run/Giong.git ss-$d && cd ss-$d && git checkout -q --detach $WANT && cargo test --release --quiet --test soak --no-run >$L/build-$d.log 2>&1) || { log "build $d failed"; exit 1; }
done
echo "commit $(git -C /root/ss-epoll rev-parse HEAD) tree $(git -C /root/ss-epoll rev-parse HEAD^{tree}); $(uname -r); $(ethtool -l enp1s0f1np1 | tail -4 | grep Combined | tr -s "\t " " "); GIONG_SOAK_SECS=$SECS; both drivers at once on br-2" >$L/setup.txt
for d in io_uring epoll; do
  ( cd /root/ss-$d; [ $d = epoll ] && export GIONG_DRIVER=epoll
    log "soak $d start"
    GIONG_SOAK_SECS=$SECS cargo test --release --quiet --test soak -- --ignored --nocapture >$L/soak-$d.log 2>&1
    log "soak $d exit $?: $(grep "test result" $L/soak-$d.log | tail -1)" ) &
done
wait
log "all done"
