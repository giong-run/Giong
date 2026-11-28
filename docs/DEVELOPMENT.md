# Developing giong with agents

Most of giong is written by AI coding agents, several at a time, each on a branch of its
own. [AGENTS.md](../AGENTS.md) holds the rules for the code; this page holds the rules for
the flow of work, so that many agents in parallel stay fast without lowering the bar.
Everything here applies to people too. Where it and [CONTRIBUTING.md](../CONTRIBUTING.md)
disagree, CONTRIBUTING.md wins: report the difference.

The process has a version: **Workflow v2.1**, described with its measurements in
[WORKFLOW.md](WORKFLOW.md). A change to the process goes there first (a new minor or
major version, measured against the last), then here.

The bar does not move: `main` changes only through a pull request that passes every CI job,
in the merge queue, which runs them all once more on `main` with the change merged. What
this page changes is how often a change has to go round that loop before it lands.

## Where the time goes

Measured on this repository on 2026-11-22 (200 CI runs over 27 hours, raw data in the
pull request that added the CI tiers): a full CI run is 46 jobs and 3.4 runner-hours; it
took a median of 28 minutes on a pull request (55 at the 90th percentile, waiting for
runners) and 24 in the merge queue. The time was lost to:

1. **Pushes that cancel the run before them.** 92 of 174 pull request runs (53%) were
   cancelled by the next push to the same branch, after a median of 7 minutes; a branch
   took 5.6 runs, and the median gap between two pushes (18 minutes) was shorter than one
   run. Each push restarts the 28 minutes.
2. **Every job for every change.** A pull request that changed only the site ran all 46
   jobs, on the pull request and again in the queue. A draft that was still being written
   ran them on each push.
3. **Stalls outside CI.** Finished pull requests waited for hours with auto-merge on and
   an unresolved review thread, which the branch protection requires resolved; release
   pull requests needed lock files under `bench/`, `fuzz/` and `sim/` bumped by hand.
4. **Conflicts between pull requests in flight**: shared lines in `CHANGELOG.md` (now
   `changelog.d/`), README numbers, and limits two changes each stay under alone but not
   together (`bench/instr/baseline.csv`). The queue failed 1 run in 24, for the last.
5. **Red runs that a local check would have caught**, and **lost context** when a new
   session re-learns what the last one knew.
6. **Leftovers.** Branches and draft pull requests that nobody owns any more hide the ones
   that matter and get rebased, reviewed or merged by mistake.

Each section below removes one of them. Fixing a bug is never one of the costs to cut:
a red job is fixed at its cause, whatever it takes.

## CI tiers

The job `plan` (`scripts/ci-scope.py`, tested by `scripts/test_ci_scope.py`) chooses what
a run does; `scripts/preflight.sh --plan` prints its choice for a branch. Under Workflow
v2 ([WORKFLOW.md](WORKFLOW.md#ci-tiers) has the full table and the reasons):

| The run | What runs |
|---|---|
| Pull request that changes no file a build or test reads: `site/`, `changelog.d/`, `bench/results/`, `docs/workflow/`, `assets/`, `skills/`, `.github/known-flakes.txt`, prose | `lint` (fmt, clippy, docs, `release.py check`, which builds the site) and `msrv` |
| Any other pull request, draft or ready | the above, default features, the release build, one round of the tests on io_uring and of the epoll tier, `instr` |
| ...ready for review, touching macOS or Windows code (`ci-scope.py --platform`) | also the macOS or Windows workflow |
| Merge queue, a push to `main` that no queue run tested, a dispatch of CI | every job with every repetition: five rounds per tier, three ThreadSanitizer runs, Miri, Kani, the kernels, the server versions, semver; macOS and Windows, called from `ci.yml` so that `all-checks` fails when they do |
| Merge queue, a group of only such docs files | `lint` and `msrv` |

**Runners are the scarce resource**: the organisation gets 60 jobs at a time (5 on
macOS), shared by every pull request and the merge queue, with no priority between them.
In v1 a ready pull request ran every job once and the queue then ran them all again; the
runners that took held the queue at two groups at a time, and the queue is where a
change waited longest (162 of 201 minutes, [measured](workflow/data/v1/README.md)). v2
leaves the heavy jobs to the queue alone, so it can build more groups at once.

Prose counts as code where code reads it: `docs/errors/` (tests check every page) and
every Markdown file a crate pulls in with `include_str!` (its code blocks are doc tests),
such as the READMEs. Anything `ci-scope.py` cannot place counts as code. Nothing with code
in it lands without the queue's full run, whatever the pull request ran. A failure that
only a heavy job or a repetition shows (Miri, a kernel, macOS, a race) is then found in
the queue instead of on the pull request, and ejects the pull request from it: fix it like
any red job. For a risky change (the unsafe core, a driver, the scheduler, cross-thread
wake-ups), run the full set on the branch before enqueueing: Actions, CI, Run workflow,
on the branch.

**One compiler.** CI's stable Rust is pinned (`RUST_STABLE` in the workflows), not
`stable`, so a Rust release cannot turn `main` red overnight, and `scripts/preflight.sh`
runs the same version (`--install` installs it; the gate warns when it is missing). Raise
the pin in a pull request of its own after each Rust release, fixing what the new lints
find in the same pull request.

## When CI fails outside the code

**The CI gate.** Required on `main`: one commit status, `CI gate`, which
`.github/workflows/ci-gate.yml` sets when a run of CI starts (pending) and ends. It
passes exactly when the run passed. When the run failed only outside the code, it reruns
the failed jobs of that same run, and only those (GitHub adds the jobs that wait for
them, such as `all-checks`), at most twice, while the merge queue keeps the pull request
in place; the jobs that passed keep their result. Before it, the first failed required
job removed the pull request from the queue and every group behind it started again from
nothing (release 1.4.1 lost three queue runs that way on 2026-11-23).

What counts as outside the code (`scripts/ci-gate.py`, tested by
`scripts/test_ci_gate.py`): a failed step whose name starts with `net: `, GitHub's own
setup steps (checkout, toolchain, caches, artifacts), a runner GitHub lost, a job that
ran out of time inside a `net: ` step. Everything else fails the gate at once: a test, a
lint, a build, and a job that ran out of time anywhere else, since a hang is a bug. **A
failing test is never rerun to pass**, with the one exception the project's rules make: a
known flake whose issue is open with a fix on its way, listed in
`.github/known-flakes.txt`, is rerun once, and the run's summary names the issue. A test
that fails in the queue on code the pull request did not touch goes into that list
within the hour, in a pull request of its own that only adds the line (it runs as docs:
minutes to land), then gets its fix ([WORKFLOW.md](WORKFLOW.md#flaky-tests)).

**Naming a step `net: `** is a promise that it only fetches or installs: it goes through
`scripts/ci-net.sh` (retries with a time limit on every try, `fetch` with a SHA-256) or
`.github/actions/apt-install`. Never put a build or a test in such a step: the gate would
rerun its failures.

**Mirrors off the hot path.** Ubuntu packages come from a cache of their `.deb` files
(`.github/actions/apt-install`, `scripts/ci-apt.sh`), keyed by the runner image and
falling back to another image version's files when they still install (GitHub serves
several image versions at once), so a job goes to an Ubuntu mirror about once a week
instead of every run. The database servers of
the `servers` job (`scripts/ci-servers.sh`), h2spec, cargo-semver-checks and kani-verifier
are cached the same way. A new apt package goes through the action, never a bare
`apt-get`.

**Starting CI again by hand.** Use "Re-run failed jobs" on the run (or
`gh run rerun --failed`), never a new push or a new enqueue: the jobs that passed keep
their result. On a test failure, read the log and fix the cause first.

## Supply chain

`deny.toml` is the policy for what the workspace builds with, checked by
[cargo-deny](https://github.com/EmbarkStudios/cargo-deny): only permissive licenses, crates
from crates.io only (no git or other registry), no wildcard version requirement, and no
crate with a RustSec advisory or a yanked version. CI's `lint` job checks the licenses,
sources and bans on every pull request. Advisories appear at any time, independently of
any change, so `.github/workflows/supply-chain.yml` checks them every day on `main` (and
on a pull request that changes a `Cargo.toml`, `Cargo.lock` or `deny.toml`) and opens the
issue labelled `supply-chain` when one applies. The fix is a `cargo update -p <crate>` in a
pull request; an advisory that cannot apply to giong goes into `deny.toml`'s `ignore` with
its reason. The lockfiles under `bench/` and `fuzz/` are not policed: they build
comparisons and harnesses that are never published.

## One change, start to finish

1. **Take one item** from the backlog of your line of work (see [The backlog](#the-backlog)).
   One concern per pull request, where related small fixes from one line of work (the
   same crate, the same debug round) count as one concern: up to about five share a pull
   request, each its own commit with its own test and `Fixes #N` in the description
   ([WORKFLOW.md](WORKFLOW.md#batching-fixes)). The unsafe core, a driver and a public
   API change keep a pull request of their own. Work too large to review in one sitting
   is split into pull requests that each pass CI on their own; a long line of work keeps a
   draft pull request open and lands in pieces.
2. **Branch from the newest `main`**: `git fetch origin main && git switch -c <type>/<name> origin/main`.
3. **Test first for a fix**: the test fails before the change and passes after.
4. **Install the pre-push hook once per clone**: `scripts/preflight.sh --install-hook`.
   Every `git push` then runs `scripts/preflight.sh --quick` first and stops on a failure.
   Do not bypass it with `--no-verify`.
5. **Pass the gate, `scripts/preflight.sh`, before marking a pull request ready.** It reads the files the
   branch changes and runs every check CI runs that they call for: the merge into `main`,
   `scripts/release.py check`, and when Rust changed fmt, clippy and docs with warnings as
   errors, the minimum Rust versions (`scripts/msrv.sh`, the same script as CI's `msrv` job) and the tests on io_uring and on epoll (of the crates that
   changed, or of the whole workspace when the runtime did); `scripts/semver.sh` when a
   published crate's code changed; one round of ThreadSanitizer for unsafe code and
   anything that crosses threads; loom for the wake-ups; every fuzz target of a crate
   whose parsing code changed; and `cargo deny --workspace check` when a `Cargo.toml`,
   `Cargo.lock` or `deny.toml` changed (no RustSec advisory or yanked crate, permissive
   licenses, crates.io only; [Supply chain](#supply-chain)). A missing tool fails the gate (`--install` installs them);
   nothing is skipped quietly. `--quick` runs the fast part while iterating, `--plan` only
   prints. The gate also says which soak a release containing the change will need.
6. **Write the changelog entry as a file in [`changelog.d/`](../changelog.d/README.md)**,
   never in `CHANGELOG.md`.
7. **Open the pull request as a draft** and fill in the template; the description is the
   handoff (see below). A draft runs the fast set of CI jobs. **Mark it ready for
   review** when the work is complete and the full gate passes. Marking it ready starts
   no new CI run (the fast set of its last push stands); it starts the macOS and Windows
   workflows when the change touches their code.
8. **Push finished steps, not edits.** A push cancels the run in progress. Collect the
   fixes for one round of CI or review into one push. Push sooner only when the running
   job is already red, or when the fix is for a job that has not started.
9. **Read CI, do not wait for it.** When a job fails, read its log (`gh run view <id>
   --log-failed`) and fix the cause. A failure is the author's to fix even when it looks
   unrelated; a test that fails only sometimes is a bug, not a reason to run CI again.
10. **Answer every review thread** once its fix is pushed: reply `Fixed in <commit>` and
   the Review threads workflow resolves it (`scripts/review-threads.py`; or resolve it by
   hand), or reply why not. The branch protection holds the merge back while any thread is
   open, silently, even with auto-merge on; that workflow's job summary lists the open ones.
11. **Do not merge `main` in to stay current.** The queue tests the pull request merged
   with the newest `main` anyway. Merge `main` in only for a conflict, or when a change on
   `main` affects this one (a new baseline, an API it uses).
12. **Before enqueueing, check the open pull requests**:
   `scripts/preflight.sh --against <n>,<n>` merges the branch with each of them and names
   the files that conflict. Agree the order with their authors when two conflict, rather
   than letting the queue eject one.
   **Enqueue by turning on auto-merge** for the pull request (`gh pr merge <n> --squash
   --auto`, or the REST or MCP call that enables auto-merge): on a branch with a merge queue
   it adds the pull request to the queue as soon as every required check has passed, and
   the queue merges it only if every job passes again on `main`. It changes nothing in
   what must pass; it only means nobody has to press the button. Never turn it on for a
   release pull request (label `release`): merging one publishes to crates.io, which
   needs a maintainer's explicit approval.
13. **After the merge**, check that the branch was deleted, remove the item from the
   backlog, and take the next one.

## Working in parallel without collisions

- **Changelog**: one file per pull request in `changelog.d/`. The release pull request
  merges them into `CHANGELOG.md` (`scripts/release.py unreleased` shows the result).
- **Error codes**: a line of work that will add several `GIONG-E` codes reserves a range
  in its tracking issue or draft pull request before it uses them, so that two branches do
  not take the same number. Codes are never renumbered once merged.
- **Numbers in READMEs** (test counts, benchmark results) change only in the pull request
  that measured them. On a conflict, measure or count again; do not add the two sides up
  by hand.
- **Lock files**: the release bumps the workspace crates in every `Cargo.lock`
  (`scripts/release.py sync-locks`); a pull request never edits another workspace's lock
  by hand for a version, and `release.py check` fails on a stale one.
- **Limits shared by every change** (`bench/instr/baseline.csv`): two pull requests that
  each stay inside a limit can cross it together. When one merges first, merge `main`
  into the other and run `scripts/instr.py check` before it enters the queue.
- **Long-lived branches** take `main` in with a merge commit. Never rebase or force-push a
  branch that another branch or pull request is based on.
- **Stacked work** opens its pull request against the branch below it; when that one
  merges, retarget to `main` and merge `main` in.
- **Shared files between two lines of work** (a driver facade, a platform layer): one line
  owns each file. The other proposes its change to the owner instead of editing in
  parallel.

## Choosing the checks

| The change touches | Before pushing | Soak before a release |
|---|---|---|
| docs, the site, `changelog.d/`, benches | the gate (no build) | none |
| a crate off the soak paths: the database clients, `giong-web`, `giong-mcp`, the HTTP client | the gate, which fuzzes the parsers that changed | none |
| a soak path: the drivers, the scheduler, cancellation, sockets, channels, timers, files, the HTTP/2 or TLS state of the servers | the gate (both drivers, TSan), `scripts/check.sh fallback` | 2 to 4 hours per driver |
| the unsafe core: `src/driver/`, `src/op.rs`, `src/buf.rs`, `src/kbox.rs`, `src/slab.rs` | the above, `scripts/valgrind.sh`, `scripts/check.sh kani` | 2 to 4 hours per driver |
| cross-thread wake-ups: `src/notify.rs`, the injector, `TaskWaker` | the above (the gate runs loom) | 2 to 4 hours per driver |
| a proven file: `src/ipc/ring.rs` | the same edit in its mirror in `verify/`, `scripts/check.sh verus` ([verus.md](verus.md)) | as for the file's row |

The soak tiers are defined in [RELEASING.md](RELEASING.md#how-long-to-soak); a major
release soaks for 72 hours whatever changed. The merge queue runs every job whatever the
table says, including what no local machine runs (kernels 5.15 to 6.18, the older ring
tiers, Miri, Kani, instruction counts, other server versions, macOS, Windows): the gate
makes a green queue run likely, it does not replace it. Since Workflow v2 those jobs no
longer run on the pull request, so the gate is what keeps them from ejecting it.

## The backlog

Work is taken from a backlog, one section per line of work (a driver, a protocol, a
release), each item with a one-line acceptance test. An agent works through its section in
order, notes the item it takes, and removes it once merged. When its section is empty, or
while CI runs and nothing else in the section is independent, it takes an item from the
unassigned section. New work found along the way goes at the end of the right section
instead of into the current pull request.

## Leave nothing behind

- **Merged**: GitHub deletes the branch of a merged pull request
  (`delete_branch_on_merge`). Check it did.
- **Superseded or abandoned**: close the pull request with a comment naming what replaces
  it. Its branch is deleted by the Hygiene workflow, which deletes only branches whose tip
  is the head of a closed or merged pull request (GitHub keeps it at `refs/pull/<n>/head`
  and can restore the branch from the pull request page) or is already in `main`, and
  never `main`, `release/*`, a branch an open pull request uses, or one listed in
  `.github/hygiene-keep.txt`. It runs every Monday; start it after closing pull requests
  (Actions, Hygiene, Run workflow, or the `workflow_dispatch` API).
- **A branch with no pull request** is work in progress for at most a day: open a draft
  pull request for it, or list it in `.github/hygiene-keep.txt` with the reason it stays.
- **A draft that waits** says in its description what it waits for (a release, another
  pull request, a measurement).
- **Issues**: a pull request that resolves an issue says `Fixes #<n>` in its description,
  so the merge closes it. An issue answered some other way (by measurements already in
  the repository, by a change that has merged) is closed with a comment linking what
  answered it and stating any part left undone, which goes to the backlog. An issue that
  is not being worked on says what it waits for.
- **Every Monday** the Hygiene workflow (`scripts/hygiene.py`) deletes those branches and puts the branches with no
  open pull request, the pull requests untouched for a week and the issues untouched for
  two into the issue labelled `hygiene`. Whoever owns an entry deals with it that week; the issue closes itself when
  nothing is left.

## Never idle

- **While CI runs**, take the next independent item, or review another open pull request.
- **When blocked on a person** (a credential, a paid machine, the first publish of a new
  crate), ask once for the exact action, naming the pull request or branch, and carry on
  with work that does not depend on it.
- **When a step finishes**, say what is next in the pull request or issue, then do it.

## Handoff: what must outlive a session

A session ends; the repository stays. Anything the next session needs goes where it will
look:

- **The pull request description**: what and why, how it was tested (commands and
  results), what is left, and for a draft the next step and the command to resume.
- **The code and its docs**: a behaviour code relies on is documented where the code is
  (AGENTS.md, rule 9). A trap that cost time goes into AGENTS.md's *Traps*.
- **Decisions** that shape later work (an API choice, a rejected design with its numbers)
  go into `docs/reviews/` or the tracking issue, not only into a conversation.
- **Measurements** go into `bench/results/` with the raw data and the method.

## What stays with the maintainers

Agents working for the maintainers push branches, open and update pull requests, and
enqueue them through auto-merge once they are ready. These stay with a
maintainer: merging a release pull request (it publishes to crates.io, which cannot be
undone), the first publish of a new crate, branch protection and repository settings,
secrets, and paid machines.
