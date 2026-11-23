#!/usr/bin/env bash
# Benchmark: how long a small service takes to build, how much disk its build
# uses and how big its binary is. The same three-route JSON service (a path
# parameter, a JSON body, a 404) with the same layers (request ids, a log line per
# request, compression, a timeout), written twice: on giong-web from crates.io
# (build-giong/) and on axum + tokio + tower-http (build-axum/).
#
#   bench/build.sh [rounds]
#
# Per app and round: a clean build, a rebuild after `touch src/main.rs` and after
# a one-line edit, for the dev profile and for `debug = "line-tables-only"`
# (fastdev); then a clean release build, a release rebuild after an edit, an
# `opt-level = "s"` + `panic = "abort"` build (small) and the stripped release
# binary. Crates are fetched first, so no time is spent downloading. Needs bc and
# strip. Results: bench/results/build/ (or $OUT): raw.txt, one line per measurement.
set -u
cd "$(dirname "$0")"
ROUNDS=${1:-2}
OUT=${OUT:-results/build}
mkdir -p "$OUT"
RAW="$OUT/raw.txt"

t() { local s; s=$(date +%s.%N); "$@" >/dev/null 2>&1 || echo "FAIL $*"; echo "$(echo "$(date +%s.%N) - $s" | bc)"; }

measure() {   # <dir> <label> <binary>
  local dir=$1 n=$2 bin=$3 prof c i1 i2 d sz b
  (
  cd "$dir" || exit 1
  cargo fetch -q
  for prof in dev fastdev; do
    rm -rf target
    c=$(t cargo build --profile $prof)
    touch src/main.rs; i1=$(t cargo build --profile $prof)
    sed -i 's/Hello/Hallo/' src/main.rs; i2=$(t cargo build --profile $prof); sed -i 's/Hallo/Hello/' src/main.rs
    d=$([ $prof = dev ] && echo debug || echo $prof)
    sz=$(du -sm target | cut -f1); b=$(stat -c %s target/$d/$bin 2>/dev/null || echo NA)
    echo "$n $prof clean=$c touch=$i1 edit=$i2 target_MB=$sz bin_bytes=$b"
  done
  rm -rf target
  c=$(t cargo build --release); echo "$n release clean=$c bin_bytes=$(stat -c %s target/release/$bin) target_MB=$(du -sm target | cut -f1)"
  sed -i 's/Hello/Hallo/' src/main.rs; i2=$(t cargo build --release); sed -i 's/Hallo/Hello/' src/main.rs; echo "$n release edit=$i2"
  c=$(t cargo build --profile small); echo "$n small clean=$c bin_bytes=$(stat -c %s target/small/$bin)"
  cp target/release/$bin "$TMPDIR_STRIP/$bin" && strip "$TMPDIR_STRIP/$bin" && echo "$n release-stripped bin_bytes=$(stat -c %s "$TMPDIR_STRIP/$bin")"
  rm -rf target
  )
}

TMPDIR_STRIP=$(mktemp -d)
trap 'rm -rf "$TMPDIR_STRIP"' EXIT
echo "# $(rustc --version), $(nproc) CPUs, $(date -u +%Y-%m-%d)" >> "$RAW"
for r in $(seq "$ROUNDS"); do
  measure build-axum a ahello | tee -a "$RAW"
  measure build-giong g ghello | tee -a "$RAW"
done
