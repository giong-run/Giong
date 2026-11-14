#!/usr/bin/env bash
# Verification beyond a plain `cargo test`.
#
#   scripts/check.sh test       release tests, repeated (default 5 rounds)
#   scripts/check.sh fallback   the same tests on the io_uring of older kernels:
#                               without the optional io_uring features
#                               (GIONG_IO_URING_OFF=all, as on 6.1 to 6.11), then
#                               also on the setup tiers of kernels older than 6.1
#                               (COOP_TASKRUN, no flags), then on the epoll driver
#                               (GIONG_DRIVER=epoll)
#   scripts/check.sh epoll      the tests on the epoll driver only
#   scripts/check.sh seccomp    the tests with io_uring blocked as in Docker's
#                               default seccomp profile (scripts/no-io-uring.py),
#                               so that giong falls back to epoll by itself
#   scripts/check.sh tsan       all tests under ThreadSanitizer (needs nightly
#                               with the rust-src component)
#   scripts/check.sh loom       model-check the wake-up handshakes (src/notify.rs)
#                               with loom, then run the same race on this CPU
#   scripts/check.sh soak       connection churn for SOAK_SECS seconds (default 300),
#                               then checks for leaked descriptors, memory and
#                               driver state (tests/soak.rs)
#   scripts/check.sh all        everything above except seccomp and soak
#
# ROUNDS=n changes how often each test binary is repeated.
# GIONG_TEST_EXPECT_DRIVER=io_uring (as CI sets it) makes the tests fail where
# giong would not choose io_uring by itself, instead of testing epoll only.
set -euo pipefail
cd "$(dirname "$0")/.."
ROUNDS="${ROUNDS:-5}"

repeat_tests() {
    local label="$1"
    for round in $(seq 1 "$ROUNDS"); do
        echo "== $label: round $round/$ROUNDS"
        cargo test --release --quiet --workspace --all-features
    done
}

run_test() {
    repeat_tests "default ring"
}

run_fallback() {
    GIONG_IO_URING_OFF=all repeat_tests "io_uring without optional features"
    for tier in coop basic; do
        GIONG_IO_URING_OFF=all GIONG_RING_TIER="$tier" repeat_tests "ring tier '$tier'"
    done
    run_epoll
}

run_epoll() {
    GIONG_DRIVER=epoll repeat_tests "epoll driver"
}

run_seccomp() {
    # Built once first, so that each round only runs the tests.
    cargo test --release --quiet --workspace --all-features --no-run
    for round in $(seq 1 "$ROUNDS"); do
        echo "== io_uring blocked by seccomp: round $round/$ROUNDS"
        env -u GIONG_DRIVER GIONG_TEST_EXPECT_DRIVER=epoll python3 scripts/no-io-uring.py \
            cargo test --release --quiet --workspace --all-features
    done
}

run_tsan() {
    local target
    target="$(rustc -vV | sed -n 's/^host: //p')"
    # TSan queues an asynchronous signal until the receiving thread next calls
    # an intercepted libc function. giong's threads sit in raw io_uring_enter
    # and futex syscalls, so the SIGTERM handler would never run and the test
    # would hang. It runs in every non-sanitizer job instead.
    local skip=(--skip sigterm_triggers_a_graceful_shutdown --skip sigterm_still_works_after_worker_0_returned)
    # `--cfg giong_tsan` makes giong close sockets with close(2) instead of
    # IORING_OP_CLOSE, which TSan cannot see (see Driver::close_fd).
    for round in $(seq 1 "$ROUNDS"); do
        echo "== ThreadSanitizer: round $round/$ROUNDS"
        RUSTFLAGS="-Zsanitizer=thread --cfg giong_tsan" \
        TSAN_OPTIONS="halt_on_error=1" \
        CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/tsan" \
            cargo +nightly test -Zbuild-std --target "$target" --tests --quiet --workspace --all-features \
            -- "${skip[@]}"
    done
}

run_loom() {
    echo "== loom models"
    RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/loom" \
        cargo test --release --quiet --lib notify
    echo "== lost wake-up race on real hardware"
    cargo test --release --quiet --test wake -- --ignored
}

run_soak() {
    echo "== soak for ${SOAK_SECS:-300} s"
    GIONG_SOAK_SECS="${SOAK_SECS:-300}" cargo test --release --quiet --test soak -- --ignored --nocapture
}

case "${1:-all}" in
    test) run_test ;;
    fallback) run_fallback ;;
    epoll) run_epoll ;;
    seccomp) run_seccomp ;;
    tsan) run_tsan ;;
    loom) run_loom ;;
    soak) run_soak ;;
    all) run_test; run_fallback; run_tsan; run_loom ;;
    *) echo "usage: $0 [test|fallback|epoll|seccomp|tsan|loom|soak|all]" >&2; exit 2 ;;
esac
echo "ok"
