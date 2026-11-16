#!/bin/bash
# HTTP/2 idle memory per connection, 0.5.0 candidate (d5a0b0c) vs 0.8.0
# (b6d9b4b), on the same machine: bench/h2.sh with 3 rounds each, interleaved.
set -u
. /root/.cargo/env
ulimit -n 1048576
L=/root/h2mem; mkdir -p $L
for c in d5a0b0c b6d9b4b; do
  [ -d /root/h2-$c ] || (cd /root && git clone -q https://github.com/giong-run/Giong.git h2-$c && cd h2-$c && git checkout -q --detach $c)
done
for r in 1 2 3; do
  for c in d5a0b0c b6d9b4b; do
    (cd /root/h2-$c/bench && OUT=$L/$c-r$r ./h2.sh 1 2 >$L/$c-r$r.log 2>&1)
    echo "$(date -u +%FT%TZ) $c round $r: $(grep -A3 '^server  *h2-idle' $L/$c-r$r/summary.txt | grep giong-http)" >>$L/progress.log
  done
done
echo done >>$L/progress.log
