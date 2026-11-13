# Contributing to giong

Thanks for taking an interest. This file explains how to build and test giong, what a
change has to satisfy before it is merged, and how to write commits.

## Your first contribution

New here? Welcome. A good way in:

1. Pick an issue labelled
   [`good first issue`](https://github.com/giong-run/Giong/labels/good%20first%20issue)
   or [`help wanted`](https://github.com/giong-run/Giong/labels/help%20wanted), or fix a
   bug you found yourself.
2. Comment on the issue to say you are working on it, so nobody duplicates the work. A
   maintainer will assign it to you. If you stop, say so; that is fine.
3. Set up as described below, reproduce the problem with a test, then fix it.
4. Open a pull request early, as a draft if it is not finished. Questions in a draft are
   welcome and are the fastest way to get help.

Pull requests from forks are the normal route. CI on a first-time contributor's pull request
starts once a maintainer approves the run, so do not worry if the checks wait at first.

Labels you will see: `bug`, `enhancement`, `documentation`, `good first issue`,
`help wanted`, `confirmed` (a maintainer reproduced it) and the `area: …` labels, which
name the part of the code involved (`driver`, `fs`, `net`, `http`, `tls`).

## Before you start

- For anything larger than a small fix, **[open an issue](https://github.com/giong-run/Giong/issues) first** and describe
  what you want to change. It avoids duplicated work and patches that do not fit the design.
- Check [ROADMAP.md](ROADMAP.md), including its non-goals.
- Bugs: include the kernel version (`uname -r`), the Rust version, and the smallest
  program that shows the problem.

## Setting up

You need **Linux with kernel 6.0 or newer** and a stable Rust toolchain. giong does not
build on Windows or macOS; use WSL2 with a 6.x kernel, a virtual machine, or a container
started with `--security-opt seccomp=unconfined` (Docker's default profile blocks io_uring).

```bash
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo run --release --example http
```

The repository is a Cargo workspace of three crates: `giong` (the runtime, at the root),
`giong-http` and `giong-tls`. The servers under `bench/` are separate workspaces.

## Checks every change must pass

Continuous integration runs these, and so should you:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
scripts/check.sh test        # the whole suite in release mode, five times over
scripts/check.sh fallback    # the same on the io_uring setups of kernels older than 6.1
```

If you touched `unsafe` code, buffer handling, the driver or anything that crosses
threads, also run:

```bash
scripts/check.sh tsan        # ThreadSanitizer; needs nightly with the rust-src component
scripts/check.sh loom        # loom models of the cross-thread wake-ups (src/notify.rs)
scripts/check.sh soak        # 5 minutes of churn, then checks for leaks (SOAK_SECS=…)
scripts/kernels.sh           # the suite on real 6.0 and 6.1 kernels in QEMU; needs
                             # qemu-system-x86, busybox-static, cpio (slow without KVM)
scripts/valgrind.sh          # needs valgrind; takes a few minutes
```

A change to code that reads bytes from the network (a parser, a decoder, a connection
state machine) should be fuzzed for a few minutes (needs nightly and
`cargo install cargo-fuzz`). Each crate that has such code has a `fuzz/` directory:

```bash
cd giong-http
cargo +nightly fuzz run head           # the server's request-head parser
cargo +nightly fuzz run chunked        # the chunked body decoder
cargo +nightly fuzz run connection     # a whole connection, over an in-memory transport
cargo +nightly fuzz run response_head  # the client's response-head parser
cargo +nightly fuzz run url            # URLs and redirect locations
cargo +nightly fuzz run inflate        # gzip and deflate bodies
cd ../giong-postgres && cargo +nightly fuzz run messages   # also: scram, url
cd ../giong-mysql && cargo +nightly fuzz run rows          # also: payload, framing, url
cd ../giong-redis && cargo +nightly fuzz run replies       # also: url
cd ../giong-web && cargo +nightly fuzz run request         # also: routes
```

Starting inputs are in each `fuzz/seeds/`; what the targets check is in each crate's
`src/fuzzing.rs` (in `giong-http`, `src/invariants.rs`, `src/client/mod.rs` and
`fuzz/fuzz_targets/connection.rs`). The randomised tests (`src/fuzz_tests.rs`) run the
same checks on every `cargo test`. CI fuzzes every night and on pull requests that touch
these crates. If a target finds a crash, add the input as a regression test.

A change to how a worker is woken from another thread (`src/notify.rs`, the injector,
`TaskWaker`) needs its loom model updated in the same pull request.

## Rules for the code

### Tests

- A bug fix comes with a test that fails without the fix.
- A new feature comes with tests for its normal path **and** for what happens when it is
  interrupted (see cancellation below).
- Tests must not depend on tight timing. Use generous upper bounds; CI machines are slow.

### `unsafe`

giong talks to the kernel, so some `unsafe` is unavoidable. Keep it small and justified.

- Every `unsafe` block has a `// SAFETY:` comment stating why it is sound.
- Prefer a safe design over a faster unsafe one unless a benchmark shows the difference
  matters.
- New `unsafe` code needs a test that would catch its misuse, and a clean valgrind run.

### Cancellation safety

This is the rule that is easiest to break. io_uring keeps using the memory an operation
points at until the kernel reports completion, whatever happens to the Rust future.

- Any new operation must keep its buffers alive until its final completion arrives, even
  if its future is dropped first. Use `Op`, which already does this, rather than
  submitting entries by hand.
- State in the documentation of every new `async fn` whether it is cancel safe, meaning
  whether dropping it can lose data.
- Add a test that drops the operation while it is in flight and then checks
  `giong::stats()` for leaked operations and buffers. `tests/cancel.rs` has examples.

### Performance claims

- A change described as faster needs numbers: before and after, from `bench/run.sh` or a
  benchmark you add, with the machine and kernel stated.
- Report the median of several runs and the spread, not the best run.
- Say what you did not measure.

### Dependencies and API

- New dependencies need a reason. The runtime depends on three crates; `giong-tls` adds
  `rustls`, and `giong-http` adds nothing.
- Public items need documentation. Breaking changes to the public API need an issue first
  and an entry in the changelog.

## Commits

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <summary in the imperative, lower case, no full stop>

<why the change is needed and what it does, wrapped at 72 columns>

<footers, e.g. "Fixes #12" or "BREAKING CHANGE: ...">
```

**Types:** `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `bench`, `ci`, `chore`.

**Scopes:** `runtime`, `executor`, `driver`, `net`, `fs`, `time`, `sync`, `buf`,
`http`, `tls`, `examples`, `bench`. Leave the scope out when a change spans several.

Examples:

```
fix(driver): cancel a paused multishot receive when its stream is dropped
feat(net): add UdpSocket
perf(time): skip the timer wheel when no timer is armed
docs: explain how to run under WSL2
```

- One logical change per commit. Formatting-only changes go in their own commit.
- A breaking change is marked with `!` after the type or scope (`feat(net)!: ...`) and a
  `BREAKING CHANGE:` footer.
- The body explains *why*. The diff already shows *what*.

## Pull requests

1. Branch from `main`. Name the branch `<type>/<short-description>`, for example
   `fix/recv-pause-on-drop`.
2. Keep the pull request focused; several small ones are reviewed faster than one large one.
3. Add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for anything a user
   would notice.
4. Fill in the pull request template. Say how you tested the change.
5. All checks must be green. A maintainer reviews and merges; pull requests are
   squash-merged, so the pull request title must itself be a valid commit summary.

## Releases

Releases are automated; maintainers start one from the **Prepare release** workflow and
merge the pull request it opens. Every crate in the workspace shares one version and is
published together, to crates.io and as a GitHub Release whose notes come from
`CHANGELOG.md` and thank everyone who contributed. The whole process, including adding a
new crate, is in [docs/RELEASING.md](docs/RELEASING.md).

What that asks of a contributor is in the list above: a `CHANGELOG.md` line under
`Unreleased` and a pull request title in the commit convention, since both feed the
release notes.

## Security

Please do not open a public issue for a vulnerability such as a memory-safety bug that
can be triggered remotely. Report it privately at
<https://github.com/giong-run/Giong/security/advisories/new> so it can be fixed before it is disclosed.

## Conduct

Everyone taking part is expected to follow the [code of conduct](CODE_OF_CONDUCT.md). In
short: be respectful, assume good faith, and criticise code, not people. Report problems to
conduct@giong.run.

## Licensing of contributions

giong is dual-licensed under MIT and Apache-2.0. Unless you state otherwise, any
contribution you submit is licensed under the same terms, without additional conditions.
