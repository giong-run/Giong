# Workflow v1, measured: release 1.5.0 to 1.5.1

The baseline of [docs/WORKFLOW.md](../../../WORKFLOW.md): every pull request merged into
`main` between the release of 1.5.0 (2026-11-25 03:16 UTC) and that of 1.5.1
(2026-11-26 13:19 UTC), 59 of them, with every GitHub Actions run from a day before the
window to an hour after it (3,479 runs).

- `prs.json.gz`, `timelines.json.gz`, `runs.json.gz`, `jobs.json.gz`: GitHub's answers,
  trimmed to the fields the report reads (`scripts/workflow-metrics.py fetch`).
- `report.md`: `python3 scripts/workflow-metrics.py report docs/workflow/data/v1`.
- `prs.csv`: the same, one row per pull request (`--csv`).

## What the numbers say

| Per pull request | median | p90 |
|---|---|---|
| ready for review to merged | 201 min | 449 min |
| ready to first entry in the merge queue (its own CI run) | 30 min | 54 min |
| time in the merge queue, every entry added up | 162 min | 386 min |
| a passing merge-queue CI run | 38 min | 44 min |
| a passing pull-request CI run | 26 min | 38 min |

1. **The merge queue is where the time goes**: 162 of the 201 minutes. A queue run takes
   38 minutes, but the queue built at most two groups at a time (two runs in progress
   56% of the window, three while an ejected one finished 17%) while 8 to 12 pull
   requests waited in it for 45% of the window. A pull request that was never ejected
   still spent a median 141 minutes in the queue.
2. **Ejections**: 15 removals from the queue that were not the merge, for 11 pull
   requests; those took a median 458 minutes from ready to merged against 182 for the
   others, and 72 of the 181 queue hours. Each also restarted the groups behind it.
3. **Most ejections were flaky tests** the pull request did not touch (from the job logs,
   classified by hand below): 11 of 15. Each was then fixed in a pull request of its own
   that went through the same queue (giong-mcp's slow-request test took three: #441,
   #448, #452).
4. **Runner time** for these 59 pull requests: 347 hours on pull-request runs (267 Linux,
   34 macOS, 46 Windows) and 557 in the queue (428 Linux, 37 macOS, 92 Windows). A pull
   request ready for review ran every job once, then the queue ran every job again with
   every repetition.
5. Not the problem: runner waits (a job started a median 3 s after it was queued in
   merge-queue runs, p90 207 s), pushes (median one per pull request; 42 of 137
   pull-request CI runs were cancelled by a newer push).

## The 15 ejections

| PR | Failed in the queue | Cause |
|---|---|---|
| #406 | Windows crates (2022): giong-postgres auth tests | real: the Windows port itself |
| #424 | no failed job | removed by hand or a group ahead failed |
| #422 | Windows crates: giong dev `a_change_that_fails_stops_the_old_build_and_says_why` | flaky |
| #410 | tsan (io_uring, run 2): giong-grpc `a_client_that_does_not_read_holds_the_handler_back` | flaky |
| #410 | Windows crates and fallback (epoll): giong-mcp `slow_requests_*`; macOS: `requests_are_split_into_phases_and_grouped_by_route` | flaky |
| #431 | `macos-checks` with no failed test | a macOS job cancelled or lost |
| #437 | fallback (no-features): `where_reuse_port_counts_at_the_bind_*` | flaky |
| #426 | macOS TSan: `requests_are_split_into_phases_and_grouped_by_route` | flaky |
| #426 | no failed job | removed by hand or a group ahead failed |
| #426 | macOS 26: giong dev `a_change_that_fails_*` (AddrInUse) | flaky |
| #407 | Windows crates: giong-mysql `a_cancel_request_stops_the_running_statement` | flaky |
| #445 | Windows crates: giong-mysql `the_statement_timeout_is_enforced_by_the_server` | flaky |
| #414 | Windows crates: giong-mcp `slow_requests_name_the_route_that_blocks_the_worker` | flaky |
| #409 | Windows crates: giong-mcp `slow_requests_blame_a_pool_that_makes_requests_wait` | flaky |
| #409 | Windows crates: giong-mysql `the_statement_timeout_is_enforced_by_the_server` | flaky |

None of these tests was in `.github/known-flakes.txt`, so the CI gate could not rerun
them; and the gate was not yet the only required check, so a failure ejected at once.
