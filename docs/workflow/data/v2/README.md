# Workflow v2, measured: its first 34 hours

Every pull request merged into `main` from the merge of Workflow v2 (#468, 2026-11-26
16:14 UTC) to 2026-11-28 03:00 UTC: 50 in the window. The comparison with
[v1](../v1/README.md) takes the 42 that became ready for review after v2 landed and
leaves out the 5 release pull requests (opened by the release bot, held for a
maintainer's approval):

    python3 scripts/workflow-metrics.py report docs/workflow/data/v2 \
        --ready-after 2026-11-26T16:14:19Z --exclude-author 'github-actions[bot]'

The files are as for v1: GitHub's answers trimmed (`*.json.gz`), `report.md`, `prs.csv`.

## v1 against v2

| Per pull request | v1 median | v2 median | v1 p90 | v2 p90 |
|---|---|---|---|---|
| ready for review to merged | 201 min | **62 min** | 449 min | **141 min** |
| ready to first queue entry | 30 min | 20 min | 54 min | **86 min (worse)** |
| time in the merge queue | 162 min | **39 min** | 386 min | **85 min** |
| a passing pull-request CI run | 26 min | 22 min | 38 min | 31 min |
| a passing queue CI run | 38 min | 39 min | 44 min | 43 min |

| Totals | v1 (59 PRs) | v2 (42 PRs) |
|---|---|---|
| ejections from the queue | 15 (25 per 100 merges), 11 pull requests | 7 (17 per 100), 5 pull requests |
| ready to merged, pull requests never ejected | 182 min | 61 min |
| ready to merged, pull requests ejected | 458 min | 177 min |
| runner time per pull request, pull-request runs | 5.9 h | **1.9 h** |
| runner time per pull request, queue runs | 9.4 h | 7.2 h |
| macOS runner time per pull request, pull-request runs | 0.57 h | 0.26 h |

## What the numbers say

1. **Three times faster to land**: a median 62 minutes from ready to merged instead of
   201, the 90th percentile 141 instead of 449. Almost all of it is the queue (39
   minutes instead of 162): a pull request now waits about one queue run, not four.
2. **Two thirds less runner time on pull requests** (1.9 h each instead of 5.9). The queue
   run itself is unchanged (39 minutes, every job, every repetition), and the queue uses
   less per merge (7.2 h instead of 9.4) because fewer ejections restart the groups behind.
3. **Fewer ejections** (17 per 100 merges instead of 25), and the flaky-test rule worked:
   the flakes that ejected #480 and #500 were listed in `.github/known-flakes.txt` the
   same hour (#484, #505); #485 then fixed the macOS one.
4. **The cost v2 accepted, measured**: 3 of the 7 ejections were `instr` (instruction,
   heap and RSS counts against `bench/instr/baseline.csv`), a job that v1 ran on every
   pull request and v2 left to the queue. #499 and #500 changed the counts themselves
   (RSS -11%, heap -2%) and learned it only in the queue. `instr` takes 200 seconds on one
   runner: **v2.1 puts it back in the fast set**. The other four were flakes (giong-quic
   under TSan, the macOS ENOBUFS timing, the Windows soak's descriptors, two macOS tests),
   which a pull-request run had not caught in v1 either.
5. **Worse**: the 90th percentile from ready to the first queue entry rose from 54 to
   86 minutes. The slowest (#499, #487, #510, #496: 86 to 115 minutes) were ready during
   the five releases of the window (1.5.2 to 1.5.6), when threads hold their pull requests
   back so the release's queue run gets runners; the data does not separate that wait
   from CI. The next measurement, in a window with fewer releases, will say.
6. **Not measured here**: the queue built at most three groups at once (build
   concurrency 4 was set), so the queue was rarely four deep; the higher setting has not
   yet been tested at full load.

## The 7 ejections

| PR | Failed in the queue | Cause |
|---|---|---|
| #477 | tsan (epoll): giong-quic `connections_lost_before_they_are_accepted_free_their_place` | flaky; #479 changed the test |
| #480 | macOS 26: `a_write_fails_with_the_code_after_two_seconds` (tests/enobufs.rs); Windows 2022: `soak` | flaky, listed in #484, fixed in #485 |
| #500 | Windows 2025: `the_descriptor_count_is_flat_over_runtime_lives` | flaky, listed in #505 |
| #500 | instr: idle-http1 heap_bytes -2.12% (limit 1.5%) | the change itself; instr not on the pull request |
| #499 | instr: idle-http1 rss_bytes -11.33% (limit 10%) | the change itself; instr not on the pull request |
| #523 | instr (the same RSS row as #499, which was in the queue at the time) and macOS 26: giong-http `a_body_that_keeps_the_minimum_rate_is_not_cut` | instr from another change, and a flake |
| #523 | macOS 15: `datagrams_of_a_shared_port_keep_their_control_messages` | flaky; #528 changed the test |
