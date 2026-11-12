#!/usr/bin/env bash
# Runs the single-threaded test suites under valgrind memcheck, one test per
# process, and fails if any test fails or valgrind reports an error or a
# definite leak.
#
#   scripts/valgrind.sh [suite ...]        default: executor time net cancel fs udp unix
#
# Not covered, because valgrind (3.22) treats io_uring_enter as non-blocking
# and so stalls programs with more than one thread waiting in it:
#   * tests/multi.rs and tests/spin.rs
#   * net::interoperates_with_blocking_std_clients, and
#     works_with_the_standard_library_on_the_other_side in tests/udp.rs and
#     tests/unix.rs: a std socket on its own thread is the other side
#   * udp::every_worker_can_bind_the_same_port, which runs three workers
#   * the tests in tests/dns.rs that ask the fake DNS server, which answers
#     from its own thread (they time out under valgrind); the unit tests in
#     src/net/dns.rs cover the parsing and the query ids
#
# Note that valgrind cannot see the kernel writing into memory, so a clean run
# does not by itself prove cancellation safety; tests/cancel.rs covers that.
set -uo pipefail
cd "$(dirname "$0")/.."

command -v valgrind >/dev/null || { echo "valgrind is not installed" >&2; exit 2; }

SUITES=("$@")
[ ${#SUITES[@]} -eq 0 ] && SUITES=(executor time net cancel fs udp unix)
SKIP="interoperates_with_blocking_std_clients"
SKIP+=" works_with_the_standard_library_on_the_other_side"
SKIP+=" every_worker_can_bind_the_same_port"
TIMEOUT=${TIMEOUT:-300}

total=0
failed=0
for suite in "${SUITES[@]}"; do
  bin=$(cargo test --no-run --test "$suite" 2>&1 | sed -n 's/.*Executable.*(\(.*\))/\1/p')
  if [ -z "$bin" ]; then
    echo "could not build test suite '$suite'" >&2
    exit 2
  fi
  for t in $("$bin" --list 2>/dev/null | sed -n 's/: test$//p'); do
    case " $SKIP " in *" $t "*) continue ;; esac
    log=$(mktemp)
    timeout -s KILL "$TIMEOUT" valgrind -q \
      --undef-value-errors=no --leak-check=full --show-leak-kinds=definite \
      "$bin" --exact "$t" --test-threads=1 >"$log" 2>&1
    rc=$?
    errors=$(grep -c '^==[0-9]*== [A-Z]' "$log")
    total=$((total + 1))
    if [ "$rc" -ne 0 ] || [ "$errors" -ne 0 ]; then
      failed=$((failed + 1))
      echo "FAIL $suite::$t (exit $rc, $errors valgrind error lines)"
      grep -E '^==[0-9]*==|panicked' "$log" | head -20
    else
      echo "ok   $suite::$t"
    fi
    rm -f "$log"
  done
done

echo "valgrind: $total tests, $failed failed"
[ "$failed" -eq 0 ]
