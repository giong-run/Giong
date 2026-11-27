#!/usr/bin/env bash
# A project's first build, with and without the build store's fill (what
# giong dev does before a dev build): eight projects from `giong new`, built
# one after another, the way someone starting new services on one machine
# would. Prints the raw numbers; results.txt holds a run.
#
#   bench/results/dev-first-build/run.sh <giong binary> <empty work dir>
#
# Each project: `cargo build` then `cargo test --no-run` (its first dev build
# and first test build), then an edit to src/main.rs and `cargo build` again.
# "cargo": plain cargo, nothing else. "giong": `giong cache fill` before the
# first build and `giong cache share` after it, as giong dev does around its
# builds. Needs network the first time (crates.io).
set -euo pipefail
GIONG=$(realpath "$1")
WORK=$(realpath "$2")
mkdir -p "$WORK"

projects=(
  "shop --db sqlite"
  "billing --db sqlite"
  "notes --db postgres"
  "orders --db postgres,redis"
  "feed --db none --tls"
  "search --template grpc"
  "jobs --template worker --db postgres"
  "mailer --template worker --db redis"
)

secs() { echo "$(date +%s.%N) - $1" | bc; }

echo "# $(date -u +%FT%TZ) $(uname -sr), $(nproc) CPUs, $(rustc --version), $(cargo --version), $("$GIONG" version)"
for mode in cargo giong; do
  dir="$WORK/$mode"
  mkdir -p "$dir"
  export GIONG_STORE_DIR="$dir/store"
  for p in "${projects[@]}"; do
    read -r name rest <<<"$p"
    (cd "$dir" && "$GIONG" new "$name" $rest --no-git --no-ai >/dev/null)
    cd "$dir/$name"
    cargo generate-lockfile -q
    fill=0
    if [ "$mode" = giong ]; then
      start=$(date +%s.%N)
      units=$("$GIONG" cache fill --json | grep -o '"filled_units":[0-9]*' | cut -d: -f2)
      fill=$(secs "$start")
    fi
    start=$(date +%s.%N)
    compiled=$(cargo build --message-format=json 2>/dev/null | grep -c '"reason":"compiler-artifact".*"fresh":false' || true)
    build=$(secs "$start")
    start=$(date +%s.%N)
    compiled_tests=$(cargo test --no-run --message-format=json 2>/dev/null | grep -c '"reason":"compiler-artifact".*"fresh":false' || true)
    tests=$(secs "$start")
    if [ "$mode" = giong ]; then "$GIONG" cache share >/dev/null; fi
    echo "// edited" >> src/main.rs
    start=$(date +%s.%N)
    cargo build -q
    edit=$(secs "$start")
    cargo test -q >/dev/null 2>&1 || { echo "$mode $name: cargo test FAILED"; exit 1; }
    echo "$mode $name: fill ${fill} s (${units:-0} units), first build ${build} s ($compiled compiled), first test build ${tests} s ($compiled_tests compiled), edit rebuild ${edit} s"
  done
  dirs=$(for p in "${projects[@]}"; do read -r name _ <<<"$p"; echo "$dir/$name/target"; done)
  echo "$mode disk: $(du -scb $dirs | tail -1 | cut -f1) bytes in ${#projects[@]} target dirs"
done
