#!/bin/bash
# Builds added after the first build.sh, from the run checkout only: bench/h3, bench/c1m, and
# bench/giong-http with BENCH_PORTS (C1M) for main and for PR #487. It leaves /root/b/main
# where build.sh put it, so the soaks stay on the commit the benches measured. On both
# machines, never while a measurement runs: bash build-late.sh
. "$(dirname "$0")/env.sh"
mkdir -p "$BIN_DIR"
log "build-late: run checkout at $(git -C "$B/run" rev-parse --short HEAD)"
build "$B/run/bench/h3" "h3-server-giong h3-server-tokio h3-client"
build "$B/run/bench/c1m" c1m-hold
build "$B/run/bench/giong-http" c1m-main
if [ -x "$BIN_DIR/pr487-giong-http" ] && [ -d "$B/pr487/bench/giong-http" ]; then
  f=bench/giong-http/src/main.rs
  cp "$B/run/$f" "$B/pr487/$f"
  build "$B/pr487/bench/giong-http" c1m-pr487
  git -C "$B/pr487" checkout -q -- "$f"
fi
ls -l "$BIN_DIR" | tee "$R15/builds-late-$(hostname).txt"
