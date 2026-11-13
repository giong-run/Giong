#!/usr/bin/env bash
# Benchmark: giong-web against axum, the same handlers on both (serde JSON,
# a path parameter, a typed query), with giong-http's bare handler as the
# floor. Then both again with request ids, gzip and a timeout
# (MIDDLEWARE=1: giong-web's own middleware, tower-http's for axum).
#
#   bench/web.sh [rounds] [seconds-per-measurement]
#
# Method as in bench/run.sh: server pinned to SERVER_CPU, wrk to LOAD_CPU,
# interleaved rounds, a warm-up before each measurement, median and range
# over the rounds. Results: bench/results/web/ (or $OUT).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
SERVER_CPU=${SERVER_CPU:-1}
LOAD_CPU=${LOAD_CPU:-0}
CONNS=${CONNS:-50}
WRK_THREADS=${WRK_THREADS:-1}
OUT=${OUT:-results/web}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
for d in axum giong-web giong-http; do (cd "$d" && cargo build --release -q); done

declare -A CMD=(
  [axum]="axum/target/release/bench-axum"
  [giong-web]="giong-web/target/release/bench-giong-web"
  [giong-http]="giong-http/target/release/bench-giong-http"
  [axum+mw]="env MIDDLEWARE=1 axum/target/release/bench-axum"
  [giong-web+mw]="env MIDDLEWARE=1 giong-web/target/release/bench-giong-web"
)
declare -A PORT=([axum]=3003 [giong-web]=3011 [giong-http]=3009 [axum+mw]=3003 [giong-web+mw]=3011)
declare -A ENDPOINTS=(
  [axum]="json users users/42 compute?n=25"
  [giong-web]="json users users/42 compute?n=25"
  [giong-http]="json users compute?n=25"
  [axum+mw]="json users"
  [giong-web+mw]="json users"
)
read -r -a NAMES <<<"${SERVERS:-axum giong-web giong-http axum+mw giong-web+mw}"

echo "server,endpoint,round,rps,p50,p99,rss_kb" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for s in "${NAMES[@]}"; do
    taskset -c "$SERVER_CPU" ${CMD[$s]} >/dev/null 2>&1 &
    pid=$!
    sleep 1.2
    base="http://127.0.0.1:${PORT[$s]}"
    # The middleware runs ask for gzip, as browsers do.
    hdr=()
    [[ $s == *+mw ]] && hdr=(-H "Accept-Encoding: gzip")
    for ep in ${ENDPOINTS[$s]}; do
      taskset -c "$LOAD_CPU" wrk -t"$WRK_THREADS" -c"$CONNS" -d2s "${hdr[@]}" "$base/$ep" >/dev/null 2>&1
      out=$(taskset -c "$LOAD_CPU" wrk -t"$WRK_THREADS" -c"$CONNS" -d"${SECS}s" --latency "${hdr[@]}" "$base/$ep")
      rps=$(awk '/Requests\/sec/{print $2}' <<<"$out")
      p50=$(awk '$1=="50%"{print $2}' <<<"$out")
      p99=$(awk '$1=="99%"{print $2}' <<<"$out")
      echo "$s,$ep,$round,$rps,$p50,$p99,$(ps -o rss= -p $pid | tr -d ' ')" >> "$CSV"
    done
    if [[ $s != *+mw ]]; then
      out=$(taskset -c "$LOAD_CPU" wrk -t"$WRK_THREADS" -c"$CONNS" -d"${SECS}s" -s pipeline.lua "$base")
      echo "$s,pipelined-json,$round,$(awk '/Requests\/sec/{print $2}' <<<"$out"),,,$(ps -o rss= -p $pid | tr -d ' ')" >> "$CSV"
    fi
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    sleep 0.5
    echo "round $round/$ROUNDS: $s done" >&2
  done
done

python3 - "$CSV" > "$OUT/summary.txt" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
order = []
data = {}
for r in rows:
    key = (r["server"], r["endpoint"])
    if key not in data:
        order.append(key)
        data[key] = []
    data[key].append(r)
print(f"{'server':14} {'endpoint':16} {'req/s median':>12} {'min':>9} {'max':>9} {'p50':>9} {'p99':>9}")
for key in order:
    rs = data[key]
    rps = sorted(float(r["rps"]) for r in rs if r["rps"])
    med = statistics.median(rps)
    def us(v):
        for unit, f in (("us", 1), ("ms", 1000), ("s", 1_000_000)):
            if v.endswith(unit):
                return float(v[: -len(unit)]) * f
        return float(v)
    def lat(field):
        vals = sorted(us(r[field]) for r in rs if r[field])
        return f"{statistics.median(vals):.0f}us" if vals else ""
    print(f"{key[0]:14} {key[1]:16} {med:12.0f} {rps[0]:9.0f} {rps[-1]:9.0f} {lat('p50'):>9} {lat('p99'):>9}")
PY
cat "$OUT/summary.txt"
