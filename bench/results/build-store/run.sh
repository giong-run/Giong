#!/usr/bin/env bash
# The build store (giong cache), measured: disk used by the builds of several
# projects from `giong new`, before and after `giong cache share`, and whether
# cargo still sees every build as fresh afterwards.
#
#   bench/results/build-store/run.sh <giong binary> <empty work dir>
#
# Needs network the first time (crates.io). Prints the raw numbers; results.txt
# holds a run.
set -euo pipefail
GIONG=$(realpath "$1")
WORK=$(realpath "$2")
mkdir -p "$WORK"
cd "$WORK"
export GIONG_STORE_DIR="$WORK/store"

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

echo "# $(date -u +%FT%TZ) $(uname -sr), $(nproc) CPUs, $(rustc --version), $(cargo --version), $("$GIONG" version)"
for p in "${projects[@]}"; do
  read -r name rest <<<"$p"
  [ -d "$name" ] || "$GIONG" new "$name" $rest --no-git --no-ai >/dev/null
done

dirs() { for p in "${projects[@]}"; do read -r name _ <<<"$p"; echo "$name/target"; done; }

for p in "${projects[@]}"; do
  read -r name _ <<<"$p"
  start=$(date +%s.%N)
  (cd "$name" && cargo build -q && cargo test -q --no-run 2>/dev/null)
  echo "build+test-build $name: $(echo "$(date +%s.%N) - $start" | bc) s, $(du -sb "$name/target" | cut -f1) bytes"
done

# du counts a file with several links once across all its arguments.
before=$(du -scb $(dirs) | tail -1 | cut -f1)
echo "before: $before bytes in ${#projects[@]} target dirs"

start=$(date +%s.%N)
"$GIONG" cache share "$WORK"
echo "share took $(echo "$(date +%s.%N) - $start" | bc) s"

after=$(du -scb $(dirs) | tail -1 | cut -f1)
echo "after: $after bytes ($(echo "scale=1; 100 * ($before - $after) / $before" | bc)% less)"

# Every unit fresh: no compiler-artifact with "fresh": false.
for p in "${projects[@]}"; do
  read -r name _ <<<"$p"
  rebuilt=$(cd "$name" && { cargo build --message-format=json; cargo test --no-run --message-format=json; } 2>/dev/null \
    | grep -c '"reason":"compiler-artifact".*"fresh":false' || true)
  echo "fresh check $name: $rebuilt units rebuilt"
done

"$GIONG" cache
