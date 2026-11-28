#!/bin/bash
# server on SCPU (default 0-1), client on CCPU (2-3)
cd "$(dirname "$0")/../../quic"; B=${BIN:-target/release}
R=$1; S=$2; SERVERS=$3; WL=$4; OUT=$5; SCPU=${SCPU:-0-1}; CCPU=${CCPU:-2-3}; CT=${CT:-2}
cmd(){ case $1 in quinn) echo "$B/server-quinn";; quinn-copy) echo "$B/server-quinn --copy 1";; proto-tokio) echo "$B/server-proto-tokio";; giong) echo "$B/server-proto-giong --offload 1";; giong-nopin) echo "$B/server-proto-giong --offload 1 --pin 0";; giong-quic) echo "$B/server-giong-quic";; gq-base) echo "$B/server-giong-quic-base";; giong-quic-nopin) echo "$B/server-giong-quic --pin 0";; esac; }
wla(){ case $1 in rr) echo "--conns 8 --streams 16 --size 100";; rr4) echo "--conns 8 --streams 4 --size 100";; bulk) echo "--conns 4 --streams 1 --size 16777216";; bulk1) echo "--conns 1 --streams 1 --size 16777216";; esac; }
PORT=${PORT:-4500}
for r in $(seq 1 $R); do for w in $WL; do for s in $SERVERS; do
  PORT=$((PORT+1))
  taskset -c $SCPU $(cmd $s) --addr 127.0.0.1:$PORT >${OUTDIR:-/tmp}/last-srv.log 2>&1 & pid=$!; sleep 1.5
  line=$(RUST_BACKTRACE=0 timeout 60 taskset -c $CCPU $B/client --addr 127.0.0.1:$PORT --server-pid $pid --threads $CT --warmup 2 --secs $S $(wla $w) 2>/dev/null)
  kill $pid; wait $pid 2>/dev/null
  [ -z "$line" ] && { echo "FAILED: $(tail -3 ${OUTDIR:-/tmp}/last-srv.log)"; cp ${OUTDIR:-/tmp}/last-srv.log ${OUTDIR:-/tmp}/fail-$s-$r-$w.log; }
  echo "$r,$w,$s,$line" >> $OUT
  echo "$r $w $s $(echo $line | grep -o 'req_s=[0-9]*\|gbit_s=[0-9.]*\|p50_us=[0-9]*\|srv_cpu_pct=[0-9.]*\|srv_us_per_req=[0-9.]*\|srv_ns_per_kib=[0-9.]*' | tr '\n' ' ')"
done; done; done
