# bench/compat.sh results

`raw.csv` and `summary.txt` are the latest session (2026-11-17, 9 interleaved runs), the one
in the table of [`giong-compat/README.md`](../../../giong-compat/README.md#what-it-costs). It
interleaves two builds of `bench/compat` ([`run-ab.sh`](run-ab.sh)): `old` is main at
`8606db1` (giong-compat 0.8.0), `new` is perf/core-faster (#145, with 16 KiB receive buffers)
on #142, which carries #146's direct sends. `old` runs only `Compat`, so giong's own API and
Tokio ran once per round, from the new build.

`sessions/` keeps the earlier sessions on the same machine type (4-vCPU cloud VM, Xeon at
2.1 GHz, Linux 6.18), each 9 interleaved runs, named by session number and commit:

| Folder | Commit | Note |
|---|---|---|
| `sessions/1-f992163` | `f992163` | The session the README quoted first (#63) |
| `sessions/2-0bd788a` | `0bd788a` | The pre-1.0 review of giong-compat, first rerun |
| `sessions/3-0bd788a` | `0bd788a` | The same review, second rerun |
| `sessions/4-8632eb7` | `8632eb7` | The table 0.5.0 shipped with (#69) |
| `sessions/5-v0.5.0-fixes` | `7158372` + #83 | First session on 0.5.0 with the review fixes |
| `sessions/6-v0.5.0-table` | `7158372` + #83 | The table 0.5.0 to 0.8.0 shipped with |
| `sessions/7-compat-direct-send` | `8606db1` + #146 | `Compat` sending large writes directly; 8 KiB receive buffers |

`send-by-version/` is one session of the send workload only, with three builds
interleaved: `8632eb7`, 0.5.0 (`7158372`) and 0.5.0 with #83 (`run.sh` there). giong's own
sends measured the same in all three (2,377 to 2,422 MiB/s, 400 to 410 ns per KiB), so
0.5.0 did not change them; Tokio, whose code is the same in all three builds, ranged from
2,351 to 2,853 MiB/s, which is how far identical binaries drift within one session here.

The cost of `Compat` moves between sessions by several percentage points, so the README
gives the range over all of them.
