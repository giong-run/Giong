#!/bin/bash
# usage: client-callgrind.sh table n port ; prints the client's total Ir for n GETs
T=$1; N=$2; P=$3
GIONG_CA=${CA:?} GIONG_QPACK_TABLE=$T valgrind --tool=callgrind --callgrind-out-file=${OUT:-.}/cgc.$T.$N ${TARGET:-target}/release/examples/h3_get https://localhost:$P/ $N >${OUT:-.}/cl.$T.$N.log 2>${OUT:-.}/vg.$T.$N.log
ir=$(grep -h "^summary:\|^totals:" ${OUT:-.}/cgc.$T.$N | head -1 | awk '{print $2}')
echo "table=$T n=$N Ir=$ir $(tail -1 ${OUT:-.}/cl.$T.$N.log)"
