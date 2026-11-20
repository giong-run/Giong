#!/bin/bash
. /root/.cargo/env
cd /root/udp-next/bench
for sc in 1-2:4-7 1-4:8-15; do
  SERVER_CPU=${sc%%:*} CLIENT_CPUS=${sc#*:} OUT=/root/q1/udp-next-cpus${sc%%:*} bash ./udp-echo-multicore.sh 5 8 >/root/q1/udp-next-cpus${sc%%:*}.log 2>&1
  echo "$(date -u +%FT%TZ) udp-next server cpus ${sc%%:*} exit $?" >>/root/q1/progress.log
done
