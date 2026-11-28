#!/bin/bash
# cgsat.sh <server binary> <port> <out-dir> [conns] [path]: server instructions per request under saturation
# (one worker, loadgen closed loop): (dump of a 12 s run - dump of a 4 s run) / (their request difference)
BIN=$1; PORT=$2; OUT=$3; CONNS=${4:-32}; P=${5:-/json}
LG=$(cd "$(dirname "$0")/../../lab" && pwd)/loadgen/target/release/lab-loadgen
mkdir -p $OUT
GIONG_WORKERS=1 GIONG_KTLS=off valgrind --tool=callgrind --compress-strings=no --compress-pos=no --callgrind-out-file=$OUT/cg.%p "$BIN" >/dev/null 2>$OUT/vg.log &
vg=$!
for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:$PORT$P && break; sleep 0.5; done
run(){ taskset -c 1-3 $LG --addr 127.0.0.1:$PORT --conns $CONNS --threads 2 --duration $1 --warmup 0 --mix "$P" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["requests"])'; }
run 2 >/dev/null
callgrind_control -z $vg >/dev/null; ra=$(run 4); callgrind_control -d $vg >/dev/null
callgrind_control -z $vg >/dev/null; rb=$(run 12); callgrind_control -d $vg >/dev/null
sleep 1; kill -9 $vg; wait $vg 2>/dev/null
a=$(grep -m1 '^summary:\|^totals:' $(ls $OUT/cg.*.1) | awk '{print $2}')
b=$(grep -m1 '^summary:\|^totals:' $(ls $OUT/cg.*.2) | awk '{print $2}')
python3 -c "print(f'reqs {$ra} {$rb} instr/req {($b-$a)/($rb-$ra):.1f}')"
