#!/bin/bash
# Sim long runs on current main (sim fix #137 included), after the soaks.
. /root/.cargo/env
L=/root/simrun; mkdir -p $L
cd /root && rm -rf giong-sim-main && git clone -q https://github.com/giong-run/Giong.git giong-sim-main && cd giong-sim-main/sim
git log --oneline -1 >$L/commit.txt
for p in "16000 14600" "64000 29200"; do
  set -- $p
  echo "$(date -u +%FT%TZ) start seeds=$1 days=$2" >>$L/progress.log
  GIONG_SIM_SEEDS=$1 GIONG_SIM_SOAK_DAYS=$2 timeout 7200 cargo test --release -- --nocapture >$L/pass-$1.log 2>&1
  echo "$(date -u +%FT%TZ) exit $? seeds=$1: $(grep -E "^test result" $L/pass-$1.log | awk "{p+=\$4; f+=\$6} END {print p \" passed, \" f \" failed\"}"); $(grep -E "^soak:" $L/pass-$1.log | tail -1)" >>$L/progress.log
done
