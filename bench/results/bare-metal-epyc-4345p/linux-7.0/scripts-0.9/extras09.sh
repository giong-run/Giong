#!/bin/bash
# 0.9 bare-metal runs of the three draft branches, on br-1 (10.40.96.4) with
# br-2 (10.40.96.3) as the other machine. Results stay in each checkout's
# bench/results/; /root/extras09/progress.log says what ran.
set -u
. /root/.cargo/env
ulimit -n 1048576
export CARGO_NET_RETRY=10
L=/root/extras09; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
P=10.40.96.3
for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f"; done
ssh root@$P 'for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance > $f; done'
LBL=bare-metal-epyc-4345p

checkout() { # dir branch sha [host]
  local cmd="cd /root && rm -rf $1 && git clone -q https://github.com/giong-run/Giong.git $1 && cd $1 && git fetch -q origin $2 && git checkout -q --detach $3 && git log --oneline -1"
  if [ -n "${4:-}" ]; then ssh root@$4 "$cmd"; else bash -c "$cmd"; fi
}

# --- QUIC (#109): server on br-2, client here
if [[ " ${PARTS:-quic pipelines lanes} " == *" quic "* ]]; then
  log "quic: $(checkout giong-quic feat/quic-prototype 0a161d6) / br-2: $(checkout giong-quic feat/quic-prototype 0a161d6 $P)"
  cd /root/giong-quic/bench
  SERVER=root@$P SERVER_ADDR=$P:4433 REPO=/root/giong-quic LABEL=$LBL \
    SERVER_CPUS=0-15 CLIENT_CPUS=0-15 WORKERS="1 4 8 16" ./quic-2host.sh 3 10 >$L/quic.log 2>&1
  log "quic exit $?"
fi

# --- io_uring pipelines (#117)
if [[ " ${PARTS:-quic pipelines lanes} " == *" pipelines "* ]]; then
  log "pipelines: $(checkout giong-pipe feat/uring-pipelines 0967058)"
  cd /root/giong-pipe/bench
  export OUT=/root/giong-pipe/bench/results/pipelines-$LBL; mkdir -p $OUT
  {
    pipelines/ab.sh 8 ping 100000 -- on: off:GIONG_IO_URING_OFF=msg-ring
    pipelines/ab.sh 5 ping-busy 20000 32 -- on: off:GIONG_IO_URING_OFF=msg-ring
    for c in 1 8 64; do pipelines/ab.sh 6 bulk 1024 $c -- on: off:GIONG_IO_URING_OFF=recv-bundles; done
    for c in 1 8 256; do pipelines/ab.sh 5 proxy 128 $c -- copy:PROXY_MODE=copy read:PROXY_MODE=read splice:PROXY_MODE=splice; done
    GIONG_DRIVER=epoll pipelines/ab.sh 3 proxy 128 8 -- read-epoll:PROXY_MODE=read splice-epoll:PROXY_MODE=splice
  } >$L/pipelines.log 2>&1
  log "pipelines ab exit $?"
  F=/root/fixed-512m.bin; [ -f $F ] || dd if=/dev/urandom of=$F bs=1M count=512 status=none
  {
    for i in $(seq 10); do pipelines/target/release/fixed file $F 400000; done
    for i in $(seq 10); do pipelines/target/release/fixed file $F 400000 direct; done
    for i in $(seq 10); do pipelines/target/release/fixed send 60000 65536; done
    for i in $(seq 10); do pipelines/target/release/fixed send 60000 262144; done
  } >$OUT/fixed-buffers.csv 2>$L/fixed.err
  log "pipelines fixed exit $?"
  unset OUT
  ssh root@$P 'rm -f /root/wrk-cpu.log'
  PATHS="/json /static?size=16384" \
    VARIANTS="all-on: no-recv-bundles:GIONG_IO_URING_OFF=recv-bundles no-msg-ring:GIONG_IO_URING_OFF=msg-ring" \
    BENCH_HOST=10.40.96.4 LOAD_HOST=root@$P OUT=results/pipelines-http-$LBL ./uring-features.sh 5 8 >$L/pipelines-http.log 2>&1
  log "pipelines http exit $?"
  scp -q root@$P:/root/wrk-cpu.log results/pipelines-http-$LBL/wrk-cpu.log
fi

# --- blocking and compute lanes (#118)
if [[ " ${PARTS:-quic pipelines lanes} " == *" lanes "* ]]; then
  log "lanes: $(checkout giong-lanes feat/blocking-compute-lanes 41b356e)"
  cd /root/giong-lanes/bench
  OUT=results/lanes-$LBL ./lanes.sh 5 10 >$L/lanes.log 2>&1
  log "lanes exit $?"
  # The weakness the PR names: under a cgroup CPU quota the lane's nice +10
  # does not protect the workers. 4 CPUs of quota spread over all 16.
  systemd-run --quiet --scope --unit=lanes-quota -p CPUQuota=400% -- \
    env OUT=results/lanes-quota4-$LBL ./lanes.sh 5 10 >$L/lanes-quota.log 2>&1
  log "lanes quota exit $?"
fi
log "all done"
