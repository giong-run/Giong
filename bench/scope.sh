#!/usr/bin/env bash
# What starting and joining children costs: bench/scope (see its src/main.rs).
#
#   bench/scope.sh
#
# For 1, 10 and 1,000 children per round: plain `giong::spawn` and
# `JoinHandle`s, a `TaskGroup`, and a `Scope`. Wall time: the median of
# ROUNDS runs (default 7). Instructions: callgrind, counted for R and 2R
# rounds and subtracted, so start-up cancels out, divided by the children.
# Results: bench/results/scope/
set -euo pipefail
cd "$(dirname "$0")"
ROUNDS=${ROUNDS:-7}
OUT=results/scope
mkdir -p "$OUT"
(cd scope && cargo build --release -q)
BIN=scope/target/release/bench-scope

median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

instr() { # mode k rounds
  local f
  f=$(mktemp)
  valgrind --tool=callgrind --callgrind-out-file="$f" "$BIN" "$1" "$2" "$3" >/dev/null 2>&1
  callgrind_annotate "$f" 2>/dev/null | awk '/PROGRAM TOTALS/ {gsub(",","",$1); print $1}'
  rm -f "$f"
}

{
  echo "# bench/scope.sh, $(date -u +%Y-%m-%dT%H:%MZ), $(uname -r), $(nproc) CPUs"
  echo "# ns per child (median of $ROUNDS runs), instructions per child (callgrind, R vs 2R rounds)"
  printf '%-10s %-8s %-8s %-8s %s\n' driver mode children ns instr
  for driver in io_uring epoll; do
    for k in 1 10 1000; do
      r=$((200000 / k)); [ "$r" -lt 50 ] && r=50
      cr=$((r / 10)); [ "$cr" -lt 5 ] && cr=5
      for mode in spawn group scope; do
        ns=$(for _ in $(seq "$ROUNDS"); do GIONG_DRIVER=$driver "$BIN" "$mode" "$k" "$r"; done | median)
        a=$(GIONG_DRIVER=$driver instr "$mode" "$k" "$cr")
        b=$(GIONG_DRIVER=$driver instr "$mode" "$k" $((2 * cr)))
        printf '%-10s %-8s %-8s %-8s %s\n' "$driver" "$mode" "$k" "$ns" $(((b - a) / (cr * k)))
      done
    done
  done
} | tee "$OUT/summary.txt"
