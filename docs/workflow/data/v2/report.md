# Workflow metrics: 2026-11-26T16:14:19Z to 2026-11-28T03:00:00Z

42 pull requests merged into main (fetched 2026-11-28T03:07:07Z), of 50 in the window: ready after 2026-11-26T16:14:19Z, authors left out: github-actions[bot].

| Per pull request | median | p90 | max | total |
|---|---|---|---|---|
| ready -> merged | 62 min | 141 min | 234 min | 3265 min |
| ready -> first enqueue (own CI, review threads) | 20 min | 86 min | 139 min | 1158 min |
| time in the merge queue (all entries) | 39 min | 85 min | 161 min | 2037 min |
| runner time, PR runs, Linux | 0.9 h | 2.4 h | 6.1 h | 49.6 h |
| runner time, PR runs, macOS | 0.1 h | 0.6 h | 1.2 h | 10.8 h |
| runner time, PR runs, Windows | 0.1 h | 1.2 h | 3.6 h | 21.3 h |
| runner time, queue runs, Linux | 5.0 h | 9.9 h | 20.2 h | 230.5 h |
| runner time, queue runs, macOS | 0.4 h | 0.8 h | 1.7 h | 18.9 h |
| runner time, queue runs, Windows | 1.1 h | 2.3 h | 4.4 h | 51.2 h |

- Queue entries: 49 for 42 merges; ejections: 7 (5 pull requests ejected at least once).
- CI runs on pull requests: 66, of which 11 cancelled by a newer push; CI runs in the queue: 53.
- Pushes per pull request: median 2, max 15.
- Runner time for these pull requests: 81.7 h on pull request runs, 300.7 h in the queue.
- Merges: 42 over 30.0 h; gap between merges median 33 min, p90 120 min.
- Queue CI runs in the data: 126; passed 103, failed 20, cancelled 1; a passing one took median 39 min, p90 43 min.
- Most queue CI runs in progress at once: 3.
- A passing pull request CI run took median 22 min, p90 31 min.

## Ejections

| PR | removed | failed jobs of the queue run before |
|---|---|---|
| #477 | | CI/all-checks; CI/tsan; CI/tsan (epoll, run 1) |
| #480 | | CI/all-checks; CI/macos / macos-checks; CI/macos / test (macos-26); CI/windows / runtime (windows-2022); CI/windows / windows-checks |
| #500 | | CI/all-checks; CI/windows / runtime (windows-2025); CI/windows / windows-checks |
| #500 | | CI/all-checks; CI/instr |
| #499 | | CI/all-checks; CI/instr |
| #523 | | CI/all-checks; CI/instr; CI/macos / macos-checks; CI/macos / test (macos-26) |
| #523 | | CI/all-checks; CI/macos / macos-checks; CI/macos / test (macos-15) |

## Slowest ten, ready -> merged

| PR | ready -> merged | own CI wait | in queue | entries | title |
|---|---|---|---|---|---|
| #499 | 234 min | 115 min | 92 min | 2 | perf(http): a JSON request with no allocation and 23% fewer  |
| #523 | 211 min | 24 min | 161 min | 3 | fix(grpc): Metadata::add refuses the field names HTTP/2 forb |
| #500 | 177 min | 1 min | 161 min | 3 | perf(web): optional fast-gzip, 31% less CPU per gzipped resp |
| #487 | 150 min | 110 min | 40 min | 1 | perf(http): idle HTTP/2 connections hold 24% less memory, 34 |
| #524 | 141 min | 139 min | 2 min | 1 | docs: 1.5.6 summary |
| #510 | 140 min | 97 min | 43 min | 1 | feat(net): recv_into is public API; bench/giong uploads 1.8x |
| #496 | 125 min | 86 min | 39 min | 1 | feat(fs): fs::Root opens paths beneath a directory and never |
| #506 | 109 min | 21 min | 89 min | 1 | test: a failed soak connect says what the machine had (#504) |
| #520 | 105 min | 66 min | 38 min | 1 | feat(cli): a new project's first dev build takes other proje |
| #503 | 98 min | 19 min | 79 min | 1 | test(balance): the stuck-worker test orders its two workers  |
