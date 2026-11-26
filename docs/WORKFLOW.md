# The development workflow, versioned

How a change gets from a branch into `main` has a version number, like the code, so that
each change to the process is measured against the one before and kept only if it is
faster without lowering the bar. This page holds the current version, what changed from
the last one, and the numbers. [DEVELOPMENT.md](DEVELOPMENT.md) holds the day-to-day
rules that follow from it; [AGENTS.md](../AGENTS.md) the rules for the code.

**Current: Workflow v2** (2026-11-26).

**Numbering.** A small change to the current version (a setting, a tier, a rule) is a
minor version: v2.1, v2.2. A change to how work flows (what runs where, how pull requests
are formed or merged) is a major one: v3. Each version gets a section below with what
changed, why, and its numbers, measured with `scripts/workflow-metrics.py` over a real
window of merged pull requests, the raw data committed under `docs/workflow/data/`.

**What never changes between versions.** `main` is protected and changes only through a
pull request in the merge queue. Before anything with code in it lands, every job runs on
`main` with the change merged: lint, MSRV 1.80, the tests on io_uring and on epoll with
every repetition, ThreadSanitizer, Miri, Kani, the kernels, macOS and Windows. A version
may move a job between the pull request and the queue; it never removes one from the
queue. No merge on red, no test skipped or rerun to green (the one exception is below).

## Workflow v2

### What changed from v1, and why

Measured over the 59 pull requests of 1.5.0 to 1.5.1 ([data](workflow/data/v1/README.md)):
a pull request took a median **201 minutes** from ready for review to merged (p90 449),
and **162** of them were spent in the merge queue, waiting behind others: the queue built
two groups at a time, 38 minutes each, with 8 to 12 pull requests in line for almost half
the window. 15 ejections, 11 of them flaky tests the pull request did not touch, each
cost about four more hours and restarted the groups behind it. The pull requests' own CI
took 30 minutes and was not the problem, but it used 347 runner-hours running every job
once before the queue ran them all again (557 hours), and the runners it took are what
kept the queue at two groups.

| | v1 | v2 |
|---|---|---|
| CI on a pull request (draft or ready) | ready: every job once, macOS and Windows; draft: a short set | the fast set: lint, MSRV, default features, the build, one round of the tests on io_uring and on epoll |
| macOS and Windows on a pull request | every ready pull request that changed code | only when it changes platform code (below); otherwise in the queue |
| Merge queue | every job, every repetition | the same, unchanged, for any group with code; a group of only docs runs lint and MSRV |
| Merge queue settings | 2 groups built at a time | 4 at a time (the runners the pull requests no longer take) |
| Required check on `main` | `lint`, `msrv`, `test`, `fallback`, `tsan`, `all-checks` | `CI gate` only, which reruns failures outside the code and known flakes in place |
| Pull requests | one fix, one pull request | related small fixes from one line of work share a pull request |
| A flaky test in the queue | fixed in a pull request of its own, through the queue, while it keeps ejecting others | listed in `.github/known-flakes.txt` within the hour (a docs-only pull request, minutes to land), then fixed |

### CI tiers

`scripts/ci-scope.py` (the `plan` job) decides; `scripts/test_ci_scope.py` tests it.

| The run | What runs |
|---|---|
| Pull request, draft or ready, that changes no file a build or test reads (`site/`, `changelog.d/`, `bench/results/`, `docs/workflow/`, prose, `.github/known-flakes.txt`) | `lint` (fmt, clippy, docs, `release.py check`) and `msrv` |
| Any other pull request | the above, `default-features`, `build`, `test` (one round, io_uring), `fallback (epoll)` (one round); not Miri, Kani, TSan, the kernels, the servers, instr, semver, h2spec, h3spec, musl, Docker, gVisor |
| ...that is ready for review and touches macOS or Windows code | also the macOS or Windows workflow, as in v1 |
| Merge queue, group with code; a dispatch of CI; a push to `main` no queue run tested | every job with every repetition (five rounds, four fallback tiers, three TSan runs), macOS and Windows, as in v1 |
| Merge queue, group with only docs | `lint` and `msrv` |

**Platform code** (`ci-scope.py --platform`): the platform layer (`src/sys/`, the kqueue
or IOCP driver), a Rust file whose source names the platform in a `cfg` (`target_os =
"macos"`, `windows`, `kqueue`, `std::os::windows` and the like), any `Cargo.toml` or
`Cargo.lock`, and the platform's workflow. Anything else reaches macOS and Windows in the
queue.

**When the fast set is not enough**: a change to the unsafe core, a driver, the scheduler
or cross-thread wake-ups, or one the queue ejected for a job the fast set does not run.
Run the full set on the branch before enqueueing: Actions, CI, Run workflow, on the
branch (or `actions_run_trigger` / `gh workflow run ci.yml --ref <branch>`). It is the
queue's run, macOS and Windows included, and the CI gate reports it on the commit.
`scripts/preflight.sh` already runs what a local machine can (both drivers, TSan for
unsafe and cross-thread code, loom, Kani for the unsafe core, `instr.py check`); run it
before marking ready, as in v1.

**The cost of moving jobs to the queue**: a failure that only a heavy job finds
(Miri, a kernel, macOS) now ejects the pull request from the queue instead of showing on
the pull request. That is the trade v2 makes, and its measurement says whether it pays:
v1 had 15 ejections in 59 merges, almost all flakes the pull request's own run had
passed anyway.

### Flaky tests

A test that fails in the queue on code the pull request did not touch is a bug, and the
one exception to "never rerun" applies to it (project rule: a single known flake with a
fix in progress):

1. **Within the hour**, open an issue with the failing job's link and the test's name.
2. Open a pull request that adds one line to `.github/known-flakes.txt`: the issue number
   and the test's name as libtest prints it (a regular expression). It is a docs-only
   change: lint and MSRV on the pull request, the same in the queue, minutes to land.
   From then on the CI gate reruns the failed jobs once when that test is the only
   failure, and names the issue in the run's summary.
3. Re-enqueue the ejected pull request right away.
4. Fix the test in its own pull request (or in the next batch); the fix removes the line.
   An entry whose issue is closed no longer counts.

The test is never skipped, `#[ignore]`d or deleted to get green. A failure of a listed
test together with any other failure fails the gate as before.

### Batching fixes

Related small fixes from one line of work (the same crate, the same debug round) go into
one pull request: each fix its own commit with its own test, the description listing
`Fixes #N` for each. At most about five per pull request, so that one red job does not
hold back a large batch, and never mixed with the unsafe core, a driver or a public API
change, which keep a pull request of their own. Fewer pull requests means fewer queue
runs; one queue run of five fixes costs the same 38 minutes as one of a single fix.

### Settings only a maintainer can change

Repository Settings, Rules or Branches, `main`:

1. **Required status checks**: remove `lint`, `msrv`, `test`, `fallback`, `tsan` and
   `all-checks`; require only **`CI gate`** (a commit status, set by
   `.github/workflows/ci-gate.yml`). The gate passes exactly when the whole CI run passed
   (`all-checks` included, macOS and Windows in the queue), and reruns in place what
   failed for a reason outside the code. It also ends the trap where cancelling a macOS
   or Windows run on a pull request blocked its queue entry.
2. **Merge queue, Build concurrency**: **4** (was 2). Each group still runs every job.
3. **Merge queue, Merge limits**: minimum **1**, maximum **5**, wait time **0**: no pull
   request waits to fill a group, and when a later group passes, the ones before it
   merge together.
4. Keep **Only merge non-failing pull requests** on, the merge method squash, and the
   status check timeout at 120 minutes.

### How v2 is measured

After v2 has merged and the settings are in, take a window of at least 30 merged pull
requests with no release in it, and run:

    python3 scripts/workflow-metrics.py fetch --since <start> --until <end> --out docs/workflow/data/v2
    python3 scripts/workflow-metrics.py report docs/workflow/data/v2 --csv docs/workflow/data/v2/prs.csv

Compare with [v1](workflow/data/v1/README.md): ready to merged, time in the queue,
ejections and their causes, runner time on pull requests and in the queue. The result
goes below, losses included. A number v2 makes worse is either fixed in v2.1 or the
change behind it is reverted.

**Expected, not yet measured**: the fast set takes about a sixth of the runner time of a
v1 ready run (#218 measured a draft at 1,018 runner-seconds against 8,366); the queue
time should fall the most, from twice the building capacity and fewer ejections.

## Workflow v1 (2026-11-15 to 2026-11-26): the baseline

The process from the merge queue (#93) to the CI gate (#357), as DEVELOPMENT.md
described it before v2: one concern per pull request; a draft ran a short set and a pull
request ready for review every job once, macOS and Windows included; the merge queue ran
every job with every repetition, two groups at a time; the required checks were `lint`,
`msrv`, `test`, `fallback`, `tsan` and `all-checks`; a flaky test was fixed in a pull
request of its own. Measured over 1.5.0 to 1.5.1: [workflow/data/v1](workflow/data/v1/README.md).

## Changelog

- **v2** (2026-11-26): fast set on pull requests, the full set in the queue only;
  macOS and Windows on pull requests only for platform code; docs-only merge groups run
  lint and MSRV; `CI gate` the only required check; queue concurrency 2 to 4; batched
  fixes; known flakes listed within the hour.
- **v1** (2026-11-15): the merge queue, tiers for drafts and docs, the CI gate.
