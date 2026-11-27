#!/bin/bash
# Builds of bench/giong-http against each other over the private network: the server on
# this machine (br-1, address S), wrk and h2load on br-2 (address L), so neither the load
# generator nor loopback shares the server's cores.
#
#   BINS="v1.5.1=/root/b/v151/bench/giong-http/target/release/bench-giong-http main=..." \
#     OUT=/root/r15/out/ab bash ab-net.sh [rounds] [seconds]
#
#   WORKERS   server cores, first N of SERVER_CPUS (default "1 4"; SERVER_CPUS 0-7, one per
#             physical core)
#   CASES     http:json http:users tls:json tls:users tls:handshake h2c:json h2:json h2:json-mux
#             (wrk for http/tls, Connection: close for handshake; h2load for h2c and h2)
#
# The builds take turns in a rotating order within every round (A B C, B C A, C A B), so a
# drift of the machines or of the network hits all of them alike. Recorded per measurement:
# requests per second, p50 and p99 (wrk; h2load gives a mean), the server's CPU time per
# request and how busy its cores were, how busy br-2's CPUs were, errors.
. "$(dirname "$0")/env.sh"
ROUNDS=${1:-6}
SECS=${2:-10}
OUT=${OUT:-$R15/out/ab}
read -r -a WORKERS <<<"${WORKERS:-1 4}"
read -r -a CASES <<<"${CASES:-http:json http:users tls:json tls:users tls:handshake h2c:json h2:json h2:json-mux}"
SERVER_CPUS=${SERVER_CPUS:-0-7}
CONNS_PER_WORKER=${CONNS_PER_WORKER:-64}
LOAD_THREADS=${LOAD_THREADS:-16}
read -r -a ITEMS <<<"${BINS:?BINS=\"label=path ...\"}"
[ -n "$S" ] && [ -n "$L" ] || { echo "no /root/r15/hosts (S=..., L=...)" >&2; exit 2; }
mkdir -p "$OUT"
CSV=$OUT/raw.csv
expand() { local o=() p; IFS=, read -r -a ps <<<"$1"; for p in "${ps[@]}"; do if [[ $p == *-* ]]; then o+=($(seq "${p%-*}" "${p#*-}")); else o+=("$p"); fi; done; echo "${o[@]}"; }
read -r -a CPUS <<<"$(expand "$SERVER_CPUS")"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
  -days 2 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:$S" 2>/dev/null
{
  echo "# ab-net.sh $(date -u +%FT%TZ): server $(hostname) $(uname -r) on CPUs ${SERVER_CPUS}, load from $L with $LOAD_THREADS threads, $SECS s per measurement after 2 s of warm-up, $CONNS_PER_WORKER connections per worker"
  for it in "${ITEMS[@]}"; do echo "# ${it%%=*} = ${it#*=} ($(cd "$(dirname "${it#*=}")/../../.." 2>/dev/null && git log -1 --format='%h %s' 2>/dev/null))"; done
} >"$OUT/conditions.txt"
[ -f "$CSV" ] || echo "build,workers,case,round,order,rps,p50,p99,server_busy,us_per_req,load_busy,errors" >"$CSV"

measure() { # label n case pos
  local label=$1 n=$2 case=$3 pos=$4 mode=${3%%:*} ep=${3#*:} bin="" it
  for it in "${ITEMS[@]}"; do [ "${it%%=*}" = "$label" ] && bin=${it#*=}; done
  local cpus=("${CPUS[@]:0:$n}") conns=$((CONNS_PER_WORKER * n)) env=(env -u TLS_CERT) url path args out
  case $mode in tls | h2) env=(env TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem" GIONG_KTLS=off) ;; esac
  case $ep in json | json-mux | handshake) path=/json ;; users) path=/users ;; esac
  case $mode in http | h2c) url="http://$S:3009$path" ;; tls | h2) url="https://$S:3010$path" ;; esac
  BENCH_HOST=$S "${env[@]}" taskset -c "$(IFS=,; echo "${cpus[*]}")" "$bin" >/dev/null 2>>"$OUT/server.log" &
  local pid=$!
  sleep 1.5
  case $mode in
    http | tls)
      args=(-t"$LOAD_THREADS" -c"$conns" --timeout 5s)
      [ "$ep" = handshake ] && args+=(-H "Connection: close")
      on_l "$(printf '%q ' wrk "${args[@]}" -d2s "$url")" >/dev/null 2>&1
      ;;
    h2c | h2)
      args=(-t"$LOAD_THREADS" -c"$conns" -m10)
      [ "$ep" = json-mux ] && args=(-t"$n" -c"$((3 * n))" -m100)
      on_l "$(printf '%q ' h2load "${args[@]}" -D2 "$url")" >/dev/null 2>&1
      ;;
  esac
  local t0 b0 bt0 lb0 lt0 t1 b1 bt1 lb1 lt1
  t0=$(ticks "$pid"); read -r b0 bt0 <<<"$(cpu_busy "${cpus[@]}")"; read -r lb0 lt0 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
  case $mode in
    http | tls) out=$(on_l "$(printf '%q ' wrk "${args[@]}" -d"${SECS}s" --latency "$url")" 2>&1) ;;
    h2c | h2) out=$(on_l "$(printf '%q ' h2load "${args[@]}" -D"$SECS" "$url")" 2>&1) ;;
  esac
  t1=$(ticks "$pid"); read -r b1 bt1 <<<"$(cpu_busy "${cpus[@]}")"; read -r lb1 lt1 <<<"$(on_l "$(declare -f cpu_busy); cpu_busy all")"
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  local rps p50 p99 reqs errors
  case $mode in
    http | tls)
      rps=$(awk '/Requests\/sec/{print $2}' <<<"$out"); reqs=$(awk '/requests in/{print $1}' <<<"$out")
      p50=$(awk '$1=="50%"{print $2}' <<<"$out"); p99=$(awk '$1=="99%"{print $2}' <<<"$out")
      errors=$(awk '/Socket errors/{gsub(/,/, ""); e += $4 + $6 + $8 + $10} /Non-2xx/{e += $NF} END{print e + 0}' <<<"$out")
      ;;
    h2c | h2)
      rps=$(awk '/^finished in/{gsub(/,/, ""); print $4}' <<<"$out"); reqs=$(awk '/^requests:/{print $8}' <<<"$out")
      p50=$(awk '/^time for request:/{print $6}' <<<"$out"); p99=-
      errors=$(awk '/^requests:/{print $10 + $12}' <<<"$out")
      ;;
  esac
  local sbusy us lbusy
  sbusy=$(awk -v b=$((b1 - b0)) -v t=$((bt1 - bt0)) 'BEGIN{printf "%.3f", t ? b/t : 0}')
  us=$(awk -v t=$((t1 - t0)) -v hz="$HZ" -v n="${reqs:-0}" 'BEGIN{printf "%.3f", (n > 0 ? t * 1e6 / hz / n : 0)}')
  lbusy=$(awk -v b=$((lb1 - lb0)) -v t=$((lt1 - lt0)) 'BEGIN{printf "%.3f", t ? b/t : 0}')
  echo "$label,$n,$case,$r,$pos,${rps:-0},${p50:--},${p99:--},$sbusy,$us,$lbusy,${errors:-0}" >>"$CSV"
  sleep 0.5
}

labels=(); for it in "${ITEMS[@]}"; do labels+=("${it%%=*}"); done
for r in $(seq 1 "$ROUNDS"); do
  for n in "${WORKERS[@]}"; do
    for c in "${CASES[@]}"; do
      k=$(( (r - 1) % ${#labels[@]} ))
      order=("${labels[@]:$k}" "${labels[@]:0:$k}")
      pos=0
      for label in "${order[@]}"; do pos=$((pos + 1)); measure "$label" "$n" "$c" "$pos"; done
    done
    log "ab-net round $r/$ROUNDS, $n worker(s) done"
  done
done
python3 "$(dirname "$0")/summarize.py" ab "$CSV" | tee "$OUT/summary.txt"
