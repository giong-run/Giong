#!/usr/bin/env bash
# The macOS prototype's test run (branch only, with macos.yml).
#
# Every test binary of the workspace runs on its own, under a time limit.
# A binary that passes or fails in time is counted from its "test result"
# line. A binary that runs out of time is run again one test at a time,
# each under a shorter limit, so a single hung test is named instead of
# holding the whole job. Counts go to the job summary and annotations;
# every log is kept in logs/ for the artifact.
set -uo pipefail

os="${1:-macos}"
bin_limit="${BIN_LIMIT:-300}"
test_limit="${TEST_LIMIT:-60}"
mkdir -p logs

# Runs "$@" in its own process group and kills the group after $1 seconds.
# Exit code 124 means the limit was hit.
limited() {
  perl -e '
    my $t = shift;
    my $p = fork // die "fork: $!";
    if ($p == 0) { setpgrp(0, 0); exec @ARGV or die "exec: $!"; }
    $SIG{ALRM} = sub { kill "KILL", -$p; waitpid($p, 0); exit 124; };
    alarm $t;
    waitpid($p, 0);
    kill "KILL", -$p;
    exit($? & 127 ? 128 + ($? & 127) : $? >> 8);
  ' "$@"
}

cargo test --workspace --all-features --no-run --message-format=json 2>/dev/null \
  | jq -r 'select(.reason == "compiler-artifact" and .profile.test == true and .executable != null)
           | [.package_id, .target.name, .executable, .manifest_path] | @tsv' \
  > logs/binaries.tsv

status=0
total_p=0; total_f=0; total_i=0; total_h=0
{
  echo "| crate | binary | passed | failed | ignored | hung | result |"
  echo "|---|---|---|---|---|---|---|"
} >> "$GITHUB_STEP_SUMMARY"

while IFS=$'\t' read -r pkg name exe manifest; do
  crate=$(basename "$(dirname "$manifest")")
  dir=$(dirname "$manifest")
  log="logs/$crate-$name.log"
  export CARGO_MANIFEST_DIR="$dir"
  (cd "$dir" && limited "$bin_limit" "$exe" --test-threads=4) > "$log" 2>&1 < /dev/null
  code=$?
  p=0; f=0; i=0; h=0; failed_names=""; hung_names=""
  if [ "$code" = 124 ]; then
    # Out of time: one test at a time.
    (cd "$dir" && "$exe" --list --format terse 2>/dev/null) | sed -n 's/: test$//p' > "logs/$crate-$name.list"
    : > "logs/$crate-$name.each.log"
    while read -r t; do
      one=$(mktemp)
      (cd "$dir" && limited "$test_limit" "$exe" --exact "$t" --test-threads=1) > "$one" 2>&1 < /dev/null
      c=$?
      { echo "=== $t (exit $c)"; cat "$one"; } >> "logs/$crate-$name.each.log"
      if [ "$c" = 124 ]; then
        h=$((h + 1)); hung_names="$hung_names $t"
      elif grep -q '^test result: ok. 0 passed; 0 failed; 1 ignored' "$one"; then
        i=$((i + 1))
      elif [ "$c" = 0 ]; then
        p=$((p + 1))
      else
        f=$((f + 1)); failed_names="$failed_names $t"
      fi
      rm -f "$one"
    done < "logs/$crate-$name.list"
  else
    line=$(grep -E '^test result:' "$log" | tail -1)
    p=$(echo "$line" | sed -nE 's/.* ([0-9]+) passed.*/\1/p'); p=${p:-0}
    f=$(echo "$line" | sed -nE 's/.* ([0-9]+) failed.*/\1/p'); f=${f:-0}
    i=$(echo "$line" | sed -nE 's/.* ([0-9]+) ignored.*/\1/p'); i=${i:-0}
    failed_names=$(grep -E '^    [A-Za-z0-9_:]+$' "$log" | sed 's/^ *//' | sort -u | paste -sd' ' -)
    if [ "$code" != 0 ] && [ "$f" = 0 ]; then failed_names="(crashed, exit $code)"; fi
  fi
  total_p=$((total_p + p)); total_f=$((total_f + f)); total_i=$((total_i + i)); total_h=$((total_h + h))
  result=ok
  if [ "$f" != 0 ] || [ "$h" != 0 ] || { [ "$code" != 0 ] && [ "$code" != 124 ]; }; then
    result="FAILED"; status=1
  fi
  echo "| $crate | $name | $p | $f | $i | $h | $result |" >> "$GITHUB_STEP_SUMMARY"
  echo "::notice title=$os $crate/$name::passed $p, failed $f, ignored $i, hung $h"
  if [ -n "${failed_names// /}" ]; then
    echo "::warning title=$os $crate/$name failed::$(echo $failed_names | cut -c1-900)"
  fi
  if [ -n "${hung_names// /}" ]; then
    echo "::warning title=$os $crate/$name hung (>${test_limit}s)::$(echo $hung_names | cut -c1-900)"
  fi
done < logs/binaries.tsv

echo "| **all** | | **$total_p** | **$total_f** | **$total_i** | **$total_h** | |" >> "$GITHUB_STEP_SUMMARY"
echo "::notice title=$os total::passed $total_p, failed $total_f, ignored $total_i, hung $total_h"
exit $status
