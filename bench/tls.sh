#!/usr/bin/env bash
# HTTPS benchmark: giong-http + giong-tls against Axum + axum-server, both on
# rustls with the `ring` provider, plus the same two servers without TLS for
# reference, and giong-http with kernel TLS (giong-http-ktls: rustls does the
# handshake, the kernel encrypts the records after it).
#
#   bench/tls.sh [rounds] [seconds-per-measurement]
#   SERVERS="giong-http-tls giong-http-ktls" bench/tls.sh   # a subset
#   ENDPOINTS="bytes64k bytes1m file64k file1m" bench/tls.sh
#
# Method (see run.sh for the general idea)
#   * One core for the server (SERVER_CPU), three for wrk (LOAD_CPU): the load
#     generator has to encrypt too, and one core is not enough to saturate
#     the server.
#   * `json` and `users` use keep-alive connections: the handshake happens once
#     and the numbers show the cost of encrypting records.
#   * `handshake` (TLS servers only) sends `Connection: close`, so every
#     request needs a new connection and a full handshake (ECDSA P-256
#     certificate, no session resumption by wrk).
#     It is not measured without TLS: on loopback that case is limited by the
#     client. When the client's FIN goes out before the server's, the client
#     side ends up in TIME_WAIT and wrk runs out of ports that may be reused
#     (about 28 000 per second with the default port range), which says
#     nothing about the server.
#   * `bytes64k` and `bytes1m` (giong-http only) answer with 64 KiB and 1 MiB
#     bodies, where encrypting the body is most of the work. They are where
#     kernel TLS can pay off; `json` and `users` show what it costs on small
#     responses.
#   * `file64k` and `file1m` (giong-http only) send the same sizes from a
#     file each worker keeps open (`Body::file_range`): with `sendfile` over
#     plain TCP and kernel TLS, so the kernel encrypts straight from the page
#     cache; read and encrypted in pieces by rustls otherwise.
#     `file64k-open` opens the file for every request, as a static file
#     server without a cache of open files does.
#   * `cpu%` is how busy the server's core was during the measurement. Well
#     below 100 means that the load generator was the limit, not the server.
#     `us/req` is the server's CPU time (user + system) per request: kernel
#     TLS moves the encryption into system time, and this column counts both.
#   * `errors` adds up wrk's socket errors and non-2xx responses. It should be
#     0: giong-http-ktls uses `Ktls::Required`, so a connection that cannot
#     have kernel TLS fails instead of quietly measuring userspace TLS.
#
# giong-http-ktls needs the kernel's `tls` module (`sudo modprobe tls`).
# Requires: wrk (with TLS), openssl, cargo, python3. Results: bench/results/tls/
# (server-<name>.log has what each server printed, kernel TLS support included).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
SERVER_CPU=${SERVER_CPU:-3}
LOAD_CPU=${LOAD_CPU:-0,1,2}
WRK_THREADS=${WRK_THREADS:-3}
CONNS=${CONNS:-50}
OUT=${OUT:-results/tls}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 2 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost" 2>/dev/null
export TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem"

read -r -a NAMES <<<"${SERVERS:-axum giong-http axum-tls giong-http-tls giong-http-ktls}"
read -r -a EPS <<<"${ENDPOINTS:-json users handshake bytes64k bytes1m file64k file1m}"
declare -A CMD=(
  [axum]="axum/target/release/bench-axum"
  [giong-http]="env -u TLS_CERT giong-http/target/release/bench-giong-http"
  [axum-tls]="axum-tls/target/release/bench-axum-tls"
  [giong-http-tls]="env GIONG_KTLS=off giong-http/target/release/bench-giong-http"
  [giong-http-ktls]="env GIONG_KTLS=required giong-http/target/release/bench-giong-http"
)
declare -A URL=(
  [axum]="http://127.0.0.1:3003"
  [giong-http]="http://127.0.0.1:3009"
  [axum-tls]="https://127.0.0.1:3011"
  [giong-http-tls]="https://127.0.0.1:3010"
  [giong-http-ktls]="https://127.0.0.1:3010"
)
declare -A PATHS=(
  [json]=/json [users]=/users [handshake]=/json
  [bytes64k]="/bytes?n=65536" [bytes1m]="/bytes?n=1048576"
  [file64k]="/file?n=65536" [file1m]="/file?n=1048576" [file64k-open]="/file?n=65536&open=1"
)
for s in "${NAMES[@]}"; do
  [ -n "${CMD[$s]:-}" ] || { echo "unknown server $s (known: ${!CMD[*]})" >&2; exit 2; }
done
for ep in "${EPS[@]}"; do
  [ -n "${PATHS[$ep]:-}" ] || { echo "unknown endpoint $ep (known: ${!PATHS[*]})" >&2; exit 2; }
done
if [[ " ${NAMES[*]} " == *" giong-http-ktls "* ]] && [ ! -d /sys/module/tls ]; then
  echo "giong-http-ktls needs the kernel's tls module: sudo modprobe tls" >&2
  exit 1
fi

echo "building..." >&2
for d in axum axum-tls giong-http; do (cd "$d" && cargo build --release -q); done

# CPU time (user + system, in clock ticks) of a process and its threads.
ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }
HZ=$(getconf CLK_TCK)

echo "server,endpoint,round,rps,p50,p99,cpu,us_per_req,errors" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for s in "${NAMES[@]}"; do
    taskset -c "$SERVER_CPU" ${CMD[$s]} >/dev/null 2>"$OUT/server-$s.log" &
    pid=$!
    sleep 1.2
    for ep in "${EPS[@]}"; do
      case $ep in
        handshake) [[ ${URL[$s]} == https:* ]] || continue ;;
        bytes* | file*) [[ $s == giong-http* ]] || continue ;;
      esac
      args=("${URL[$s]}${PATHS[$ep]}")
      [ "$ep" = handshake ] && args=(-H "Connection: close" "${args[@]}")
      taskset -c "$LOAD_CPU" wrk -t"$WRK_THREADS" -c"$CONNS" -d2s "${args[@]}" >/dev/null 2>&1
      before=$(ticks "$pid")
      out=$(taskset -c "$LOAD_CPU" wrk -t"$WRK_THREADS" -c"$CONNS" -d"${SECS}s" --latency "${args[@]}")
      after=$(ticks "$pid")
      rps=$(awk '/Requests\/sec/{print $2}' <<<"$out")
      reqs=$(awk '/requests in/{print $1}' <<<"$out")
      p50=$(awk '$1=="50%"{print $2}' <<<"$out")
      p99=$(awk '$1=="99%"{print $2}' <<<"$out")
      cpu=$(( (after - before) * 100 / (HZ * SECS) ))
      us=$(awk -v t=$((after - before)) -v hz="$HZ" -v n="$reqs" 'BEGIN{printf "%.2f", (n > 0 ? t * 1e6 / hz / n : 0)}')
      errors=$(awk '/Socket errors/{gsub(/,/, ""); e += $4 + $6 + $8 + $10}
                    /Non-2xx/{e += $NF} END{print e + 0}' <<<"$out")
      echo "$s,$ep,$round,$rps,$p50,$p99,$cpu,$us,$errors" >> "$CSV"
    done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    sleep 0.5
    echo "round $round/$ROUNDS: $s done" >&2
  done
done

python3 - "$CSV" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys

def to_ms(v):
    for unit, k in (("us", 1e-3), ("ms", 1.0), ("s", 1e3)):
        if v.endswith(unit): return float(v[:-len(unit)]) * k
    return float(v)

rows = list(csv.DictReader(open(sys.argv[1])))
servers, endpoints = [], []
for r in rows:
    if r["server"] not in servers: servers.append(r["server"])
    if r["endpoint"] not in endpoints: endpoints.append(r["endpoint"])

med = statistics.median
print(f"{'endpoint':<10}{'server':<17}{'req/s median':>13}{'min':>10}{'max':>10}"
      f"{'p50 ms':>9}{'p99 ms':>9}{'cpu%':>6}{'us/req':>9}{'errors':>8}")
for ep in endpoints:
    for s in servers:
        sel = [r for r in rows if r["server"] == s and r["endpoint"] == ep]
        if not sel: continue
        rps = [float(r["rps"]) for r in sel]
        print(f"{ep:<10}{s:<17}{med(rps):13.0f}{min(rps):10.0f}{max(rps):10.0f}"
              f"{med(to_ms(r['p50']) for r in sel):9.2f}{med(to_ms(r['p99']) for r in sel):9.2f}"
              f"{med(int(r['cpu']) for r in sel):6.0f}{med(float(r['us_per_req']) for r in sel):9.2f}"
              f"{sum(int(r['errors']) for r in sel):8d}")
    print()
PY
