#!/bin/bash
# 0.9 regression pass on giong 0.8.0 (main b6d9b4b): the same runs as the
# final 0.5.0 pass (bench/results/bare-metal-epyc-4345p/linux-7.0/release-0.5.0)
# plus fs. br-1 (10.40.96.4) is the server, br-2 (10.40.96.3) the load
# generator over the private network. Kernel 7.0, irdma removed, 16 queues.
set -u
. /root/.cargo/env
ulimit -n 1048576
export CARGO_NET_RETRY=10
L=/root/reg09; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
WANT=${WANT:-b6d9b4b}
P=10.40.96.3
for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f"; done
ssh root@$P 'for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance > $f; done'
modprobe tls
cd /root && rm -rf giong-09 && git clone -q https://github.com/giong-run/Giong.git giong-09 && cd giong-09
git checkout -q --detach $WANT
log "at $(git log --oneline -1), tree $(git rev-parse 'HEAD^{tree}')"
R=results/bare-metal-epyc-4345p/linux-7.0/release-0.8.0
{
  echo "commit $(git rev-parse HEAD) tree $(git rev-parse 'HEAD^{tree}')"
  echo "br-1: $(uname -r), ulimit -l $(ulimit -l), ulimit -n $(ulimit -n), $(ethtool -l enp1s0f1np1 | tail -4 | grep Combined | tr -s '\t ' ' '), irdma loaded: $(lsmod | grep -c '^irdma'), EPP $(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference), tls module: $(test -d /sys/module/tls && echo yes)"
  echo "br-2: $(ssh root@$P 'echo "$(uname -r), ulimit -l $(ulimit -l), $(ethtool -l enp1s0f1np1 | tail -4 | grep Combined | tr -s "\t " " "), irdma loaded: $(lsmod | grep -c ^irdma), EPP $(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference)"')"
  echo "rustc: $(rustc -V); wrk: $(wrk -v 2>&1 | head -1); h2load: $(h2load --version 2>&1 | head -1)"
} >$L/setup.txt 2>&1
cd bench; mkdir -p $R; cp $L/setup.txt $R/

ssh root@$P 'rm -f /root/wrk-cpu.log'
BENCH_HOST=10.40.96.4 LOAD_HOST=root@$P LOAD_THREADS=16 SERVER_CPUS=0-15 \
  SERVERS="giong giong-http tokio-raw monoio-tuned" WORKERS="1 2 4 8 16" \
  OUT=$R/workers ./workers.sh 3 6 >$L/workers.log 2>&1
log "workers exit $?"
scp -q root@$P:/root/wrk-cpu.log $R/workers/wrk-cpu.log

ssh root@$P 'rm -f /root/wrk-cpu.log'
BENCH_HOST=10.40.96.4 LOAD_HOST=root@$P OUT=$R/uring-features ./uring-features.sh 3 10 >$L/uring.log 2>&1
log "uring-features exit $?"
scp -q root@$P:/root/wrk-cpu.log $R/uring-features/wrk-cpu.log

OUT=$R/h2 ./h2.sh 3 5 >$L/h2.log 2>&1;  log "h2 exit $?"
SERVERS="axum-tls giong-http-tls giong-http-ktls" ENDPOINTS="json users handshake bytes1m file1m" \
  OUT=$R/tls ./tls.sh 3 5 >$L/tls.log 2>&1; log "tls exit $?"
OUT=$R/fs ./fs.sh >$L/fs.log 2>&1; log "fs exit $?"
cp $L/progress.log $R/
log "all done"
