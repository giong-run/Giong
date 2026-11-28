#!/usr/bin/env bash
# What a deadline costs a round trip: bench/deadline (see its src/main.rs).
#
#   bench/deadline.sh                 wall time and instruction counts
#   BASE=/path/to/bench-deadline bench/deadline.sh
#                                     also the plain mode of another build
#                                     (giong without giong::task, built with
#                                     --no-default-features), to compare the
#                                     path without deadlines
#
# Modes (bench/deadline/src/main.rs): plain, per-call (a deadline per round
# trip), whole (one deadline around the loop); whole-unlinked is whole on
# io_uring with GIONG_IO_URING_OFF=linked-deadlines, so the difference to
# whole is what linking the deadline to the reads costs.
# Wall time: ROUNDS runs (default 7) of N round trips (default 200000) per
# mode, on io_uring and on epoll; the median is reported.
# Instructions: callgrind, counted for N and 2N round trips and subtracted,
# so start-up and warm-up cancel out; two runs agree within about 0.1%.
# Results: bench/results/deadline/
set -euo pipefail
cd "$(dirname "$0")"
N=${N:-200000}
ROUNDS=${ROUNDS:-7}
CN=${CN:-20000}
OUT=results/deadline
mkdir -p "$OUT"
(cd deadline && cargo build --release -q)
BIN=deadline/target/release/bench-deadline

median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

instr() { # bin mode n
  local f
  f=$(mktemp)
  valgrind --tool=callgrind --callgrind-out-file="$f" "$1" "$2" "$3" >/dev/null 2>&1
  callgrind_annotate "$f" 2>/dev/null | awk '/PROGRAM TOTALS/ {gsub(",","",$1); print $1}'
  rm -f "$f"
}

{
  echo "# bench/deadline.sh, $(date -u +%Y-%m-%dT%H:%MZ), $(uname -r), $(nproc) CPUs"
  echo "# ns per round trip (median of $ROUNDS runs of $N), instructions per round trip (callgrind, ${CN} vs $((2 * CN)))"
  printf '%-10s %-10s %-10s %s\n' driver mode ns instr
  for driver in io_uring epoll; do
    modes="plain per-call whole"
    for mode in $modes; do
      ns=$(for _ in $(seq "$ROUNDS"); do GIONG_DRIVER=$driver "$BIN" "$mode" "$N"; done | median)
      a=$(GIONG_DRIVER=$driver instr "$BIN" "$mode" "$CN")
      b=$(GIONG_DRIVER=$driver instr "$BIN" "$mode" $((2 * CN)))
      printf '%-10s %-10s %-10s %s\n' "$driver" "$mode" "$ns" $(((b - a) / CN))
    done
    if [ "$driver" = io_uring ]; then
      # The same, with the deadline's timer only: what the kernel link costs.
      ns=$(for _ in $(seq "$ROUNDS"); do GIONG_IO_URING_OFF=linked-deadlines "$BIN" whole "$N"; done | median)
      a=$(GIONG_IO_URING_OFF=linked-deadlines instr "$BIN" whole "$CN")
      b=$(GIONG_IO_URING_OFF=linked-deadlines instr "$BIN" whole $((2 * CN)))
      printf '%-10s %-10s %-10s %s\n' "$driver" "whole-unlinked" "$ns" $(((b - a) / CN))
    fi
    if [ -n "${BASE:-}" ]; then
      ns=$(for _ in $(seq "$ROUNDS"); do GIONG_DRIVER=$driver "$BASE" plain "$N"; done | median)
      a=$(GIONG_DRIVER=$driver instr "$BASE" plain "$CN")
      b=$(GIONG_DRIVER=$driver instr "$BASE" plain $((2 * CN)))
      printf '%-10s %-10s %-10s %s\n' "$driver" "base-plain" "$ns" $(((b - a) / CN))
    fi
  done
} | tee "$OUT/summary.txt"
