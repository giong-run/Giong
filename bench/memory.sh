#!/usr/bin/env bash
# Benchmark: how much memory an HTTP server spends per connection.
#
#   bench/memory.sh [rounds]
#
#   CONNS=10000 WORKERS=4 SERVERS="giong-http axum go" bench/memory.sh 3
#
# Scenarios, each with CONNS connections from bench/hold:
#   open     connected, nothing sent yet
#   idle     one keep-alive request answered, then quiet: the usual state of
#            a connection from a browser, a mobile app or a connection pool
#   pending  one request on each connection, still being handled: the handler
#            waits (GET /wait?ms=600000) as it would for a database or another
#            service
#   load     wrk keeps LOAD_CONNS connections busy with GET /json for LOAD_SECS
#            seconds; the peak resident memory counts
#
# The number reported for open, idle and pending is
#   (resident memory with the connections held - resident memory before) / CONNS
# in KiB, the median of all rounds. For load it is the peak (VmHWM) minus the
# memory before, in MiB, plus the requests per second wrk reached (the
# servers and wrk share the CPUs here, so that rate is only indicative; see
# bench/run.sh for throughput).
#
# Every server runs WORKERS threads: giong's workers (GIONG_WORKERS), Tokio's
# worker threads (TOKIO_WORKER_THREADS), Go's GOMAXPROCS. `giong-http-epoll`
# is giong-http on the epoll driver (GIONG_DRIVER=epoll).
#
# Servers:
#   giong-http   giong-http (bench/giong-http)
#   axum         axum 0.8 on Tokio and hyper (bench/axum)
#   tokio-raw    Tokio with a hand-written HTTP loop (bench/tokio-raw)
#   go           Go's net/http (bench/go-http, needs `go`)
#
# Requires: wrk, cargo, python3, go (for `go`); CONNS + 100 open files for
# both this script's client and the server (`ulimit -n`).
# Results: bench/results/memory/ (or $OUT).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-3}
CONNS=${CONNS:-10000}
WORKERS=${WORKERS:-4}
LOAD_CONNS=${LOAD_CONNS:-5000}
LOAD_SECS=${LOAD_SECS:-10}
SCENARIOS=${SCENARIOS:-open idle pending load}
OUT=${OUT:-results/memory}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

limit=$(ulimit -n)
if (( CONNS + 100 > limit || LOAD_CONNS + 100 > limit )); then
  echo "ulimit -n is $limit: lower CONNS / LOAD_CONNS or raise the limit" >&2
  exit 1
fi

read -r -a NAMES <<<"${SERVERS:-giong-http giong-http-epoll axum tokio-raw go}"

echo "building..." >&2
(cd hold && cargo build --release -q)
for name in "${NAMES[@]}"; do
  case $name in
    giong-http*) (cd giong-http && cargo build --release -q) ;;
    axum) (cd axum && cargo build --release -q) ;;
    tokio-raw) (cd tokio-raw && cargo build --release -q) ;;
    go) (cd go-http && go build -o bench-go-http .) ;;
  esac
done

declare -A CMD=(
  [giong-http]="giong-http/target/release/bench-giong-http"
  [giong-http-epoll]="env GIONG_DRIVER=epoll giong-http/target/release/bench-giong-http"
  [axum]="axum/target/release/bench-axum"
  [tokio-raw]="tokio-raw/target/release/bench-tokio-raw"
  [go]="go-http/bench-go-http"
)
declare -A PORT=([giong-http]=3009 [giong-http-epoll]=3009 [axum]=3003 [tokio-raw]=3005 [go]=3011)

rss_kb() { awk '/^VmRSS/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }
hwm_kb() { awk '/^VmHWM/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }

start_server() { # name -> sets SERVER_PID
  GIONG_WORKERS=$WORKERS TOKIO_WORKER_THREADS=$WORKERS GOMAXPROCS=$WORKERS \
    ${CMD[$1]} >/dev/null 2>&1 &
  SERVER_PID=$!
  local url="http://127.0.0.1:${PORT[$1]}/json"
  for _ in $(seq 100); do
    if curl -s -o /dev/null "$url"; then break; fi
    sleep 0.1
  done
  # Warm up, so that what the first requests allocate is not counted.
  for _ in $(seq 200); do curl -s -o /dev/null "$url" || true; done
  sleep 1
}

alive() { kill -0 "$SERVER_PID" 2>/dev/null; }

stop_server() {
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
}

hold() { # name mode path -> prints KiB per connection
  local name=$1 mode=$2 path=$3 base held log
  base=$(rss_kb "$SERVER_PID")
  log=$(mktemp)
  hold/target/release/bench-hold "127.0.0.1:${PORT[$name]}" "$CONNS" "$mode" "$path" >"$log" 2>&1 &
  local client=$!
  for _ in $(seq 1200); do
    grep -q ready "$log" && break
    if ! kill -0 "$client" 2>/dev/null; then
      echo "bench-hold failed against $name: $(cat "$log")" >&2
      rm -f "$log"
      echo "nan"
      return
    fi
    sleep 0.1
  done
  sleep 2
  held=$(rss_kb "$SERVER_PID")
  kill "$client" 2>/dev/null || true
  wait "$client" 2>/dev/null || true
  rm -f "$log"
  python3 -c "print(f'{($held - $base) / $CONNS:.2f}')"
}

load() { # name -> prints "MiB rps"
  local name=$1 base out rps peak
  base=$(rss_kb "$SERVER_PID")
  out=$(wrk -t"$WORKERS" -c"$LOAD_CONNS" -d"${LOAD_SECS}s" "http://127.0.0.1:${PORT[$name]}/json" 2>&1)
  rps=$(echo "$out" | awk '/Requests\/sec/ {print int($2)}')
  peak=$(hwm_kb "$SERVER_PID")
  python3 -c "print(f'{($peak - $base) / 1024:.1f} ${rps:-0}')"
}

echo "server,scenario,round,value,rps" >"$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for name in "${NAMES[@]}"; do
    for scenario in $SCENARIOS; do
      # A fresh server for each scenario: what one leaves behind (freed
      # memory the allocator keeps) must not count for the next.
      start_server "$name"
      if ! alive; then
        echo "$name did not start" >&2
        echo "$name,$scenario,$round,nan," >>"$CSV"
        continue
      fi
      case $scenario in
        open) value=$(hold "$name" open /json); rps= ;;
        idle) value=$(hold "$name" idle /json); rps= ;;
        pending) value=$(hold "$name" pending "/wait?ms=600000"); rps= ;;
        load) read -r value rps < <(load "$name") ;;
      esac
      if ! alive; then
        echo "$name exited during the $scenario scenario" >&2
        value=nan
      fi
      stop_server
      echo "$name,$scenario,$round,$value,$rps" >>"$CSV"
      printf '%-18s %-8s round %d: %s\n' "$name" "$scenario" "$round" "$value${rps:+ MiB, $rps req/s}" >&2
    done
  done
done

python3 - "$CSV" "$CONNS" "$WORKERS" "$LOAD_CONNS" >"$OUT/summary.txt" <<'EOF'
import csv, statistics, sys
path, conns, workers, load_conns = sys.argv[1:]
rows = list(csv.DictReader(open(path)))
servers = list(dict.fromkeys(r["server"] for r in rows))
def med(server, scenario, key="value"):
    vals = [float(r[key]) for r in rows if r["server"] == server and r["scenario"] == scenario and r[key] not in ("", "nan")]
    return statistics.median(vals) if vals else None
def fmt(v, f="{:.1f}"):
    return "-" if v is None else f.format(v)
print(f"{conns} connections, {workers} worker threads per server; load: {load_conns} busy connections")
print()
print(f"{'server':18} {'open':>8} {'idle':>8} {'pending':>8}   KiB per connection")
for s in servers:
    print(f"{s:18} {fmt(med(s, 'open')):>8} {fmt(med(s, 'idle')):>8} {fmt(med(s, 'pending')):>8}")
print()
print(f"{'server':18} {'peak MiB':>9} {'req/s':>9}   under load")
for s in servers:
    print(f"{s:18} {fmt(med(s, 'load')):>9} {fmt(med(s, 'load', 'rps'), '{:.0f}'):>9}")
EOF
cat "$OUT/summary.txt"
