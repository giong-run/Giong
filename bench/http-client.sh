#!/usr/bin/env bash
# HTTP client benchmark: giong-http's client against reqwest on Tokio, each
# on one core, against the same local server.
#
#   bench/http-client.sh [rounds] [seconds-per-measurement]
#
# Method
#   * The server is bench/giong-http (giong-http's server, one worker per
#     core) on SERVER_CPUS, over plain HTTP on port 3009 and HTTPS on port
#     3010 (a certificate from an authority made for the run). The client runs on
#     CLIENT_CPU, so the numbers show how much one core of client gets done.
#   * Workloads: `json` is a 27-byte JSON answer, `users` an 8.5 KiB one;
#     each client reads the whole body of every response.
#   * Clients: `giong` is one giong_http::client::Client shared by every
#     task, with its default of at most 16 connections to a host;
#     `giong-64` allows 64 (one per task). `reqwest` is one reqwest::Client
#     (0.12, rustls) on a current-thread Tokio runtime, with its default
#     pool, which has no limit: one connection per task.
#   * TASKS concurrent tasks send requests back to back. 2 s of warm-up,
#     then SECS measured. `cpu%` is the client's core; `srv%` the server's
#     cores together (100 per core), from /proc/stat.
#
# WORKLOADS, SCHEMES and CLIENTS pick a subset, for example
#   WORKLOADS=json SCHEMES=https CLIENTS=reqwest OUT=results/x bench/http-client.sh 3
#
# Requires: cargo, openssl, python3. Results: bench/results/http-client/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
CLIENT_CPU=${CLIENT_CPU:-3}
SERVER_CPUS=${SERVER_CPUS:-0,1,2}
TASKS=${TASKS:-64}
WORKLOADS=${WORKLOADS:-json users}
SCHEMES=${SCHEMES:-http https}
CLIENTS=${CLIENTS:-giong giong-64 reqwest}
OUT=${OUT:-results/http-client}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
export CLK_TCK=$(getconf CLK_TCK)

echo "building..." >&2
for d in giong-http http-client-giong http-client-tokio; do (cd "$d" && cargo build --release -q); done

TMP=$(mktemp -d)
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; done
  rm -rf "$TMP"
}
trap cleanup EXIT
# A certificate authority for the run, and a certificate for localhost
# signed by it (webpki refuses a self-signed authority as the server's own).
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 \
  -subj "/CN=bench CA" -keyout "$TMP/ca.key" -out "$TMP/ca.pem" 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=localhost \
  -keyout "$TMP/key.pem" -out "$TMP/req.pem" 2>/dev/null
printf "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n" >"$TMP/ext"
openssl x509 -req -in "$TMP/req.pem" -CA "$TMP/ca.pem" -CAkey "$TMP/ca.key" -CAcreateserial \
  -days 1 -extfile "$TMP/ext" -out "$TMP/cert.pem" 2>/dev/null
SERVER=giong-http/target/release/bench-giong-http
taskset -c "$SERVER_CPUS" "$SERVER" >/dev/null 2>&1 &
PIDS+=($!)
TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem" taskset -c "$SERVER_CPUS" "$SERVER" >/dev/null 2>&1 &
PIDS+=($!)
sleep 1

# Busy ticks of the server's cores, from /proc/stat.
server_ticks() {
  awk -v cpus=",$SERVER_CPUS," '$1 ~ /^cpu[0-9]+$/ && index(cpus, "," substr($1, 4) ",") {
    busy += $2 + $3 + $4 + $7 + $8 } END { print busy }' /proc/stat
}

echo "client,scheme,workload,round,ops,p50,p99,cpu,srv" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for scheme in $SCHEMES; do
    case $scheme in
      http) url_base="http://127.0.0.1:3009" ;;
      https) url_base="https://localhost:3010" ;;
    esac
    for workload in $WORKLOADS; do
      for client in $CLIENTS; do
        case $client in
          giong) cmd=(env POOL_SIZE=16 http-client-giong/target/release/bench-http-client-giong) ;;
          giong-64) cmd=(env POOL_SIZE=64 http-client-giong/target/release/bench-http-client-giong) ;;
          reqwest) cmd=(http-client-tokio/target/release/bench-http-client-tokio) ;;
        esac
        out_file="$TMP/out"
        CA="$TMP/ca.pem" taskset -c "$CLIENT_CPU" "${cmd[@]}" "$url_base/$workload" "$TASKS" "$SECS" >"$out_file" &
        cpid=$!
        sleep 2.2
        before=$(server_ticks)
        sleep "$SECS"
        after=$(server_ticks)
        wait "$cpid"
        out=$(cat "$out_file")
        srv=$(( (after - before) * 100 / (CLK_TCK * SECS) ))
        get() { sed -n "s/.*$1=\([0-9.]*\).*/\1/p" <<<"$out"; }
        echo "$client,$scheme,$workload,$round,$(get ops),$(get p50_us),$(get p99_us),$(get cpu),$srv" >> "$CSV"
      done
    done
  done
  echo "round $round/$ROUNDS done" >&2
done

python3 - "$CSV" "$TASKS" "$ROUNDS" "$SECS" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
med = statistics.median
print(f"{sys.argv[2]} tasks, {sys.argv[3]} rounds of {sys.argv[4]} s; median, min and max of the rounds")
print(f"{'scheme':<7}{'workload':<9}{'client':<10}{'req/s median':>13}{'min':>9}{'max':>9}{'p50 us':>8}{'p99 us':>8}{'cpu%':>6}{'srv%':>6}")
for s in ("http", "https"):
    for w in ("json", "users"):
        for c in ("giong", "giong-64", "reqwest"):
            sel = [r for r in rows if (r["scheme"], r["workload"], r["client"]) == (s, w, c)]
            if not sel: continue
            ops = [float(r["ops"]) for r in sel]
            print(f"{s:<7}{w:<9}{c:<10}{med(ops):13.0f}{min(ops):9.0f}{max(ops):9.0f}"
                  f"{med(float(r['p50']) for r in sel):8.0f}{med(float(r['p99']) for r in sel):8.0f}"
                  f"{med(float(r['cpu']) for r in sel):6.0f}{med(float(r['srv']) for r in sel):6.0f}")
        print()
PY
