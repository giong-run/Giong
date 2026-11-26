# Workflow metrics: 2026-11-25T03:16:00Z to 2026-11-26T13:19:00Z

59 pull requests merged into main (fetched 2026-11-26T14:27:37Z).

| Per pull request | median | p90 | max | total |
|---|---|---|---|---|
| ready -> merged | 201 min | 449 min | 909 min | 13644 min |
| ready -> first enqueue (own CI, review threads) | 30 min | 54 min | 256 min | 2398 min |
| time in the merge queue (all entries) | 162 min | 386 min | 812 min | 10851 min |
| runner time, PR runs, Linux | 3.7 h | 6.9 h | 20.6 h | 267.0 h |
| runner time, PR runs, macOS | 0.5 h | 0.9 h | 1.7 h | 33.9 h |
| runner time, PR runs, Windows | 0.7 h | 1.2 h | 6.1 h | 45.8 h |
| runner time, queue runs, Linux | 5.1 h | 10.1 h | 19.1 h | 427.6 h |
| runner time, queue runs, macOS | 0.4 h | 0.9 h | 2.2 h | 37.3 h |
| runner time, queue runs, Windows | 1.1 h | 2.3 h | 4.5 h | 92.2 h |

- Queue entries: 74 for 59 merges; ejections: 15 (11 pull requests ejected at least once).
- CI runs on pull requests: 137, of which 42 cancelled by a newer push; CI runs in the queue: 87.
- Pushes per pull request: median 1, max 7.
- Runner time for these pull requests: 346.7 h on pull request runs, 557.1 h in the queue.
- Merges: 59 over 33.8 h; gap between merges median 33 min, p90 74 min.
- Queue CI runs in the data: 150; passed 116, failed 31, cancelled 1; a passing one took median 38 min, p90 44 min.
- Most queue CI runs in progress at once: 5.
- A passing pull request CI run took median 26 min, p90 38 min.

## Ejections

| PR | removed | failed jobs of the queue run before |
|---|---|---|
| #406 | | CI/all-checks; CI/windows / crates (windows-2022); CI/windows / windows-checks |
| #424 | | (no failed queue job: dequeued by hand, a group ahead failed, or a push) |
| #422 | | CI/all-checks; CI/windows / crates (windows-2025); CI/windows / windows-checks |
| #410 | | CI/all-checks; CI/tsan; CI/tsan (io_uring, run 2) |
| #410 | | CI/all-checks; CI/fallback; CI/fallback (epoll); CI/macos / macos-checks; CI/macos / test (macos-15); CI/windows / crates (windows-2025); CI/windows / windows-checks |
| #431 | | CI/all-checks; CI/macos / macos-checks |
| #437 | | CI/all-checks; CI/fallback; CI/fallback (no-features) |
| #426 | | CI/all-checks; CI/macos / macos-checks; CI/macos / macos-tsan |
| #426 | | (no failed queue job: dequeued by hand, a group ahead failed, or a push) |
| #426 | | CI/all-checks; CI/macos / macos-checks; CI/macos / test (macos-26) |
| #407 | | CI/all-checks; CI/windows / crates (windows-2025); CI/windows / windows-checks |
| #445 | | CI/all-checks; CI/windows / crates (windows-2025); CI/windows / windows-checks |
| #414 | | CI/all-checks; CI/windows / crates (windows-2025); CI/windows / windows-checks |
| #409 | | CI/all-checks; CI/windows / crates (windows-2025); CI/windows / windows-checks |
| #409 | | CI/all-checks; CI/windows / crates (windows-2025); CI/windows / windows-checks |

## Slowest ten, ready -> merged

| PR | ready -> merged | own CI wait | in queue | entries | title |
|---|---|---|---|---|---|
| #426 | 909 min | 25 min | 812 min | 4 | fix: percent decoding takes exactly two hex digits; giong db |
| #409 | 640 min | 54 min | 556 min | 3 | fix(quic): a connection lost before it is accepted frees its |
| #410 | 636 min | 46 min | 532 min | 3 | perf(runtime): keep the empty-turn checks inline and the wor |
| #431 | 552 min | 33 min | 473 min | 2 | fix(cli): giong dev waits for a new build to bind when it ha |
| #437 | 492 min | 32 min | 428 min | 2 | fix(cli): the worker template's tests give each test its own |
| #445 | 458 min | 26 min | 397 min | 2 | fix(web): Record redacts secret-named headers and multipart  |
| #407 | 449 min | 41 min | 380 min | 2 | ci(templates): smoke runs take a free port below the ephemer |
| #414 | 443 min | 29 min | 386 min | 2 | docs: the panic hook runs on the worker; run production with |
| #413 | 318 min | 59 min | 259 min | 1 | fix(fs): read directories on network filesystems off the wor |
| #406 | 311 min | 160 min | 92 min | 2 | feat: every crate builds and passes its tests on Windows |
