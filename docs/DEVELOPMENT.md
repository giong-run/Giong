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

Measured on this repository: a CI run takes about 17 to 22 minutes from push to result,
and the merge queue runs the same jobs again. The time is lost to:

1. **Red runs that a local check would have caught**: formatting, clippy, a broken doc
   link, a malformed changelog, a test of the crate that changed. Each costs a run and a
   second push.
2. **Conflicts between pull requests in flight.** Most pull requests added a line to
   `CHANGELOG.md`, so the first to merge put the others out of date, and the merge queue
   ejected the ones it had already started.
3. **Waiting.** An agent that finished a step and waits for CI, or for a person, does
   nothing in the meantime.
4. **Lost context.** A new session re-learns what the last one knew, when that lived only
   in a conversation.

5. **Leftovers.** Branches and draft pull requests that nobody owns any more hide the ones
   that matter and get rebased, reviewed or merged by mistake.

Each section below removes one of them.

## One change, start to finish

1. **Take one item** from the backlog of your line of work (see [The backlog](#the-backlog)).
   One concern per pull request. Work too large to review in one sitting
   is split into pull requests that each pass CI on their own; a long line of work keeps a
   draft pull request open and lands in pieces.
2. **Branch from the newest `main`**: `git fetch origin main && git switch -c <type>/<name> origin/main`.
3. **Test first for a fix**: the test fails before the change and passes after.
4. **Pass the gate, `scripts/preflight.sh`, before every push.** It reads the files the
   branch changes and runs every check CI runs that they call for: the merge into `main`,
   `scripts/release.py check`, and when Rust changed fmt, clippy and docs with warnings as
   errors, the Rust 1.80 check and the tests on io_uring and on epoll (of the crates that
   changed, or of the whole workspace when the runtime did); `scripts/semver.sh` when a
   published crate's code changed; one round of ThreadSanitizer for unsafe code and
   anything that crosses threads; loom for the wake-ups; and every fuzz target of a crate
   whose parsing code changed. A missing tool fails the gate (`--install` installs them);
   nothing is skipped quietly. `--quick` runs the fast part while iterating, `--plan` only
   prints. The gate also says which soak a release containing the change will need.
5. **Write the changelog entry as a file in [`changelog.d/`](../changelog.d/README.md)**,
   never in `CHANGELOG.md`.
6. **Push and open the pull request**, as a draft until the preflight passes. Fill in the
   template; the description is the handoff (see below).
7. **Read CI, do not wait for it.** When a job fails, read its log (`gh run view <id>
   --log-failed`) and fix the cause. A failure is the author's to fix even when it looks
   unrelated; a test that fails only sometimes is a bug, not a reason to run CI again.
8. **Before enqueueing, check the open pull requests**:
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
9. **After the merge**, check that the branch was deleted, remove the item from the
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
whatever the table says, including what no local machine runs (kernels 5.15 to 6.18, the
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
  it, then delete its branch. GitHub keeps the commits at `refs/pull/<n>/head` and can
  restore the branch from the pull request page, so nothing is lost.
- **A branch with no pull request** is work in progress for at most a day: open a draft
  pull request for it, or list it in `.github/hygiene-keep.txt` with the reason it stays.
- **A draft that waits** says in its description what it waits for (a release, another
  pull request, a measurement).
- **Every Monday** the Hygiene workflow (`scripts/hygiene.py`) puts the branches with no
  open pull request and the pull requests untouched for a week into the issue labelled
  `hygiene`. Whoever owns an entry deals with it that week; the issue closes itself when
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
