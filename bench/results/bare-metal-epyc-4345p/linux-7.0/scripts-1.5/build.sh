#!/bin/bash
# Checkouts and release builds for the 1.5.x run, on each machine: bash build.sh. Safe to run
# again; a failed build is logged and the rest go on. Binaries are copied to /root/b/bins, so
# no run script builds while another one measures.
#
#   main      origin/main: 1.5.2 and what landed after it (the reg pass, rate sweep, QUIC)
#   v151      tag v1.5.1, the release before the 1.5.2 memory and instruction work
#   pr487     refs/pull/487/head (h2 and h2-over-TLS idle memory); PR487=merged skips it
#             once main has it
#   pr429     refs/pull/429/head (outside PR, measured only): bench/giong-web with and
#             without giong-web/fast-gzip. Its bench binds 127.0.0.1:3011; the copy built
#             here binds BENCH_HOST:3011 instead (a one-line sed, logged), nothing else
#   pr180     refs/pull/180/head and its base d4cc995; bench/placement.sh builds both
#   pr186     refs/pull/186/head (spin governor, closed); bench/spin.sh builds its
#             BASE_REF itself
#   run       this branch: these scripts and bench/lab/loadgen with --header
. "$(dirname "$0")/env.sh"
RUN_BRANCH=${RUN_BRANCH:-claude/project-thread-0t1y8n}
BINS=$B/bins
mkdir -p "$BINS"
BLOG=$R15/build.log
build() { # <dir of a bench crate> <copy as> [cargo args...]
  local d=$1 as=$2; shift 2
  local name; name=$(awk -F'"' '/^name *=/{print $2; exit}' "$d/Cargo.toml")
  if (cd "$d" && cargo build --release -q "$@") >>"$BLOG" 2>&1; then
    cp "$d/target/release/$name" "$BINS/$as" && log "built $as from $d $*"
  else
    log "BUILD FAILED: $as from $d $* (see $BLOG)"
  fi
}

co "$B/run" "$RUN_BRANCH"
build "$B/run/bench/lab/loadgen" lab-loadgen

co "$B/main" main
for c in giong-http giong tokio-raw monoio axum axum-tls giong-web hold quic; do
  [ -d "$B/main/bench/$c" ] && build "$B/main/bench/$c" "main-$c"
done

co "$B/v151" v1.5.1 && build "$B/v151/bench/giong-http" v151-giong-http

if [ "${PR487:-open}" != merged ]; then
  co "$B/pr487" pull/487/head && build "$B/pr487/bench/giong-http" pr487-giong-http
fi

if co "$B/pr429" pull/429/head; then
  f=$B/pr429/bench/giong-web/src/main.rs
  sed -i 's|TcpListener::bind("127.0.0.1:3011")|TcpListener::bind(format!("{}:3011", std::env::var("BENCH_HOST").unwrap_or_else(\|_\| "127.0.0.1".into())))|' "$f"
  grep -q 'BENCH_HOST' "$f" && log "pr429: bench/giong-web binds BENCH_HOST:3011 ($(git -C "$B/pr429" diff --stat | tail -1))" \
    || log "pr429: the bind sed did not apply"
  build "$B/pr429/bench/giong-web" pr429-web-miniz
  build "$B/pr429/bench/giong-web" pr429-web-fast --features giong-web/fast-gzip
fi

co "$B/pr180" pull/180/head
co "$B/pr180-base" d4cc995ca82e886bffecf10d5c7676667ce4c374
co "$B/pr186" pull/186/head
git -C "$B/pr186" fetch -q origin 5a4cdb35c629c5c76604738feab3f9f32788d79a 2>/dev/null || true

{
  echo "# $(date -u +%FT%TZ) $(hostname): $(rustc -V)"
  for d in run main v151 pr487 pr429 pr180 pr180-base pr186; do
    [ -d "$B/$d/.git" ] && echo "$d $(git -C "$B/$d" rev-parse HEAD) $(git -C "$B/$d" log -1 --format=%s | cut -c1-80)"
  done
  ls -l "$BINS"
} | tee "$R15/builds-$(hostname).txt"
