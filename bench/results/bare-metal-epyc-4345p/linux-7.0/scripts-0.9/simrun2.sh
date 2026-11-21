#!/bin/bash
# Sim pass 2 rerun on main b07333d (#160: deterministic sweeps).
. /root/.cargo/env
L=/root/simrun2; mkdir -p $L
cd /root && rm -rf giong-sim2 && git clone -q https://github.com/giong-run/Giong.git giong-sim2 && cd giong-sim2 && git checkout -q --detach b07333d674b8c64e37bdc56acc8bd7022da17b99 && cd sim
git log --oneline -1 >$L/commit.txt
echo "$(date -u +%FT%TZ) start seeds=64000 days=29200" >>$L/progress.log
GIONG_SIM_SEEDS=64000 GIONG_SIM_SOAK_DAYS=29200 cargo test --release --no-fail-fast -- --nocapture >$L/pass-64000.log 2>&1
echo "$(date -u +%FT%TZ) exit $?: $(grep -E "^test result" $L/pass-64000.log | awk "{p+=\$4; f+=\$6} END {print p \" passed, \" f \" failed\"}"); $(grep -E "^soak:" $L/pass-64000.log | tail -1)" >>$L/progress.log
