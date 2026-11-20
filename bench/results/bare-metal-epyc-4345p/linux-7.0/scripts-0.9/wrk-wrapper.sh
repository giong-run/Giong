#!/bin/bash
# Installed as /usr/local/bin/wrk on the load machine for the bare-metal run only:
# runs the real wrk unchanged and logs how busy this machine's CPUs were meanwhile.
read -r _ a b c d e f g h _ </proc/stat
t0=$((a + b + c + d + e + f + g + h)); i0=$((d + e))
/usr/bin/wrk "$@"
rc=$?
read -r _ a b c d e f g h _ </proc/stat
t1=$((a + b + c + d + e + f + g + h)); i1=$((d + e))
busy=$(awk -v t=$((t1 - t0)) -v i=$((i1 - i0)) 'BEGIN { printf "%.3f", t ? (t - i) / t : -1 }')
echo "$(date -u +%FT%T.%3NZ),$busy,$*" >>/root/wrk-cpu.log
exit $rc
