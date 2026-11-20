#!/bin/bash
# The 0.9 72-hour soak: one uninterrupted run of tests/soak.rs on giong 0.8.0
# (main b6d9b4b), release build. DRIVER (io_uring by default, or epoll) picks
# the driver. With SIM=1 the simulation's long runs (sim/) go on beside it at
# nice 19, in passes of growing size.
set -u
. /root/.cargo/env
ulimit -n 1048576
WANT=${WANT:-b6d9b4b}
SECS=${SECS:-259200}
L=/root/soak72; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f"; done
cd /root && rm -rf giong-soak && git clone -q https://github.com/giong-run/Giong.git giong-soak && cd giong-soak && { [ -z "${FETCH:-}" ] || git fetch -q origin "$FETCH"; } && git checkout -q --detach $WANT || { log "checkout of $WANT failed"; exit 1; }
{
  echo "commit $(git rev-parse HEAD) tree $(git rev-parse 'HEAD^{tree}')"
  echo "$(hostname): $(uname -r), driver ${DRIVER:-io_uring}, ulimit -n $(ulimit -n), ulimit -l $(ulimit -l), $(ethtool -l enp1s0f1np1 | tail -4 | grep Combined | tr -s '\t ' ' '), irdma loaded: $(lsmod | grep -c '^irdma'), EPP $(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference)"
  echo "rustc: $(rustc -V); GIONG_SOAK_SECS=$SECS"
} >$L/setup.txt
cargo test --release --quiet --test soak --no-run >$L/build.log 2>&1
log "soak start ($SECS s, driver ${DRIVER:-io_uring})"
if [ "${DRIVER:-io_uring}" = epoll ]; then export GIONG_DRIVER=epoll; fi
(GIONG_SOAK_SECS=$SECS cargo test --release --quiet --test soak -- --ignored --nocapture >$L/soak.log 2>&1; echo "soak exit $?" >$L/soak.exit; log "soak exit $(cat $L/soak.exit): $(grep 'test result' $L/soak.log | tail -1)") &
unset GIONG_DRIVER
if [ "${SIM:-0}" = 1 ]; then
  cd /root/giong-soak/sim
  seeds=1000; days=3650; pass=0
  while [ ! -f $L/soak.exit ]; do
    pass=$((pass + 1))
    log "sim pass $pass start: GIONG_SIM_SEEDS=$seeds GIONG_SIM_SOAK_DAYS=$days"
    GIONG_SIM_SEEDS=$seeds GIONG_SIM_SOAK_DAYS=$days nice -n 19 cargo test --release -- --nocapture >$L/sim-pass-$pass.log 2>&1
    rc=$?
    log "sim pass $pass exit $rc: $(grep -E '^test result' $L/sim-pass-$pass.log | awk '{p+=$4; f+=$6} END {print p " passed, " f " failed"}'); $(grep -E '^soak:' $L/sim-pass-$pass.log | tail -1)"
    seeds=$((seeds * 4)); days=$((days * 2))
  done
fi
wait
log "all done"
