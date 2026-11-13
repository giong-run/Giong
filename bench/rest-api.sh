#!/usr/bin/env bash
# The complete example API (examples/rest-api) against the same API on
# axum (bench/rest-api-axum): users in PostgreSQL, reads through a Redis
# cache, over HTTPS.
#
#   bench/rest-api.sh [rounds] [seconds-per-measurement]
#
# Method
#   * One PostgreSQL 16 cluster (fsync and synchronous_commit off, so writes
#     measure the servers, not the disk) and one Redis, both on DB_CPUS, with
#     10,000 users. Each API server gets SERVER_CPU and one worker; wrk runs
#     on LOAD_CPU with CONNS kept-alive HTTPS connections (TLS 1.3, P-256
#     certificate).
#   * `get`: GET /users/<id> among the first 1,000 ids, from the cache after
#     the warm-up. `list`: GET /users?limit=20&after=<random>, from
#     PostgreSQL. `create`: POST /users with a new email each time.
#   * Connections to PostgreSQL per server: GIONG_DB_POOL (2) for giong, whose
#     pool pipelines statements over a few connections, and AXUM_DB_POOL (32)
#     for deadpool-postgres, which gives each statement a connection. Axum
#     did best with 32 in a sweep of 4, 8, 16, 32 and 64 (one 3 s round
#     each: 18.1k to 20.3k req/s on `list`, 23.4k to 27.7k on `create`). One
#     Redis connection each (giong's pool of 1, redis-rs's ConnectionManager).
#     No access log on either; request ids, gzip and a 10 s timeout on both.
#   * Rounds interleave the servers; Redis is emptied before each server's
#     turn, then 2 s of warm-up before each measurement. Median and range
#     over the rounds; p99 is the median of the rounds' p99s. Responses
#     other than 2xx are counted and must stay at 0.
#   * `srv%`, `wrk%` and `db%`: how busy the server's core, wrk's core and
#     the database cores were (100 per core), from /proc/stat. The side at
#     about 100 per core is what limited the run.
#
# Requires: PostgreSQL server binaries, redis-server, wrk (with TLS),
# openssl, cargo, python3. Results: bench/results/rest-api/ (or $OUT).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
SERVER_CPU=${SERVER_CPU:-1}
LOAD_CPU=${LOAD_CPU:-0}
DB_CPUS=${DB_CPUS:-2,3}
CONNS=${CONNS:-50}
GIONG_DB_POOL=${GIONG_DB_POOL:-2}
AXUM_DB_POOL=${AXUM_DB_POOL:-32}
OUT=${OUT:-results/rest-api}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
read -r -a NAMES <<<"${SERVERS:-giong axum}"
CLK_TCK=$(getconf CLK_TCK)

# Busy ticks of a list of cores, from /proc/stat.
busy() {
  awk -v cpus=",$1," '$1 ~ /^cpu[0-9]+$/ && index(cpus, "," substr($1, 4) ",") {
    b += $2 + $3 + $4 + $7 + $8 } END { print b }' /proc/stat
}
read -r -a SCENARIOS <<<"${WORKLOADS:-get list create}"

BIN=${PG_BIN:-$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1)}
[ -x "$BIN/initdb" ] || { echo "no PostgreSQL binaries; set PG_BIN" >&2; exit 2; }

echo "building..." >&2
(cd .. && cargo build --release -q -p rest-api)
(cd rest-api-axum && cargo build --release -q)

TMP=$(mktemp -d)
AS=()
if [ "$(id -u)" = 0 ]; then
  chown postgres "$TMP"
  AS=(runuser -u postgres --)
fi
cleanup() {
  for pid in ${SRV_PID:-} ${REDIS_PID:-} ${PG_PID:-}; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  rm -rf "$TMP"
}
trap cleanup EXIT

PG_PORT=55433
"${AS[@]}" "$BIN/initdb" -D "$TMP/data" -U postgres -A trust -E UTF8 --locale=C --no-sync >/dev/null
cat >> "$TMP/data/postgresql.conf" <<CONF
listen_addresses = '127.0.0.1'
port = $PG_PORT
unix_socket_directories = '$TMP'
fsync = off
synchronous_commit = off
shared_buffers = 256MB
max_connections = 100
CONF
taskset -c "$DB_CPUS" "${AS[@]}" "$BIN/postgres" -D "$TMP/data" >"$TMP/pg.log" 2>&1 &
PG_PID=$!
for _ in $(seq 100); do "$BIN/pg_isready" -q -h 127.0.0.1 -p "$PG_PORT" && break; sleep 0.1; done

REDIS_PORT=56380
taskset -c "$DB_CPUS" redis-server --port "$REDIS_PORT" --bind 127.0.0.1 --save '' --appendonly no \
  >"$TMP/redis.log" 2>&1 &
REDIS_PID=$!
for _ in $(seq 100); do redis-cli -p "$REDIS_PORT" ping >/dev/null 2>&1 && break; sleep 0.1; done

psql() { "$BIN/psql" -q -h 127.0.0.1 -p "$PG_PORT" -U postgres "$@"; }
psql -c "
  CREATE TABLE users (id bigserial PRIMARY KEY, name text NOT NULL,
    email text NOT NULL UNIQUE, created_at timestamptz NOT NULL DEFAULT now());
  INSERT INTO users (name, email)
    SELECT 'User ' || i, 'user' || i || '@example.com' FROM generate_series(1, 10000) i;
  ANALYZE users;" >/dev/null

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 2 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null

export DATABASE_URL="postgres://postgres@127.0.0.1:$PG_PORT/postgres"
export REDIS_URL="redis://127.0.0.1:$REDIS_PORT"
export TLS_CERT="$TMP/cert.pem" TLS_KEY="$TMP/key.pem"
export WORKERS=1 ACCESS_LOG=off LISTEN=127.0.0.1:8443 CACHE_TTL_SECS=600
declare -A CMD=(
  [giong]="env DB_POOL=$GIONG_DB_POOL CACHE_POOL=1 ../target/release/rest-api"
  [axum]="env DB_POOL=$AXUM_DB_POOL rest-api-axum/target/release/bench-rest-api-axum"
)
declare -A SCRIPT=([get]=rest-api-get.lua [list]=rest-api-list.lua [create]=rest-api-post.lua)

echo "server,workload,round,rps,p50,p99,non2xx,rss_kb,srv,wrk,db" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for s in "${NAMES[@]}"; do
    redis-cli -p "$REDIS_PORT" flushall >/dev/null
    taskset -c "$SERVER_CPU" ${CMD[$s]} >"$TMP/$s.log" 2>&1 &
    SRV_PID=$!
    for _ in $(seq 100); do
      curl -skf https://127.0.0.1:8443/health >/dev/null && break
      sleep 0.1
    done
    for w in "${SCENARIOS[@]}"; do
      lua=${SCRIPT[$w]}
      taskset -c "$LOAD_CPU" wrk -t1 -c"$CONNS" -d2s -s "$lua" https://127.0.0.1:8443 >/dev/null 2>&1
      b0=("$(busy "$SERVER_CPU")" "$(busy "$LOAD_CPU")" "$(busy "$DB_CPUS")")
      out=$(taskset -c "$LOAD_CPU" wrk -t1 -c"$CONNS" -d"${SECS}s" --latency -s "$lua" https://127.0.0.1:8443)
      b1=("$(busy "$SERVER_CPU")" "$(busy "$LOAD_CPU")" "$(busy "$DB_CPUS")")
      pct() { echo $(( (b1[$1] - b0[$1]) * 100 / (CLK_TCK * SECS) )); }
      rps=$(awk '/Requests\/sec/{print $2}' <<<"$out")
      p50=$(awk '$1=="50%"{print $2}' <<<"$out")
      p99=$(awk '$1=="99%"{print $2}' <<<"$out")
      bad=$(awk '/Non-2xx/{print $NF}' <<<"$out")
      echo "$s,$w,$round,$rps,$p50,$p99,${bad:-0},$(ps -o rss= -p "$SRV_PID" | tr -d ' '),$(pct 0),$(pct 1),$(pct 2)" >> "$CSV"
    done
    kill "$SRV_PID" 2>/dev/null || true
    wait "$SRV_PID" 2>/dev/null || true
    SRV_PID=
    sleep 0.5
    echo "round $round/$ROUNDS: $s done" >&2
  done
done

python3 - "$CSV" > "$OUT/summary.txt" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
order, data = [], {}
for r in rows:
    key = (r["server"], r["workload"])
    if key not in data:
        order.append(key)
        data[key] = []
    data[key].append(r)
def us(v):
    for unit, f in (("us", 1), ("ms", 1000), ("s", 1_000_000)):
        if v.endswith(unit):
            return float(v[: -len(unit)]) * f
    return float(v)
print(f"{'server':8} {'workload':8} {'req/s median':>12} {'min':>9} {'max':>9} {'p50':>9} {'p99':>9} {'non-2xx':>8} {'rss':>8} {'srv%':>5} {'wrk%':>5} {'db%':>5}")
for key in order:
    rs = data[key]
    rps = sorted(float(r["rps"]) for r in rs if r["rps"])
    def lat(field):
        vals = sorted(us(r[field]) for r in rs if r[field])
        return f"{statistics.median(vals):.0f}us" if vals else ""
    bad = sum(int(r["non2xx"] or 0) for r in rs)
    rss = statistics.median(int(r["rss_kb"]) for r in rs if r["rss_kb"])
    cpu = lambda f: statistics.median(int(r[f]) for r in rs if r[f])
    print(f"{key[0]:8} {key[1]:8} {statistics.median(rps):12.0f} {rps[0]:9.0f} {rps[-1]:9.0f} {lat('p50'):>9} {lat('p99'):>9} {bad:8d} {rss/1024:7.1f}M {cpu('srv'):5.0f} {cpu('wrk'):5.0f} {cpu('db'):5.0f}")
PY
cat "$OUT/summary.txt"
