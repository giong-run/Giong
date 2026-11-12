#!/usr/bin/env bash
# MySQL client benchmark: giong-mysql against mysql_async and SQLx on Tokio,
# each on one core, against the same local MySQL server.
#
#   bench/mysql.sh [rounds] [seconds-per-measurement]
#
# Method
#   * A throwaway server (mysqld --initialize-insecure in a temporary
#     directory, no binary log, redo log flushed once a second) on
#     SERVER_CPUS, a table of 10 000 users. The client runs on CLIENT_CPU, so
#     the numbers show how much one core of client gets done.
#   * Workloads: `point` reads one row by primary key, `list` reads 100 rows;
#     every column of every row is decoded into Rust values (i64, two
#     strings, a date and time).
#   * Clients: `giong` is giong-mysql's Pool of POOL_SIZE connections (4 by
#     default; statements share connections and pipeline); `async` and
#     `sqlx` are mysql_async's and SQLx's pools of the same size (a
#     connection per statement while it runs). `async` keeps connections as
#     they are when they go back to the pool; `async-default` keeps
#     mysql_async's default of resetting them (COM_RESET_CONNECTION, which
#     also drops their prepared statements). `giong-client` is one giong
#     Client shared by every task.
#   * TASKS concurrent tasks issue statements back to back. 2 s of warm-up,
#     then SECS measured. `cpu%` is the client's core; `srv%` the server's
#     cores together (100 per core), from /proc/stat. A client below 100% was
#     waiting for the server.
#
# WORKLOADS and CLIENTS pick a subset, for example
#   POOL_SIZE=16 WORKLOADS=point CLIENTS=async OUT=results/x bench/mysql.sh 3
#
# Requires: mysqld and the mysql client (MYSQL_BIN, the directory holding
# mysqld, or /usr/sbin and PATH), cargo, python3. Results:
# bench/results/mysql/.
set -euo pipefail
cd "$(dirname "$0")"

ROUNDS=${1:-5}
SECS=${2:-5}
CLIENT_CPU=${CLIENT_CPU:-3}
SERVER_CPUS=${SERVER_CPUS:-0,1,2}
TASKS=${TASKS:-64}
export POOL_SIZE=${POOL_SIZE:-4}
WORKLOADS=${WORKLOADS:-point list}
CLIENTS=${CLIENTS:-giong async async-default sqlx giong-client}
OUT=${OUT:-results/mysql}
mkdir -p "$OUT"
CSV="$OUT/raw.csv"
export CLK_TCK=$(getconf CLK_TCK)

MYSQLD=${MYSQL_BIN:+$MYSQL_BIN/mysqld}
MYSQLD=${MYSQLD:-$(command -v mysqld || echo /usr/sbin/mysqld)}
MYSQL=$(command -v mysql || echo "$(dirname "$MYSQLD")/mysql")
[ -x "$MYSQLD" ] && [ -x "$MYSQL" ] || { echo "no mysqld and mysql; set MYSQL_BIN" >&2; exit 2; }

echo "building..." >&2
for d in mysql-giong mysql-tokio; do (cd "$d" && cargo build --release -q); done

TMP=$(mktemp -d)
AS=()
[ "$(id -u)" = 0 ] && AS=(--user=root)
cleanup() {
  if [ -n "${MY_PID:-}" ]; then
    kill "$MY_PID" 2>/dev/null || true
    wait "$MY_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT
PORT=53306
"$MYSQLD" --no-defaults "${AS[@]}" --initialize-insecure --datadir="$TMP/data" \
  --log-error="$TMP/init.log"
taskset -c "$SERVER_CPUS" "$MYSQLD" --no-defaults "${AS[@]}" --datadir="$TMP/data" \
  --port=$PORT --bind-address=127.0.0.1 --socket="$TMP/mysqld.sock" --mysqlx=OFF \
  --log-error="$TMP/server.log" --pid-file="$TMP/mysqld.pid" --skip-log-bin --secure-file-priv=NULL \
  --skip-name-resolve --innodb-flush-log-at-trx-commit=2 --innodb-buffer-pool-size=256M \
  --max-connections=200 &
MY_PID=$!
sql() { "$MYSQL" --no-defaults -uroot -S "$TMP/mysqld.sock" "$@"; }
for _ in $(seq 300); do sql -e "SELECT 1" >/dev/null 2>&1 && break; sleep 0.1; done
sql -e "
  CREATE DATABASE bench;
  CREATE USER bench@'%' IDENTIFIED BY 'bench';
  GRANT ALL ON bench.* TO bench@'%';
  SET SESSION cte_max_recursion_depth = 20000;
  CREATE TABLE bench.bench_users (id BIGINT PRIMARY KEY, name VARCHAR(100) NOT NULL,
    email VARCHAR(100) NOT NULL, created_at DATETIME(6) NOT NULL);
  INSERT INTO bench.bench_users WITH RECURSIVE n (i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
    SELECT i, CONCAT('user ', i), CONCAT('user', i, '@example.com'), NOW(6) - INTERVAL i MINUTE FROM n;
  ANALYZE TABLE bench.bench_users;" >/dev/null
# One login over the socket puts the password in the server's cache, so
# every client's TCP logins take caching_sha2_password's fast path.
"$MYSQL" --no-defaults -ubench -pbench -S "$TMP/mysqld.sock" -e "SELECT 1" >/dev/null 2>&1
URL="mysql://bench:bench@127.0.0.1:$PORT/bench"

# Busy ticks of the server's cores, from /proc/stat.
server_ticks() {
  awk -v cpus=",$SERVER_CPUS," '$1 ~ /^cpu[0-9]+$/ && index(cpus, "," substr($1, 4) ",") {
    busy += $2 + $3 + $4 + $7 + $8 } END { print busy }' /proc/stat
}

echo "client,workload,round,ops,p50,p99,cpu,srv" > "$CSV"
for round in $(seq 1 "$ROUNDS"); do
  for workload in $WORKLOADS; do
    for client in $CLIENTS; do
      case $client in
        giong) cmd=(mysql-giong/target/release/bench-mysql-giong "$URL" "$workload" pool) ;;
        giong-client) cmd=(mysql-giong/target/release/bench-mysql-giong "$URL" "$workload" client) ;;
        *) cmd=(mysql-tokio/target/release/bench-mysql-tokio "$URL" "$workload" "$client") ;;
      esac
      out_file="$TMP/out"
      taskset -c "$CLIENT_CPU" "${cmd[@]}" "$TASKS" "$SECS" >"$out_file" &
      cpid=$!
      sleep 2.2
      before=$(server_ticks)
      sleep "$SECS"
      after=$(server_ticks)
      wait "$cpid"
      out=$(cat "$out_file")
      srv=$(( (after - before) * 100 / (CLK_TCK * SECS) ))
      get() { sed -n "s/.*$1=\([0-9.]*\).*/\1/p" <<<"$out"; }
      echo "$client,$workload,$round,$(get ops),$(get p50_us),$(get p99_us),$(get cpu),$srv" >> "$CSV"
    done
  done
  echo "round $round/$ROUNDS done" >&2
done

python3 - "$CSV" "$TASKS" "$POOL_SIZE" "$ROUNDS" "$SECS" "$("$MYSQLD" --version)" <<'PY' | tee "$OUT/summary.txt"
import csv, statistics, sys
rows = list(csv.DictReader(open(sys.argv[1])))
med = statistics.median
print(sys.argv[6])
print(f"{sys.argv[2]} tasks, pools of {sys.argv[3]}, {sys.argv[4]} rounds of {sys.argv[5]} s; median, min and max of the rounds")
print(f"{'workload':<9}{'client':<14}{'ops/s median':>13}{'min':>9}{'max':>9}{'p50 us':>8}{'p99 us':>8}{'cpu%':>6}{'srv%':>6}")
for w in ("point", "list"):
    for c in ("giong", "async", "async-default", "sqlx", "giong-client"):
        sel = [r for r in rows if (r["workload"], r["client"]) == (w, c)]
        if not sel: continue
        ops = [float(r["ops"]) for r in sel]
        print(f"{w:<9}{c:<14}{med(ops):13.0f}{min(ops):9.0f}{max(ops):9.0f}"
              f"{med(float(r['p50']) for r in sel):8.0f}{med(float(r['p99']) for r in sel):8.0f}"
              f"{med(float(r['cpu']) for r in sel):6.0f}{med(float(r['srv']) for r in sel):6.0f}")
    print()
PY
