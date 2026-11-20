#!/bin/bash
# Single-box runs on br-1 after its 72 h soak, while br-2's epoll soak runs:
# nothing here touches br-2. Logs: /root/q1/progress.log.
set -u
. /root/.cargo/env
ulimit -n 1048576
export CARGO_NET_RETRY=10
modprobe tls
ufw allow in on xs >/dev/null 2>&1
for f in /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference; do echo performance >"$f"; done
L=/root/q1; mkdir -p $L
log() { echo "$(date -u +%FT%TZ) $*" >>$L/progress.log; }
REPO=https://github.com/giong-run/Giong.git
MAIN=${MAIN:-faa18a3}
co() { # dir commit [extra fetch refs...]
  local d=$1 c=$2; shift 2
  [ -d $d/.git ] || git clone -q $REPO $d
  (cd $d && for r in "$@"; do git fetch -q origin "$r"; done; git checkout -q --detach -f $c) || { log "checkout $c failed"; return 1; }
}

# ---- 1. json-mux, five builds of bench/giong-http, TLS and kTLS, perf stat
if [[ " ${PARTS:-mux tls lanes udp} " == *" mux "* ]]; then
  B=/root/mux; mkdir -p $B
  declare -A SRC=([b6d9b4b]=b6d9b4b [3bec259]=3bec2590292f6a5d1a325d8f810811c7c37c1f7c [6372cd5]=6372cd5 [6372cd5-oldbench]=6372cd5 [main-$MAIN]=$MAIN)
  for n in b6d9b4b 3bec259 6372cd5 6372cd5-oldbench main-$MAIN; do
    co $B/$n ${SRC[$n]} ${SRC[$n]} || continue
    [ $n = 6372cd5-oldbench ] && (cd $B/$n && git checkout -q 3bec2590292f6a5d1a325d8f810811c7c37c1f7c -- bench/giong-http/src/main.rs)
    (cd $B/$n/bench/giong-http && cargo build --release -q) >>$L/mux-build.log 2>&1 || log "build $n failed"
  done
  T=$(mktemp -d)
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout $T/key.pem -out $T/cert.pem -days 2 -subj /CN=localhost -addext subjectAltName=DNS:localhost 2>/dev/null
  O=$L/mux; mkdir -p $O
  echo "round,build,mode,req_s,cycles,instructions,branch_misses" >$O/raw.csv
  for r in 1 2 3 4 5; do
    for n in b6d9b4b 3bec259 6372cd5 6372cd5-oldbench main-$MAIN; do
      for mode in off required; do
        TLS_CERT=$T/cert.pem TLS_KEY=$T/key.pem GIONG_KTLS=$mode GIONG_WORKERS=1 taskset -c 3 $B/$n/bench/giong-http/target/release/bench-giong-http >$O/server-$n-$mode.log 2>&1 &
        pid=$!
        for _ in $(seq 50); do curl -sk --http2 -o /dev/null https://127.0.0.1:3010/json && break; sleep 0.1; done
        taskset -c 0-2 h2load -t3 -c3 -m100 -D 2 https://127.0.0.1:3010/json >/dev/null 2>&1
        perf stat -C 3 -x, -e cycles,instructions,branch-misses -o $O/perf-$n-$mode-r$r.txt -- \
          taskset -c 0-2 h2load -t3 -c3 -m100 -D 10 https://127.0.0.1:3010/json >$O/h2load-$n-$mode-r$r.txt 2>&1
        kill $pid; wait $pid 2>/dev/null
        rps=$(awk '/^finished in/ {gsub(",", "", $4); print $4}' $O/h2load-$n-$mode-r$r.txt)
        p() { awk -F, -v e=$1 '$3 ~ e {print $1}' $O/perf-$n-$mode-r$r.txt; }
        echo "$r,$n,$mode,$rps,$(p cycles),$(p instructions),$(p branch-misses)" >>$O/raw.csv
        sleep 1
      done
    done
    log "mux round $r done"
  done
  rm -rf $T
fi

# ---- 2. TLS: kTLS against axum + tokio-rustls, on main (has #146)
if [[ " ${PARTS:-mux tls lanes udp} " == *" tls "* ]]; then
  co /root/tlsrun $MAIN $MAIN && (cd /root/tlsrun/bench && SERVERS="axum-tls giong-http-tls giong-http-ktls" \
    ENDPOINTS="json users handshake bytes64k bytes1m file64k file1m" OUT=$L/tls ./tls.sh 5 5 >$L/tls.log 2>&1)
  log "tls exit $?"
fi

# ---- 3. lanes on feat/sched-contract (#135)
if [[ " ${PARTS:-mux tls lanes udp} " == *" lanes "* ]]; then
  co /root/sched origin/feat/sched-contract feat/sched-contract && (cd /root/sched && git log --oneline -1 >$L/lanes-commit.txt; cd bench && OUT=$L/lanes ./lanes.sh 5 10 >$L/lanes.log 2>&1)
  log "lanes exit $?"
fi

# ---- 4. UDP echo over veth: #139, #142, and the multi-core experiment
if [[ " ${PARTS:-mux tls lanes udp} " == *" udp "* ]]; then
  for b in perf/udp-faster perf/udp-epoll; do
    d=/root/udp-${b#perf/}
    co $d origin/$b $b || continue
    (cd $d && git log --oneline -1 >$L/udp-${b#perf/}-commit.txt)
    for t in 16 1; do
      (cd $d/bench && TASKS=$t OUT=$L/udp-${b#perf/}-tasks$t ./udp-echo.sh 5 8 >$L/udp-${b#perf/}-tasks$t.log 2>&1)
      log "udp ${b#perf/} tasks $t exit $?"
    done
  done
  d=/root/udp-next
  co $d 168d0f77ec2660c5873e81820d28975017c808e4 perf/udp-faster 168d0f77ec2660c5873e81820d28975017c808e4 \
    && (cd $d && git fetch -q /root/udp-next-experiments.bundle exp/udp-next:exp/udp-next && git checkout -q exp/udp-next && git log --oneline -1 >$L/udp-next-commit.txt) \
    && for sc in 1-2:4-7 1-4:8-15; do (cd $d/bench && SERVER_CPU=${sc%%:*} CLIENT_CPUS=${sc#*:} OUT=$L/udp-next-cpus${sc%%:*} bash ./udp-echo-multicore.sh 5 8 >$L/udp-next-cpus${sc%%:*}.log 2>&1); log "udp-next server cpus ${sc%%:*} exit $?"; done
fi
log "all done"
