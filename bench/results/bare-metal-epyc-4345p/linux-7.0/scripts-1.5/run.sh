#!/bin/bash
# The two-machine part of the run, on br-1 (the server) in tmux, after build.sh on both:
#
#   bash run.sh [parts...]      default: reg ab rate gzip h3 quic, in that order
#
#   reg    bench/workers.sh (giong, giong-http, tokio-raw, monoio-tuned, 1 to 16 workers)
#          and bench/uring-features.sh, wrk on br-2: the regression pass against the
#          0.8.0 data in ../release-0.8.0
#   ab     ab-net.sh: v1.5.1 against main (and PR #487 while it is open), HTTP/1.1,
#          HTTPS, TLS handshakes (the #481 recheck), h2c, h2
#   rate   rate.sh: closed loop and open-loop latency, 1/2/4/8 workers (issue #6)
#   gzip   gzip.sh: PR #429 with and without giong-web/fast-gzip
#   h3     h3.sh: HTTP/3 requests, giong-http serve_h3 against h3 on quinn
#   quic   bench/quic-2host.sh: QUIC streams, giong's engine against quinn (server on br-2)
#   c1m    c1m.sh: a million held connections (not in the default list; needs build-late.sh)
#
# Results go to /root/r15/out/<part>; progress to /root/r15/progress.log.
D=$(cd "$(dirname "$0")" && pwd)
. "$D/env.sh"
O=$R15/out
mkdir -p "$O"
performance
on_l "$(declare -f performance); performance"
pr487=""; [ -x "$B/bins/pr487-giong-http" ] && pr487="pr487=$B/bins/pr487-giong-http"
for part in ${*:-reg ab rate gzip h3 quic}; do
  log "part $part: start"
  case $part in
    reg)
      (cd "$B/main/bench" &&
        BENCH_HOST=$S LOAD_HOST=root@$L LOAD_THREADS=16 SERVER_CPUS=0-15 \
          SERVERS="giong giong-http tokio-raw monoio-tuned" WORKERS="1 2 4 8 16" \
          OUT=$O/reg/workers ./workers.sh 3 6 >"$O/reg-workers.log" 2>&1
        log "reg: workers exit $?"
        BENCH_HOST=$S LOAD_HOST=root@$L OUT=$O/reg/uring-features ./uring-features.sh 3 10 >"$O/reg-uring.log" 2>&1
        log "reg: uring-features exit $?")
      ;;
    ab)
      BINS="v1.5.1=$B/bins/v151-giong-http main=$B/bins/main-giong-http $pr487" OUT=$O/ab bash "$D/ab-net.sh" 5 10 >"$O/ab.log" 2>&1
      ;;
    rate) OUT=$O/rate bash "$D/rate.sh" 3 10 >"$O/rate.log" 2>&1 ;;
    gzip) OUT=$O/gzip bash "$D/gzip.sh" 6 10 >"$O/gzip.log" 2>&1 ;;
    h3) OUT=$O/h3 bash "$D/h3.sh" 5 10 >"$O/h3.log" 2>&1 ;;
    c1m) OUT=$O/c1m bash "$D/c1m.sh" >"$O/c1m.log" 2>&1 ;;
    quic)
      label=bare-metal-1.5-$(date -u +%Y%m%d)
      on_l "nstat -asz 'Udp*'" >"$O/quic-udp-before.txt" 2>&1
      (cd "$B/main/bench" &&
        SERVER=root@$L SERVER_ADDR=$L:4433 REPO=$B/main LABEL=$label SERVERS="quinn giong giong-epoll" WORKERS="1 4" \
          ./quic-2host.sh 3 10 >"$O/quic.log" 2>&1
        log "quic exit $?")
      on_l "nstat -asz 'Udp*'" >"$O/quic-udp-after.txt" 2>&1
      rm -rf "$O/quic" && cp -r "$B/main/bench/results/quic/$label" "$O/quic"
      ;;
    *) log "unknown part $part" ;;
  esac
  log "part $part: done"
done
