#!/usr/bin/env bash
# The server held to the cheapest class of machine: 1 CPU and 512 MiB, the
# size of the smallest VPS, against Tokio (axum), Go and nginx.
#
#   LOAD_HOST=root@10.0.0.3 BENCH_IP=10.0.0.2 bench/small-box.sh [rounds] [seconds]
#
# Run it as root on the server machine; LOAD_HOST is a second machine on a
# physical network that runs wrk and h2load (root ssh without a password).
#
# Method
#   * Each server runs in its own cgroup made with systemd-run: cpu.max of one
#     CPU (CPUQuota=100%), pinned to one logical CPU (CPU, default 2; nothing
#     else runs there or on its SMT sibling), memory.max of MEM (512M) and no
#     swap. Every server sees one CPU and starts one worker: giong's default
#     follows the affinity mask, Tokio's multi-threaded runtime takes
#     available_parallelism(), Go's GOMAXPROCS likewise, nginx is configured
#     with one worker process.
#   * rps: requests per second from LOAD_HOST over the network, ROUNDS rounds
#     of SECS seconds after a 2 s warm-up, servers interleaved:
#       json      plain HTTP/1.1 keep-alive, wrk -t4 -c128
#       json-tls  HTTPS keep-alive (handshakes once), wrk -t4 -c128
#       json-h2   HTTP/2 over TLS (ALPN), h2load -t4 -c32 -m16
#     plus the server's CPU use (cgroup cpu.stat) and how long the quota
#     throttled it.
#   * idle: the most idle HTTPS connections the server holds before it
#     refuses or is killed for memory (small-box/tlshold.py, from this
#     machine over loopback, from up to 250 source addresses), up to
#     IDLE_MAX, and the memory per connection at 20,000 connections (cgroup
#     memory.current, which counts the kernel's socket and TLS memory, and
#     the server's resident memory). Done once per server (IDLE_ROUNDS).
#     Idle timeouts are raised to an hour everywhere so that the first
#     connections are still there at the end; giong-http's limit of 10,000
#     connections per worker is raised to IDLE_MAX (GIONG_MAX_CONNS).
#   * flood: slow uploads (small-box/flood.py): 1 MB bodies sent one byte
#     short and held, up to FLOOD_MAX, to a handler that reads the whole
#     body, while a well-behaved client asks for /json every 100 ms. Reports
#     how many uploads were held when the cgroup's out-of-memory killer
#     struck, or that it never did.
#
# Servers (SERVERS): giong-http (also giong-http-ktls: kernel TLS after the
# rustls handshake), axum (axum and axum-tls on Tokio), go (net/http), nginx
# (also nginx-ktls: OpenSSL's kernel TLS). giong-http and axum use rustls
# with the ring provider; Go its own TLS; nginx OpenSSL. Certificate: ECDSA
# P-256.
#
# Requires: cargo, go, nginx, wrk and h2load on LOAD_HOST, python3, the
# kernel's tls module for the -ktls servers, cgroup v2 with systemd.
# Results: bench/results/small-box/ (or $OUT): rps.csv, idle-<server>.csv,
# flood-<server>.csv, raw/ (every client's output), env.txt, summary.txt.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-3}
SECS=${2:-10}
: "${LOAD_HOST:?set LOAD_HOST=user@host of the load generator}"
: "${BENCH_IP:?set BENCH_IP to the address of this machine on the network to LOAD_HOST}"
CPU=${CPU:-2}
MEM=${MEM:-512M}
CLIENT_CPUS=${CLIENT_CPUS:-4-7,12-15}
SERVERS=${SERVERS:-giong-http giong-http-ktls axum go nginx nginx-ktls}
TESTS=${TESTS:-rps idle flood}
IDLE_MAX=${IDLE_MAX:-200000}
IDLE_ROUNDS=${IDLE_ROUNDS:-1}
FLOOD_MAX=${FLOOD_MAX:-3000}
OUT=${OUT:-results/small-box}
mkdir -p "$OUT/raw"
ulimit -n 1048576

TMP=$(mktemp -d)
cleanup() { systemctl stop 'sbx-*.scope' 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 7 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:$BENCH_IP" 2>/dev/null
chmod 644 "$TMP"/*.pem
modprobe tls 2>/dev/null || true

echo "building..." >&2
(cd giong-http && cargo build --release -q)
(cd axum && cargo build --release -q)
(cd axum-tls && cargo build --release -q)
(cd go-http && go build -o bench-go-http .)

{
  echo "# $(date -u +%FT%TZ) $(uname -r), $(nproc) CPUs:$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2)"
  echo "# server cgroup: CPUQuota=100% AllowedCPUs=$CPU MemoryMax=$MEM MemorySwapMax=0"
  echo "# load: $LOAD_HOST $(ssh "$LOAD_HOST" 'uname -r; nproc' | paste -sd' ' -), wrk $(ssh "$LOAD_HOST" 'wrk -v 2>&1 | head -1'), $(ssh "$LOAD_HOST" 'h2load --version | head -1')"
  echo "# $(rustc -V); $(go version); $(nginx -v 2>&1); $(openssl version)"
  echo "# giong commit $(git rev-parse HEAD 2>/dev/null || echo unknown)"
} >"$OUT/env.txt"

# name -> "plain-port tls-port command..."
cmd() {
  local g="giong-http/target/release/bench-giong-http"
  local giong_env="BENCH_HOST=0.0.0.0 GIONG_MAX_CONNS=$IDLE_MAX GIONG_IDLE_TIMEOUT_SECS=3600"
  case $1 in
    giong-http) echo "env $giong_env $g" ;;
    giong-http-tls) echo "env $giong_env TLS_CERT=$TMP/cert.pem TLS_KEY=$TMP/key.pem GIONG_KTLS=off $g" ;;
    giong-http-ktls) echo "env $giong_env TLS_CERT=$TMP/cert.pem TLS_KEY=$TMP/key.pem GIONG_KTLS=required $g" ;;
    axum) echo "env BENCH_HOST=0.0.0.0 axum/target/release/bench-axum" ;;
    axum-tls) echo "env BENCH_HOST=0.0.0.0 TCP_NODELAY=1 TLS_CERT=$TMP/cert.pem TLS_KEY=$TMP/key.pem axum-tls/target/release/bench-axum-tls" ;;
    go) echo "env BENCH_HOST=0.0.0.0 go-http/bench-go-http" ;;
    go-tls) echo "env BENCH_HOST=0.0.0.0 TLS_CERT=$TMP/cert.pem TLS_KEY=$TMP/key.pem go-http/bench-go-http" ;;
    nginx|nginx-tls|nginx-ktls)
      local d="$TMP/$1" ktls=""
      [ "$1" = nginx-ktls ] && ktls="ssl_conf_command Options KTLS;"
      mkdir -p "$d"
      sed -e "s|@DIR@|$d|g" -e "s|@CERT@|$TMP/cert.pem|" -e "s|@KEY@|$TMP/key.pem|" -e "s|@KTLS@|$ktls|" \
        small-box/nginx.conf >"$d/nginx.conf"
      echo "nginx -p $d -c $d/nginx.conf -g 'daemon off; user root;'" ;;
  esac
}
port() {
  case $1 in
    giong-http) echo 3009 ;; giong-http-tls|giong-http-ktls) echo 3010 ;;
    axum) echo 3003 ;; axum-tls) echo 3011 ;;
    go) echo 3011 ;; go-tls) echo 3012 ;;
    nginx) echo 3013 ;; nginx-tls|nginx-ktls) echo 3014 ;;
  esac
}
# The variants of a server: plain, TLS.
plain_of() { case $1 in giong-http-ktls|nginx-ktls) echo "" ;; *) echo "$1" ;; esac; }
tls_of() {
  case $1 in
    giong-http) echo giong-http-tls ;; axum) echo axum-tls ;; go) echo go-tls ;; nginx) echo nginx-tls ;;
    *) echo "$1" ;;
  esac
}

CG=""
start() { # variant
  local unit="sbx-$1"
  systemctl stop "$unit.scope" 2>/dev/null || true
  # shellcheck disable=SC2046
  eval systemd-run --quiet --scope --unit="$unit" -p CPUQuota=100% -p AllowedCPUs="$CPU" \
    -p MemoryMax="$MEM" -p MemorySwapMax=0 -- taskset -c "$CPU" $(cmd "$1") \
    >>"$OUT/raw/server-$1.log" 2>&1 &
  CG=/sys/fs/cgroup/system.slice/$unit.scope
  local p
  p=$(port "$1")
  for _ in $(seq 100); do
    if python3 -c "import socket; socket.create_connection(('127.0.0.1', $p), 0.2)" 2>/dev/null; then
      sleep 0.5
      return 0
    fi
    sleep 0.1
  done
  echo "$1 did not start" >&2
  return 1
}
stop() { systemctl stop "sbx-$1.scope" 2>/dev/null || true; sleep 1; }
cpu_usec() { awk '$1 == "usage_usec" { print $2 }' "$CG/cpu.stat"; }
throttled_usec() { awk '$1 == "throttled_usec" { print $2 }' "$CG/cpu.stat"; }

# One measurement from LOAD_HOST; appends to rps.csv.
measure() { # round variant test url
  local round=$1 v=$2 test=$3 url=$4 c0 t0 c1 t1 raw="$OUT/raw/rps-$2-$3-r$1.txt" client
  if [ "$test" = json-h2 ]; then
    client="h2load -t4 -c32 -m16"
    ssh "$LOAD_HOST" "h2load -t4 -c32 -m16 -D 2 $url" >/dev/null 2>&1 || true
    c0=$(cpu_usec); t0=$(throttled_usec)
    ssh "$LOAD_HOST" "h2load -t4 -c32 -m16 -D $SECS $url" >"$raw" 2>&1 || true
  else
    client="wrk -t4 -c128"
    ssh "$LOAD_HOST" "wrk -t4 -c128 -d2s $url" >/dev/null 2>&1 || true
    c0=$(cpu_usec); t0=$(throttled_usec)
    ssh "$LOAD_HOST" "wrk -t4 -c128 -d${SECS}s --latency $url" >"$raw" 2>&1 || true
  fi
  c1=$(cpu_usec); t1=$(throttled_usec)
  python3 - "$raw" "$round" "$v" "$test" "$client" $((c1 - c0)) $((t1 - t0)) "$SECS" >>"$OUT/rps.csv" <<'PY'
import re, sys
raw, rnd, v, test, client, cpu, thr, secs = sys.argv[1:]
t = open(raw).read()
rps = p99 = ""
errors = 0
if "h2load" in client:
    m = re.search(r"finished in [\d.]+\w*, ([\d.]+) req/s", t)
    rps = m.group(1) if m else ""
    m = re.search(r"requests: \d+ total, \d+ started, \d+ done, \d+ succeeded, (\d+) failed, (\d+) errored", t)
    errors = int(m.group(1)) + int(m.group(2)) if m else -1
else:
    m = re.search(r"Requests/sec:\s+([\d.]+)", t)
    rps = m.group(1) if m else ""
    m = re.search(r"^\s+99%\s+([\d.]+)(us|ms|s)", t, re.M)
    if m:
        p99 = float(m.group(1)) * {"us": 1e-3, "ms": 1, "s": 1e3}[m.group(2)]
    for m in re.finditer(r"Socket errors: connect (\d+), read (\d+), write (\d+), timeout (\d+)", t):
        errors += sum(int(x) for x in m.groups())
    m = re.search(r"Non-2xx or 3xx responses: (\d+)", t)
    errors += int(m.group(1)) if m else 0
cpu_pct = int(cpu) / (float(secs) * 1e4)
print(f"{rnd},{v},{test},{rps},{p99},{cpu_pct:.1f},{int(thr) / 1e3:.0f},{errors}")
PY
}

if [[ " $TESTS " == *" rps "* ]]; then
  [ -f "$OUT/rps.csv" ] || echo "round,server,test,req_s,p99_ms,cpu_pct,throttled_ms,errors" >"$OUT/rps.csv"
  for round in $(seq 1 "$ROUNDS"); do
    for s in $SERVERS; do
      p=$(plain_of "$s")
      if [ -n "$p" ]; then
        echo "round $round: $p json" >&2
        start "$p"
        measure "$round" "$p" json "http://$BENCH_IP:$(port "$p")/json"
        stop "$p"
      fi
      t=$(tls_of "$s")
      echo "round $round: $t json-tls json-h2" >&2
      start "$t"
      measure "$round" "$t" json-tls "https://$BENCH_IP:$(port "$t")/json"
      measure "$round" "$t" json-h2 "https://$BENCH_IP:$(port "$t")/json"
      stop "$t"
    done
  done
fi

if [[ " $TESTS " == *" idle "* ]]; then
  for round in $(seq 1 "$IDLE_ROUNDS"); do
    for s in $SERVERS; do
      t=$(tls_of "$s")
      echo "idle $round: $t" >&2
      start "$t"
      taskset -c "$CLIENT_CPUS" python3 small-box/tlshold.py "$(port "$t")" "$CG" "$IDLE_MAX" \
        >"$OUT/idle-$t-r$round.csv" 2>"$OUT/raw/idle-$t-r$round.err" || true
      tail -1 "$OUT/idle-$t-r$round.csv" >&2
      stop "$t"
      sleep 5 # let TIME_WAIT and the kernel's socket memory settle
    done
  done
fi

if [[ " $TESTS " == *" flood "* ]]; then
  for s in $SERVERS; do
    p=$(plain_of "$s")
    [ -n "$p" ] || continue
    echo "flood: $p" >&2
    start "$p"
    taskset -c "$CLIENT_CPUS" python3 small-box/flood.py "$(port "$p")" "$CG" "$FLOOD_MAX" \
      >"$OUT/flood-$p.csv" 2>"$OUT/raw/flood-$p.err" || true
    tail -1 "$OUT/flood-$p.csv" >&2
    stop "$p"
    sleep 5
  done
fi

python3 - "$OUT" >"$OUT/summary.txt" <<'PY'
import csv, glob, os, statistics, sys
out = sys.argv[1]
print(open(os.path.join(out, "env.txt")).read().strip())
rps = os.path.join(out, "rps.csv")
if os.path.exists(rps):
    rows = list(csv.DictReader(open(rps)))
    print("\nrequests per second, median of rounds (min-max); p99 ms (wrk only); server CPU %; errors")
    print(f"{'server':<17}{'test':<10}{'req/s':>28}{'p99 ms':>9}{'cpu%':>7}{'thr ms':>8}{'errors':>8}")
    keys = list(dict.fromkeys((r["server"], r["test"]) for r in rows))
    for s, t in keys:
        rs = [r for r in rows if (r["server"], r["test"]) == (s, t) and r["req_s"]]
        if not rs:
            print(f"{s:<17}{t:<10}{'failed':>28}")
            continue
        v = [float(r["req_s"]) for r in rs]
        p99 = [float(r["p99_ms"]) for r in rs if r["p99_ms"]]
        med = lambda xs: statistics.median(xs) if xs else float("nan")
        print(f"{s:<17}{t:<10}{f'{med(v):,.0f} ({min(v):,.0f}-{max(v):,.0f})':>28}{med(p99):>9.2f}"
              f"{med([float(r['cpu_pct']) for r in rs]):>7.0f}{med([float(r['throttled_ms']) for r in rs]):>8.0f}"
              f"{sum(int(r['errors']) for r in rs):>8}")
idle = sorted(glob.glob(os.path.join(out, "idle-*.csv")))
if idle:
    print("\nidle HTTPS connections: most held, why it stopped, KiB per connection at 20,000"
          "\n(cgroup memory.current, which includes the kernel's socket and TLS memory; resident memory; kernel part)")
    for f in idle:
        lines = [l.strip() for l in open(f) if l.strip()]
        rows = [dict(zip(lines[0].split(","), l.split(","))) for l in lines[1:] if not l.startswith("end")]
        end = [l for l in lines if l.startswith("end")]
        held, why = (end[0].split(",")[1:3] if end else ("?", "incomplete"))
        base = rows[0] if rows else None
        at = next((r for r in rows if int(r["held"]) >= 20000), None)
        name = os.path.basename(f)[5:-4]
        if base and at:
            n = int(at["held"])
            per = lambda k: (int(at[k]) - int(base[k])) / n
            print(f"{name:<24} held {int(held):>7,} ({why:<8})  cgroup {per('mem_current_kib'):6.2f}  "
                  f"rss {per('rss_kib'):6.2f}  kernel {per('kernel_kib'):6.2f}  sock {per('sock_kib'):6.2f}")
        else:
            print(f"{name:<24} held {held} ({why})")
flood = sorted(glob.glob(os.path.join(out, "flood-*.csv")))
if flood:
    print("\nslow-upload flood (1 MB bodies one byte short): uploads sent, still held, refused, outcome,"
          "\npeak cgroup memory, well-behaved /json requests answered / not")
    for f in flood:
        lines = [l.strip() for l in open(f) if l.strip()]
        end = [l for l in lines if l.startswith("end")]
        rows = [l.split(",") for l in lines[1:] if not l.startswith("end")]
        peak = max((int(r[4]) for r in rows), default=0) // 1024
        name = os.path.basename(f)[6:-4]
        if end:
            _, sent, held, refused, why, ok, bad = end[0].split(",")
            print(f"{name:<12} sent {sent:>5}  held {held:>5}  refused {refused:>5}  {why:<9} peak {peak:>4} MiB  ok {ok} bad {bad}")
        else:
            print(f"{name:<12} incomplete")
PY
cat "$OUT/summary.txt"
