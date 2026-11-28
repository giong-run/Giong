#!/bin/bash
# cg.sh name server-binary -> one CSV row: name,req_s_4,ir_4,req_s_12,ir_12,reqs,instr_per_req
# Serial round trips (1 connection, 1 stream, 100-byte answer); instructions per
# request = (Ir of a 12 s run - Ir of a 4 s run) / (requests 12 s - requests 4 s).
cd "$(dirname "$0")/../../quic"; B=target/release; name=$1; srv=$2; port=$3; O=${OUTDIR:-/tmp}
one(){ secs=$1; port=$2; valgrind --tool=callgrind --fair-sched=yes --callgrind-out-file=$O/cg-$name-$secs.out $srv --addr 127.0.0.1:$port >$O/cg-$name-$secs.log 2>&1 & vp=$!; sleep 8
 r=$(RUST_BACKTRACE=0 timeout 60 $B/client --addr 127.0.0.1:$port --threads 1 --warmup 1 --secs $secs --conns 1 --streams 1 --size 100 2>/dev/null | grep -o "req_s=[0-9.]*" | cut -d= -f2)
 sleep 1; kill -TERM $vp; for i in $(seq 1 30); do kill -0 $vp 2>/dev/null || break; sleep 1; done; kill -9 $vp 2>/dev/null; wait $vp 2>/dev/null
 ir=$(grep -o "Collected : [0-9]*" $O/cg-$name-$secs.log | awk '{print $3}'); echo "${r:-0} ${ir:-0}"; }
a=($(one 4 $port)); b=($(one 12 $((port+1))))
python3 -c "
r4,i4,r12,i12=${a[0]},${a[1]},${b[0]},${b[1]}
n=r12*12-r4*4
print('$name',r4,i4,r12,i12,round(n),round((i12-i4)/n) if i4 and i12 and n>0 else 'FAILED',sep=',')"
