#!/bin/bash
# usage: run.sh table n port ; prints server Ir after n requests (dumped live)
T=$1; N=$2; P=$3
GIONG_QPACK_TABLE=$T valgrind --tool=callgrind --fair-sched=yes --callgrind-out-file=${OUT:-.}/cg.$T.$N.%p ${TARGET:-target}/release/examples/h3 127.0.0.1:$P >/dev/null 2>${OUT:-.}/vg.$T.$N.log &
VP=$!
sleep 6
${PYTHON:-python3} "$(dirname "$0")/h3req.py" localhost $P $N >${OUT:-.}/cl.$T.$N.log 2>&1
sleep 1
callgrind_control -d $VP >/dev/null 2>&1
sleep 2
kill -9 $VP; wait $VP 2>/dev/null
f=$(ls ${OUT:-.}/cg.$T.$N.*.1 | head -1)
ir=$(grep -h "^summary:\|^totals:" $f | head -1 | awk '{print $2}')
echo "table=$T n=$N Ir=$ir $(head -c 60 ${OUT:-.}/cl.$T.$N.log)"
