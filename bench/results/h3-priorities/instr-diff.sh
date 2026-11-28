#!/bin/bash
# usage: instr-diff.sh BASE NEW [lines]: the instructions per request of each function, new
# minus base, from two directories of ../h3-qpack-dynamic/callgrind.sh output (n=100 and 600).
ext(){ callgrind_annotate --inclusive=no --threshold=100 $1 2>/dev/null | grep -E "^ *[0-9,]+ +\(" | sed -E 's/ \[[^]]*\]$//; s/\( *[0-9.]+%\) +//' | awk '{v=$1; gsub(",","",v); $1=""; sub(/^ /,""); print $0"\t"v}' | sort -t$'\t' -k1,1; }
for t in $1 $2; do ext $(ls $t/cg.0.600.*.1|head -1) > $t.600; ext $(ls $t/cg.0.100.*.1|head -1) > $t.100; join -t$'\t' $t.600 $t.100 | awk -F'\t' '{print $1"\t"($2-$3)/500}' > $t.per; done
join -t$'\t' -a1 -a2 -e0 -o 0,1.2,2.2 $1.per $2.per | awk -F'\t' '{d=$3-$2; if (d>5||d<-5) print d"\t"$2"\t"$3"\t"$1}' | sort -rn | head -${3:-20}
