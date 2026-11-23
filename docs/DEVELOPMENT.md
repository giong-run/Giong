# Developing giong with agents

Most of giong is written by AI coding agents, several at a time, each on a branch of its
own. [AGENTS.md](../AGENTS.md) holds the rules for the code; this page holds the rules for
the flow of work, so that many agents in parallel stay fast without lowering the bar.
Everything here applies to people too. Where it and [CONTRIBUTING.md](../CONTRIBUTING.md)
disagree, CONTRIBUTING.md wins: report the difference.

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

The job `plan` (`scripts/ci-scope.py`) chooses what a run does; `scripts/preflight.sh
--plan` prints its choice for a branch.

| The run | What runs | Expected time |
|---|---|---|
| Pull request that changes no file a build or test reads: `site/`, `changelog.d/`, `bench/results/`, `assets/`, `skills/`, prose | `lint` (fmt, clippy, docs, `release.py check`, which builds the site) and `msrv` | 2 min |
| Draft pull request | the above, default features, the release build, one round of the tests on io_uring; the Fuzz, gRPC, Proto and macOS workflows skip drafts | 16 min, 1,000 runner-seconds (a twelfth of a full run) |
| Pull request ready for review | every job once: each suite one round on every tier, two ThreadSanitizer runs, the kernels and server versions four at a time, Miri, Kani, instr, macOS, Windows (when code it builds changed) | 17 min of jobs, 8,400 runner-seconds (two thirds of a full run) |
| Merge queue, a push to `main` that no queue run tested, a dispatch | every job with every repetition: five rounds per tier, three ThreadSanitizer runs, every matrix entry at once; macOS and Windows, called from `ci.yml` so that `all-checks` fails when they do | 24 min, 12,200 runner-seconds |

The times are those of the pull request that introduced the tiers (#218) and of the queue
runs before it. **Runners are the scarce resource**: the organisation gets 60 Linux jobs at
a time, shared by every pull request and the merge queue, and GitHub has no priority
between them. A full run asks for 46 jobs, so two pull requests running full sets starve
the queue (the 1.4.0 release waited behind 60 queued runs). Every tier above exists to
give runners back: a draft takes a twelfth, a ready pull request two thirds and its two
largest matrices four jobs at a time, and a push that cancels a run frees its jobs.

Prose counts as code where code reads it: `docs/errors/` (tests check every page) and
every Markdown file a crate pulls in with `include_str!` (its code blocks are doc tests),
such as the READMEs. Anything `ci-scope.py` cannot place counts as code. What lands on
`main` is tested exactly as before: the queue runs everything, whatever the pull request
ran. A failure that only a repetition shows (a race, a rare timing) is then found in the
queue instead of on the pull request, and ejects the pull request from it: fix it like any
red job.

**One compiler.** CI's stable Rust is pinned (`RUST_STABLE` in the workflows), not
`stable`, so a Rust release cannot turn `main` red overnight, and `scripts/preflight.sh`
runs the same version (`--install` installs it; the gate warns when it is missing). Raise
the pin in a pull request of its own after each Rust release, fixing what the new lints
find in the same pull request.

## One change, start to finish

1. **Take one item** from the backlog of your line of work (see [The backlog](#the-backlog)).
   One concern per pull request. Work too large to review in one sitting
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
   anything that crosses threads; loom for the wake-ups; and every fuzz target of a crate
   whose parsing code changed. A missing tool fails the gate (`--install` installs them);
   nothing is skipped quietly. `--quick` runs the fast part while iterating, `--plan` only
   prints. The gate also says which soak a release containing the change will need.
6. **Write the changelog entry as a file in [`changelog.d/`](../changelog.d/README.md)**,
   never in `CHANGELOG.md`.
7. **Open the pull request as a draft** and fill in the template; the description is the
   handoff (see below). A draft runs the short set of CI jobs. **Mark it ready for
   review** when the work is complete and the full gate passes: that starts every job.
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

The soak tiers are defined in [RELEASING.md](RELEASING.md#how-long-to-soak); a major
release soaks for 72 hours whatever changed. CI runs every job on every pull request
ready for review and again in the merge queue whatever the table says, including what no
local machine runs (kernels 5.15 to 6.18, the
older ring tiers, Miri, Kani, instruction counts, other server versions): the gate makes a
green run likely, it does not replace it.

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
