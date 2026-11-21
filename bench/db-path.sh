#!/usr/bin/env bash
# The whole path of a request that reads the database: HTTP in, one
# statement on PostgreSQL or MySQL, JSON out. giong (giong-web,
# giong-postgres, giong-mysql) against axum with tokio-postgres through
# deadpool-postgres, and axum with SQLx.
#
#   bench/db-path.sh [rounds] [seconds-per-measurement]
#
# Method
#   * One PostgreSQL 16 and one MySQL 8.0, both on DB_CPUS, each with a
#     users table of 10,000 rows. Each server gets SERVER_CPU and one
#     worker (a current-thread Tokio runtime for axum); wrk runs on
#     LOAD_CPU. Plain HTTP/1.1 with keep-alive, no middleware on either.
#   * `GET /pg/<id>` and `GET /my/<id>`, a random id each time:
#     `SELECT id, name, email FROM users WHERE id = $1`, answered as JSON.
#   * Two loads: CONNS (50) connections, each sending its next request as
#     soon as the answer arrives, for throughput and the p50/p99 under load;
#     then one connection, one request at a time, for the latency of a single
#     request.
#   * Connections to the database per server: GIONG_POOL (2) for giong,
#     whose pools pipeline statements over a few connections, TOKIO_POOL (32)
#     for deadpool-postgres and SQLx, which give each statement a
#     connection (axum did best with 32 in bench/rest-api.sh's sweep).
#   * `giong-pg-batch` is giong-pg with `Config::batch_reads(true)`: the
#     statements a pooled connection has ready in the same tick go to the
#     server under one `Sync` (at most 16, never waiting for more).
#   * GIONG_WORKERS (1) workers for giong's servers, on the SERVER_CPU list:
#     with 2 on two cores, the database on one is the limit, e.g.
#     SERVER_CPU=0,1 GIONG_WORKERS=2 LOAD_CPU=2 DB_CPUS=3
#     SUBJECTS="giong-pg giong-pg-batch".
#   * Rounds interleave the servers, 2 s of warm-up before each measurement.
#     Median and range of req/s over the rounds; p50 and p99 are the medians
#     of the rounds'. `srv%`, `wrk%`, `db%`: how busy the server's core,
#     wrk's core and the database cores were (100 per core). Responses other
#     than 2xx are counted and must stay at 0.
#
# SUBJECTS picks a subset (giong-pg giong-pg-batch deadpool-pg sqlx-pg giong-my sqlx-my).
# Requires: PostgreSQL server binaries, mysqld and the mysql client, wrk,
# cargo, python3. Results: bench/results/db-path/ (or $OUT).
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
SERVER_CPU=${SERVER_CPU:-1}
LOAD_CPU=${LOAD_CPU:-0}
DB_CPUS=${DB_CPUS:-2,3}
CONNS=${CONNS:-50}
GIONG_POOL=${GIONG_POOL:-2}
GIONG_WORKERS=${GIONG_WORKERS:-1}
TOKIO_POOL=${TOKIO_POOL:-32}
OUT=${OUT:-results/db-path}
read -r -a SUBJECTS <<<"${SUBJECTS:-giong-pg giong-pg-batch deadpool-pg sqlx-pg giong-my sqlx-my}"
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
CLK_TCK=$(getconf CLK_TCK)

busy() {
  awk -v cpus=",$1," '$1 ~ /^cpu[0-9]+$/ && index(cpus, "," substr($1, 4) ",") {
    b += $2 + $3 + $4 + $7 + $8 } END { print b }' /proc/stat
}

BIN=${PG_BIN:-$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1)}
[ -x "$BIN/initdb" ] || { echo "no PostgreSQL binaries; set PG_BIN" >&2; exit 2; }
MYSQLD=${MYSQL_BIN:+$MYSQL_BIN/mysqld}
MYSQLD=${MYSQLD:-$(command -v mysqld || echo /usr/sbin/mysqld)}
MYSQL=$(command -v mysql || echo "$(dirname "$MYSQLD")/mysql")
[ -x "$MYSQLD" ] && [ -x "$MYSQL" ] || { echo "no mysqld and mysql; set MYSQL_BIN" >&2; exit 2; }

echo "building..." >&2
for d in db-path-giong db-path-tokio; do (cd "$d" && cargo build --release -q); done

TMP=$(mktemp -d /var/tmp/db-path.XXXXXX)
PGAS=()
MYAS=()
if [ "$(id -u)" = 0 ]; then
  chown postgres "$TMP"
  PGAS=(runuser -u postgres --)
  MYAS=(--user=root)
fi
cleanup() {
  for pid in ${SRV_PID:-} ${PG_PID:-} ${MY_PID:-}; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  rm -rf "$TMP"
}
trap cleanup EXIT

PG_PORT=55434
"${PGAS[@]}" "$BIN/initdb" -D "$TMP/pg" -U postgres -A trust -E UTF8 --locale=C --no-sync >/dev/null
cat >> "$TMP/pg/postgresql.conf" <<CONF
listen_addresses = '127.0.0.1'
port = $PG_PORT
unix_socket_directories = '$TMP'
fsync = off
synchronous_commit = off
shared_buffers = 256MB
max_connections = 100
CONF
taskset -c "$DB_CPUS" "${PGAS[@]}" "$BIN/postgres" -D "$TMP/pg" >"$TMP/pg.log" 2>&1 &
PG_PID=$!
for _ in $(seq 100); do "$BIN/pg_isready" -q -h 127.0.0.1 -p "$PG_PORT" && break; sleep 0.1; done
"$BIN/psql" -q -h 127.0.0.1 -p "$PG_PORT" -U postgres -c "
  CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL, email text NOT NULL);
  INSERT INTO users SELECT i, 'User ' || i, 'user' || i || '@example.com'
    FROM generate_series(1, 10000) i;
  ANALYZE users;" >/dev/null

MY_PORT=53307
mkdir -p "$TMP/my"
"$MYSQLD" --no-defaults "${MYAS[@]}" --initialize-insecure --datadir="$TMP/my/data" \
  --log-error="$TMP/my/init.log"
taskset -c "$DB_CPUS" "$MYSQLD" --no-defaults "${MYAS[@]}" --datadir="$TMP/my/data" \
  --port=$MY_PORT --bind-address=127.0.0.1 --socket="$TMP/my/mysqld.sock" --mysqlx=OFF \
  --log-error="$TMP/my/server.log" --pid-file="$TMP/my/mysqld.pid" --skip-log-bin \
  --secure-file-priv=NULL --skip-name-resolve --innodb-flush-log-at-trx-commit=2 \
  --innodb-buffer-pool-size=256M --max-connections=200 &
MY_PID=$!
sql() { "$MYSQL" --no-defaults -uroot -S "$TMP/my/mysqld.sock" "$@"; }
for _ in $(seq 300); do sql -e "SELECT 1" >/dev/null 2>&1 && break; sleep 0.1; done
sql -e "
  CREATE DATABASE bench;
  CREATE USER bench@'%' IDENTIFIED BY 'bench';
  GRANT ALL ON bench.* TO bench@'%';
  SET SESSION cte_max_recursion_depth = 20000;
  CREATE TABLE bench.users (id BIGINT PRIMARY KEY, name VARCHAR(100) NOT NULL,
    email VARCHAR(100) NOT NULL);
  INSERT INTO bench.users WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
    SELECT i, CONCAT('User ', i), CONCAT('user', i, '@example.com') FROM n;
  ANALYZE TABLE bench.users;" >/dev/null
# One login over the socket caches the password: TCP logins take the fast path.
"$MYSQL" --no-defaults -ubench -pbench -S "$TMP/my/mysqld.sock" -e "SELECT 1" >/dev/null 2>&1

export PG_URL="postgres://postgres@127.0.0.1:$PG_PORT/postgres"
export MY_URL="mysql://bench:bench@127.0.0.1:$MY_PORT/bench"
LISTEN=127.0.0.1:8090
declare -A CMD=(
  [giong-pg]="env WORKERS=$GIONG_WORKERS PG_POOL=$GIONG_POOL MY_POOL=$GIONG_POOL db-path-giong/target/release/bench-db-path-giong $LISTEN"
  [giong-pg-batch]="env WORKERS=$GIONG_WORKERS PG_BATCH=1 PG_POOL=$GIONG_POOL MY_POOL=$GIONG_POOL db-path-giong/target/release/bench-db-path-giong $LISTEN"
  [giong-my]="env WORKERS=$GIONG_WORKERS PG_POOL=$GIONG_POOL MY_POOL=$GIONG_POOL db-path-giong/target/release/bench-db-path-giong $LISTEN"
  [deadpool-pg]="env PG_POOL=$TOKIO_POOL MY_POOL=$TOKIO_POOL db-path-tokio/target/release/bench-db-path-tokio $LISTEN deadpool"
  [sqlx-pg]="env PG_POOL=$TOKIO_POOL MY_POOL=$TOKIO_POOL db-path-tokio/target/release/bench-db-path-tokio $LISTEN sqlx"
  [sqlx-my]="env PG_POOL=$TOKIO_POOL MY_POOL=$TOKIO_POOL db-path-tokio/target/release/bench-db-path-tokio $LISTEN sqlx"
)

echo "subject,load,round,rps,p50,p99,non2xx,srv,wrk,db" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for s in "${SUBJECTS[@]}"; do
    db=pg
    [[ $s == *-my ]] && db=my
    export PATH_PREFIX="/$db/"
    taskset -c "$SERVER_CPU" ${CMD[$s]} >"$TMP/$s.log" 2>&1 &
    SRV_PID=$!
    for _ in $(seq 200); do
      curl -sf "http://$LISTEN${PATH_PREFIX}1" >/dev/null && break
      sleep 0.1
    done
    for load in many one; do
      c=$CONNS
      [ "$load" = one ] && c=1
      taskset -c "$LOAD_CPU" wrk -t1 -c"$c" -d2s -s db-path.lua "http://$LISTEN" >/dev/null 2>&1
      b0=("$(busy "$SERVER_CPU")" "$(busy "$LOAD_CPU")" "$(busy "$DB_CPUS")")
      out=$(taskset -c "$LOAD_CPU" wrk -t1 -c"$c" -d"${SECS}s" --latency -s db-path.lua "http://$LISTEN")
      b1=("$(busy "$SERVER_CPU")" "$(busy "$LOAD_CPU")" "$(busy "$DB_CPUS")")
      pct() { echo $(( (b1[$1] - b0[$1]) * 100 / (CLK_TCK * SECS) )); }
      rps=$(awk '/Requests\/sec/{print $2}' <<<"$out")
      p50=$(awk '$1=="50%"{print $2}' <<<"$out")
      p99=$(awk '$1=="99%"{print $2}' <<<"$out")
      bad=$(awk '/Non-2xx/{print $NF}' <<<"$out")
      echo "$s,$load,$round,$rps,$p50,$p99,${bad:-0},$(pct 0),$(pct 1),$(pct 2)" >> "$CSV"
    done
    kill "$SRV_PID" 2>/dev/null || true
    wait "$SRV_PID" 2>/dev/null || true
    SRV_PID=
    sleep 0.5
    echo "round $round/$ROUNDS: $s done" >&2
  done
done

python3 - "$CSV" "$CONNS" "$DB_CPUS" > "$OUT/summary.txt" <<'PY'
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
order, data = [], {}
for r in rows:
    key = (r["subject"], r["load"])
    if key not in data:
        order.append(key)
        data[key] = []
    data[key].append(r)
def us(v):
    for unit, f in (("us", 1), ("ms", 1000), ("s", 1_000_000)):
        if v.endswith(unit):
            return float(v[: -len(unit)]) * f
    return float(v)
print(f"load `many`: {sys.argv[2]} connections; `one`: one connection, one request at a time; "
      f"databases on CPUs {sys.argv[3]}")
print(f"{'subject':14} {'load':5} {'req/s median':>12} {'min':>9} {'max':>9} {'p50':>9} {'p99':>9} {'non-2xx':>8} {'srv%':>5} {'wrk%':>5} {'db%':>5}")
for load in ("many", "one"):
    for key in [k for k in order if k[1] == load]:
        rs = data[key]
        rps = sorted(float(r["rps"]) for r in rs if r["rps"])
        def lat(field):
            vals = sorted(us(r[field]) for r in rs if r[field])
            return f"{statistics.median(vals):.0f}us" if vals else ""
        bad = sum(int(r["non2xx"] or 0) for r in rs)
        cpu = lambda f: statistics.median(int(r[f]) for r in rs if r[f])
        print(f"{key[0]:14} {key[1]:5} {statistics.median(rps):12.0f} {rps[0]:9.0f} {rps[-1]:9.0f} {lat('p50'):>9} {lat('p99'):>9} {bad:8d} {cpu('srv'):5.0f} {cpu('wrk'):5.0f} {cpu('db'):5.0f}")
    print()
PY
cat "$OUT/summary.txt"
