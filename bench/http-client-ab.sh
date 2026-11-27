#!/usr/bin/env bash
# A/B of two builds of giong-http's client: the same benchmark client
# (bench/http-client-giong) built from two trees, run in turn against one
# server, so that both see the same machine at about the same time.
#
#   A=<tree> B=<tree> bench/http-client-ab.sh [rounds] [seconds]
#
# Method
#   * A and B are checkouts of the repository (main and a change, say);
#     each one's bench/http-client-giong is built in release. The server is
#     A's bench/giong-http on SERVER_CPUS, over plain HTTP on port 3009 and
#     HTTPS on port 3010, as in bench/http-client.sh.
#   * Each round runs every case with A, then with B, on CLIENT_CPU: 64
#     tasks, at most 16 connections, 2 s of warm-up and SECS measured.
#     Interleaving keeps the drift of a shared VM out of the comparison,
#     which a run of A then a run of B does not.
#   * `ops` is requests per second; the client's user and system CPU time
#     come from bash's `time` over the whole run, warm-up included, and are
#     divided by the requests of the whole run (ops times 2 + SECS).
#
# CASES picks the cases: "scheme-workload" with scheme http, https, h2
# (TLS, ALPN) or h2c, and workload json or users (default: all eight).
#
# Requires: cargo, openssl, taskset. Results: OUT (default results/http-client-ab).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-8}
SECS=${2:-5}
CLIENT_CPU=${CLIENT_CPU:-3}
SERVER_CPUS=${SERVER_CPUS:-0,1,2}
CASES=${CASES:-http-json http-users https-json https-users h2-json h2-users h2c-json h2c-users}
OUT=${OUT:-results/http-client-ab}
: "${A:?A=<tree> names the first checkout}" "${B:?B=<tree> names the second checkout}"
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

echo "building..." >&2
(cd "$A/bench/giong-http" && cargo build --release -q)
for t in "$A" "$B"; do (cd "$t/bench/http-client-giong" && cargo build --release -q); done
BIN_A="$A/bench/http-client-giong/target/release/bench-http-client-giong"
BIN_B="$B/bench/http-client-giong/target/release/bench-http-client-giong"
SERVER="$A/bench/giong-http/target/release/bench-giong-http"

TMP=$(mktemp -d)
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; done
  rm -rf "$TMP"
}
trap cleanup EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 \
  -subj "/CN=bench CA" -keyout "$TMP/ca.key" -out "$TMP/ca.pem" 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=localhost \
  -keyout "$TMP/key.pem" -out "$TMP/req.pem" 2>/dev/null
printf "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n" >"$TMP/ext"
openssl x509 -req -in "$TMP/req.pem" -CA "$TMP/ca.pem" -CAkey "$TMP/ca.key" -CAcreateserial \
  -days 1 -extfile "$TMP/ext" -out "$TMP/cert.pem" 2>/dev/null
taskset -c "$SERVER_CPUS" "$SERVER" >/dev/null 2>&1 &
PIDS+=($!)
TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem" taskset -c "$SERVER_CPUS" "$SERVER" >/dev/null 2>&1 &
PIDS+=($!)
sleep 1

echo "case,build,round,ops,user_s,sys_s,user_us_per_req,sys_us_per_req" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for case in $CASES; do
    scheme=${case%-*} workload=${case#*-}
    case $scheme in
      http) url="http://127.0.0.1:3009/$workload" http=1 ;;
      https) url="https://localhost:3010/$workload" http=1 ;;
      h2) url="https://localhost:3010/$workload" http=2 ;;
      h2c) url="http://127.0.0.1:3009/$workload" http=h2c ;;
      *) echo "unknown scheme in $case" >&2; exit 2 ;;
    esac
    for build in A B; do
      bin=BIN_$build
      out=$( { TIMEFORMAT="TIME %U %S"
        time CA="$TMP/ca.pem" HTTP=$http POOL_SIZE=16 taskset -c "$CLIENT_CPU" "${!bin}" "$url" 64 "$SECS"; } 2>&1 )
      ops=$(sed -n 's/.*ops=\([0-9.]*\).*/\1/p' <<<"$out" | head -1)
      read -r user sys < <(sed -n 's/^TIME \([0-9.]*\) \([0-9.]*\)$/\1 \2/p' <<<"$out")
      awk -v c="$case" -v b="$build" -v r="$round" -v o="$ops" -v u="$user" -v s="$sys" -v t="$SECS" \
        'BEGIN { n = o * (t + 2); printf "%s,%s,%s,%s,%s,%s,%.3f,%.3f\n", c, b, r, o, u, s, u * 1e6 / n, s * 1e6 / n }' >> "$CSV"
    done
  done
  echo "round $round of $ROUNDS done" >&2
done

python3 - "$CSV" > "$OUT/summary.txt" <<'EOF'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
cases = list(dict.fromkeys(r["case"] for r in rows))
med = lambda c, b, k: statistics.median(float(r[k]) for r in rows if r["case"] == c and r["build"] == b)
print(f"{'case':12} {'A req/s':>9} {'B req/s':>9} {'change':>7} {'A user us':>9} {'B user us':>9} {'change':>7} {'A cpu us':>8} {'B cpu us':>8} {'change':>7}")
for c in cases:
    oa, ob = med(c, "A", "ops"), med(c, "B", "ops")
    ua, ub = med(c, "A", "user_us_per_req"), med(c, "B", "user_us_per_req")
    ta = statistics.median(float(r["user_us_per_req"]) + float(r["sys_us_per_req"]) for r in rows if r["case"] == c and r["build"] == "A")
    tb = statistics.median(float(r["user_us_per_req"]) + float(r["sys_us_per_req"]) for r in rows if r["case"] == c and r["build"] == "B")
    print(f"{c:12} {oa:9.0f} {ob:9.0f} {100 * (ob / oa - 1):+6.1f}% {ua:9.3f} {ub:9.3f} {100 * (ub / ua - 1):+6.1f}% {ta:8.3f} {tb:8.3f} {100 * (tb / ta - 1):+6.1f}%")
print("\nMedians of the rounds; A is the first tree, B the second.")
EOF
cat "$OUT/summary.txt"
