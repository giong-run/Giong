#!/bin/bash
# sys.sh rounds servers workloads out.csv: syscalls, io_uring submissions and
# completions of the server per request (rr) or per MiB (bulk), counted by perf
# over the measured window.
cd "$(dirname "$0")/../../quic"; B=target/release
R=$1; SERVERS=$2; WL=$3; OUT=$4; PORT=${PORT:-7400}; S=5
cmd(){ case $1 in quinn) echo "$B/server-quinn";; quinn-copy) echo "$B/server-quinn --copy 1";; giong-quic) echo "$B/server-giong-quic-final";; gq-base) echo "$B/server-giong-quic-base";; esac; }
wla(){ case $1 in rr) echo "--conns 8 --streams 16 --size 100";; rr1) echo "--conns 1 --streams 1 --size 100";; bulk) echo "--conns 4 --streams 1 --size 16777216";; esac; }
for r in $(seq 1 $R); do for w in $WL; do for s in $SERVERS; do
  PORT=$((PORT+1))
  taskset -c 0-1 $(cmd $s) --addr 127.0.0.1:$PORT >/dev/null 2>&1 & pid=$!; sleep 1.5
  RUST_BACKTRACE=0 timeout 60 taskset -c 2-3 $B/client --addr 127.0.0.1:$PORT --server-pid $pid --threads 2 --warmup 2 --secs $S $(wla $w) >${OUTDIR:-/tmp}/cli.out 2>/dev/null & cp=$!
  sleep 2.1
  perf stat -x, -e raw_syscalls:sys_enter,io_uring:io_uring_submit_req,io_uring:io_uring_complete -p $pid -- sleep $((S-1)) 2>${OUTDIR:-/tmp}/perf.out >/dev/null
  wait $cp; kill $pid; wait $pid 2>/dev/null
  line=$(cat ${OUTDIR:-/tmp}/cli.out)
  sc=$(grep sys_enter ${OUTDIR:-/tmp}/perf.out | cut -d, -f1); sb=$(grep submit_req ${OUTDIR:-/tmp}/perf.out | cut -d, -f1); cq=$(grep io_uring_complete ${OUTDIR:-/tmp}/perf.out | cut -d, -f1)
  echo "$r,$w,$s,perf_secs=$((S-1)) syscalls=$sc uring_submits=$sb uring_completes=$cq $line" >> $OUT
done; done; done
