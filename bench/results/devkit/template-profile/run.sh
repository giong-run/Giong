#!/usr/bin/env bash
# The dev profile `giong new` writes, against Cargo's default, on the project of
# `giong new shop --db postgres,redis --tls` (this checkout's crates by path):
# a clean debug build, the target directory's size, and the rebuild after a
# one-line change to src/users.rs. Rounds interleave the two, so the machine's
# drift hits both alike.
#   bench/results/devkit/template-profile/run.sh [rounds]   (default 3)
set -euo pipefail
ROUNDS="${1:-3}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../../.." && pwd)"
WORK="$(mktemp -d)"
cargo build -q --manifest-path "$ROOT/Cargo.toml" -p giong-cli
GIONG="$ROOT/target/debug/giong"
"$GIONG" new "$WORK/fast" --db postgres,redis --tls --giong-path "$ROOT" --no-git --no-ai >/dev/null
cp -r "$WORK/fast" "$WORK/default"
# Cargo's default: everything from the first [profile. table on goes.
python3 - "$WORK/default/Cargo.toml" <<'PY'
import sys, re
p = sys.argv[1]
s = open(p).read()
i = s.index("# Faster rebuilds")
open(p, "w").write(s[:i].rstrip() + "\n")
PY
# Resolve the same versions in both.
(cd "$WORK/fast" && cargo generate-lockfile -q) && cp "$WORK/fast/Cargo.lock" "$WORK/default/"
OUT="$HERE/raw.csv"
echo "round,profile,clean_build_s,target_mb,rebuild_s" > "$OUT"
for r in $(seq "$ROUNDS"); do
    for p in fast default; do
        d="$WORK/$p"
        rm -rf "$d/target"
        s=$(date +%s.%N); (cd "$d" && cargo build -q); e=$(date +%s.%N)
        clean=$(echo "$e - $s" | bc)
        mb=$(du -sm "$d/target" | cut -f1)
        echo "// edit $r" >> "$d/src/users.rs"
        s=$(date +%s.%N); (cd "$d" && cargo build -q); e=$(date +%s.%N)
        rebuild=$(echo "$e - $s" | bc)
        echo "$r,$p,$clean,$mb,$rebuild" | tee -a "$OUT"
    done
done
{
    echo "machine: $(nproc) vCPU, $(uname -r), $(rustc --version)"
    echo "giong: $(git -C "$ROOT" rev-parse --short HEAD)"
} > "$HERE/machine.txt"
rm -rf "$WORK"
