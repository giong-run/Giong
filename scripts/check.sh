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
#   scripts/check.sh fallback <tier>
#                               one of those four only: no-features, coop, basic
#                               or epoll (CI runs them side by side)
#   scripts/check.sh epoll      the tests on the epoll driver only
#   scripts/check.sh seccomp    the tests with io_uring blocked as in Docker's
#                               default seccomp profile (scripts/no-io-uring.py),
#                               so that giong falls back to epoll by itself
#   scripts/check.sh tsan       all tests under ThreadSanitizer (needs nightly
#                               with the rust-src component)
#   scripts/check.sh loom       model-check the wake-up handshakes (src/notify.rs)
#                               with loom, then run the same race on this CPU
#   scripts/check.sh kani       prove the unsafe core with the Kani model checker
#                               (`cargo install --locked kani-verifier --version
#                               0.68.0 && cargo kani setup`): the slab, the
#                               receive buffer pool, KernelBox and the lifetime
#                               of an io_uring operation (docs/kani.md)
#   scripts/check.sh soak       connection churn for SOAK_SECS seconds (default 300),
#                               then checks for leaked descriptors, memory and
#                               driver state (tests/soak.rs); then the same over
#                               TLS, with kernel TLS where the tls module is
#                               loaded (giong-tls/tests/soak.rs). `soak runtime`
#                               or `soak tls` runs one of the two
#   scripts/check.sh miri       the unit tests and the tests Miri can run, on the
#                               epoll driver, under Miri, which checks the unsafe
#                               code against Rust's memory and aliasing rules
#                               (needs nightly with the miri component); also
#                               giong-http's allocator and parsers and the
#                               database clients' read buffers
#   scripts/check.sh sim        the deterministic simulation (sim/README.md): its
#                               scenarios, months of simulated uptime, and this
#                               crate's tests on it without faults and then with
#                               loopback faults for SIM_SEEDS seeds (default 3)
#   scripts/check.sh sim-long   years of simulated uptime under hostile faults:
#                               the hostile soak (sim/tests/soak.rs) for
#                               SIM_LONG_SEEDS seeds (default 1200) from seed
#                               SIM_LONG_FROM (default 0), SIM_LONG_DAYS
#                               (default 3650) split over every seed, one
#                               process per CPU; a failure prints its seed
#   scripts/check.sh all        everything above except seccomp, soak, miri, kani, sim and sim-long
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
    case "$1" in
        no-features) GIONG_IO_URING_OFF=all repeat_tests "io_uring without optional features" ;;
        coop | basic) GIONG_IO_URING_OFF=all GIONG_RING_TIER="$1" repeat_tests "ring tier '$1'" ;;
        epoll) run_epoll ;;
        all)
            for tier in no-features coop basic epoll; do
                run_fallback "$tier"
            done
            ;;
        *) echo "usage: $0 fallback [no-features|coop|basic|epoll]" >&2; exit 2 ;;
    esac
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
    # would hang. It runs in every non-sanitizer job instead. The handler
    # itself still runs under TSan in
    # sigterm_delivered_to_a_thread_outside_the_runtime_shuts_it_down, whose
    # signal goes to a thread sleeping in nanosleep, which TSan intercepts.
    local skip=(--skip sigterm_triggers_a_graceful_shutdown --skip sigterm_still_works_after_worker_0_returned
        --skip a_second_signal_ends_a_process_whose_shutdown_hangs)
    # `--cfg giong_tsan` makes giong close sockets with close(2) instead of
    # IORING_OP_CLOSE, which TSan cannot see (see Driver::close_fd).
    # allocator_may_return_null: TSan's malloc aborts the process on a request
    # over 1 TiB instead of failing it as the system allocator does, and the
    # fs test of #296 asks for 2^60 bytes through try_reserve.
    for round in $(seq 1 "$ROUNDS"); do
        echo "== ThreadSanitizer: round $round/$ROUNDS"
        RUSTFLAGS="-Zsanitizer=thread --cfg giong_tsan" \
        TSAN_OPTIONS="halt_on_error=1 allocator_may_return_null=1" \
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
    # One after the other, each for SOAK_SECS: both use every core.
    local which="$1"
    if [ "$which" = all ] || [ "$which" = runtime ]; then
        echo "== soak for ${SOAK_SECS:-300} s"
        GIONG_SOAK_SECS="${SOAK_SECS:-300}" cargo test --release --quiet -p giong --test soak -- --ignored --nocapture
    fi
    if [ "$which" = all ] || [ "$which" = tls ]; then
        echo "== TLS soak for ${SOAK_SECS:-300} s"
        GIONG_SOAK_SECS="${SOAK_SECS:-300}" cargo test --release --quiet -p giong-tls --all-features --test soak -- --ignored --nocapture
    fi
}

run_miri() {
    # Miri cannot run io_uring, so the epoll driver; it reads and writes real
    # files (isolation off); the blocking threads may outlive a test (leaks
    # ignored); and it would otherwise make reads short at random, which the
    # tests check exactly.
    export GIONG_DRIVER=epoll
    export MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks -Zmiri-no-short-fd-operations"
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/miri"
    echo "== Miri: unit tests"
    cargo +nightly miri test --quiet --lib
    # The test files whose tests Miri can mostly run; the rest of their tests
    # are marked `#[cfg_attr(miri, ignore = "why")]`. Miri has no sendmsg,
    # SO_REUSEPORT or Unix sockets, so the network tests stay out, except for
    # the file test in cancel.rs.
    local files=(fs executor channels observe time multi)
    for file in "${files[@]}"; do
        echo "== Miri: tests/$file.rs"
        cargo +nightly miri test --quiet --test "$file"
    done
    echo "== Miri: tests/cancel.rs (files)"
    cargo +nightly miri test --quiet --test cancel -- --exact a_dropped_open_closes_the_descriptor_it_produced
    # The unsafe code of the other crates that Miri can reach: giong-http's
    # pooled allocator for connection futures (heap.rs) and the parsers on the
    # request path, and the database clients' read buffers, which lend their
    # spare capacity to the kernel (IoBufMut). Only the unit tests of those
    # modules: the others open sockets or call ring's assembly, which Miri
    # cannot run (an unsupported call aborts the whole test binary).
    echo "== Miri: giong-http (allocator, parsers)"
    cargo +nightly miri test --quiet -p giong-http --lib -- \
        heap:: chunked:: parse:: trailers:: h2::hpack h2::frame h3::qpack h3::frame client::decode
    echo "== Miri: giong-postgres, giong-mysql (read buffers)"
    cargo +nightly miri test --quiet -p giong-postgres --lib -- connect::
    cargo +nightly miri test --quiet -p giong-mysql --lib -- connect::
}

run_sim() {
    (
        cd sim
        echo "== sim: fmt and clippy"
        cargo fmt --check
        cargo clippy --quiet --release --all-targets -- -D warnings
        echo "== sim: scenarios, long runs, this crate's tests without faults"
        local log
        log="$(mktemp)"
        if ! cargo test --release -- --nocapture >"$log" 2>&1; then
            tail -n 200 "$log"
            exit 1
        fi
        # How much simulated time this run covered.
        grep -E '^soak:' "$log"
        rm -f "$log"
        for seed in $(seq 1 "${SIM_SEEDS:-3}"); do
            echo "== sim: this crate's tests with loopback faults, seed $seed"
            GIONG_SIM_FAULTS=loopback GIONG_SIM_SEED="$seed" cargo test --release --quiet --test suite
        done
    )
}

run_sim_long() {
    (
        cd sim
        local seeds="${SIM_LONG_SEEDS:-1200}" from="${SIM_LONG_FROM:-0}"
        local days="${SIM_LONG_DAYS:-3650}" jobs bin dir
        jobs="$(nproc)"
        bin="$(cargo test --release --test soak --no-run --message-format=json |
            jq -r 'select(.reason == "compiler-artifact" and .target.name == "soak") | .executable')"
        dir="$(mktemp -d)"
        echo "== sim-long: seeds $from to $((from + seeds - 1)), $((days / 7)) simulated days each, $jobs processes"
        local per=$(((seeds + jobs - 1) / jobs)) i pids=() failed=0
        for ((i = 0; i < jobs; i++)); do
            local start=$((from + i * per)) n=$per
            if ((start + n > from + seeds)); then n=$((from + seeds - start)); fi
            ((n > 0)) || continue
            GIONG_SIM_SOAK_DAYS="$days" GIONG_SIM_SEEDS="$n" GIONG_SIM_SEED_FROM="$start" \
                "$bin" --exact days_of_uptime_under_hostile_faults --nocapture >"$dir/$i.log" 2>&1 &
            pids+=("$!:$i")
        done
        for p in "${pids[@]}"; do
            if ! wait "${p%%:*}"; then
                failed=1
                echo "== sim-long: process ${p##*:} failed:"
                tail -n 200 "$dir/${p##*:}.log"
            fi
        done
        grep -h -E '^soak:' "$dir"/*.log || true
        rm -rf "$dir"
        exit "$failed"
    )
}

run_kani() {
    # Every `#[kani::proof]` in the core crate; each one's module docs say
    # what it proves and within which bounds. Kani compiles with its own
    # nightly and sets `cfg(kani)`, which the proofs live under.
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/kani"
    # The operation lifecycle proof alone: next to another proof it can
    # exhaust a 16 GB machine. The others in parallel with KANI_JOBS.
    cargo kani --lib -p giong --output-format terse --harness driver::uring::proofs
    cargo kani --lib -p giong --output-format terse ${KANI_JOBS:+-j "$KANI_JOBS"} \
        --harness slab::proofs --harness kbox::proofs --harness buf::proofs
}

case "${1:-all}" in
    test) run_test ;;
    fallback) run_fallback "${2:-all}" ;;
    epoll) run_epoll ;;
    seccomp) run_seccomp ;;
    tsan) run_tsan ;;
    loom) run_loom ;;
    soak)
        case "${2:-all}" in
            all | runtime | tls) run_soak "${2:-all}" ;;
            *) echo "usage: $0 soak [runtime|tls]" >&2; exit 2 ;;
        esac
        ;;
    miri) run_miri ;;
    kani) run_kani ;;
    sim) run_sim ;;
    sim-long) run_sim_long ;;
    all) run_test; run_fallback all; run_tsan; run_loom ;;
    *) echo "usage: $0 [test|fallback|epoll|seccomp|tsan|loom|soak [runtime|tls]|miri|kani|sim|sim-long|all]" >&2; exit 2 ;;
esac
echo "ok"
