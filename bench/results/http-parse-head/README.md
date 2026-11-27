# Request heads read in one pass

giong-http before and after the change of `changelog.d/perf-http-parse-head.md`, on top of the
per-connection alarm (`bench/results/http-conn-alarm/`). After it, callgrind on
`bench/giong-http` put 593 of the server's 5,123 instructions per `/json` request in
`parse_head`, for a 33-byte head (`GET /json HTTP/1.1`, `Host: x`): every byte went through
three or four loops, one after the other (find the LF, find each space, check the method's
bytes, check the target's, find the colon, check the name, trim and check the value).

- **The request line in one pass**, when it is the common kind: a method of at most 32 token
  bytes, a target of visible ASCII but `#`, then `HTTP/1.1` or `HTTP/1.0` and CRLF, with single
  spaces. The same tables as before (the target's is new, with the same bytes as the check it
  replaces). Anything else goes to the checks one after the other, which say what is wrong
  (400, or 505 for another version).
- **Every well-formed header line in one pass**, with `quick_field`, which the client already
  used for response heads: a token name, a colon, a value without control bytes but tab,
  CRLF. Anything else, the careful checks.

On a head the one-pass paths accept, the careful checks find the same lines, names, values and
ranges (a token or target byte is never a space, a colon or a line end, so each loop ends where
the separate searches would). The new test `the_one_pass_paths_change_no_verdict` checks it:
the old `parse_head`, kept in the tests, and the new one give the same verdict and the same
parsed head on 175,559 generated heads (15,485 of them valid), with methods, targets, versions,
spacing, line ends and header lines picked from good and bad ones, and random bytes changed,
added or removed.

Machine: 4-vCPU cloud VM (Intel Xeon), Linux 6.18, rustc 1.97.0, valgrind 3.22, 2026-11-27.
"before" is the per-connection alarm change (d81d526); "change" is this change on top of it.

## The server alone (bench/giong-http, callgrind)

`bench/results/idle-conn-memory/callgrind.sh` on both builds, two runs of each, interleaved
([`callgrind.txt`](callgrind.txt)): the server's instructions per request on a keep-alive
connection, 2,000 requests after 300 of warm-up.

| path | before | change | change |
|---|---:|---:|---:|
| `/json` | 5,137.3 | **4,934.5** | **−3.95%** |
| `/users` | 177,905.9 | **177,691.0** | −0.12% |

## Instructions (bench/instr)

`scripts/instr.py check --rounds 3` on both trees: [`instr-before.txt`](instr-before.txt) and
[`instr-change.txt`](instr-change.txt) (the "now" column of each is that tree's medians),
[`instr-change-raw.csv`](instr-change-raw.csv).

| case | before (io_uring / epoll) | change (io_uring / epoll) | change |
|---|---:|---:|---:|
| `http1` | 6,971.1 / 7,882.6 | **6,847.3 / 7,758.4** | **−1.78% / −1.57%** |
| `web` | 9,858.1 / 10,771.6 | **9,675.2 / 10,592.4** | **−1.86% / −1.66%** |
| `http1-client` | 18,669.5 / 19,766.6 | **18,357.1 / 19,446.6** | **−1.67% / −1.62%** |
| `https` | 29,535.4 / 30,624.1 | **29,221.9 / 30,310.4** | **−1.06% / −1.02%** |

HTTP/2 rebuilds no HTTP/1.1 head and is unchanged (`h2` within 0.01%, `h2-streams` −0.07% and
−0.04%); every other row is within 0.08%, allocations and heap bytes the same. No row is
higher.
