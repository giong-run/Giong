# bench/compat.sh results

`raw.csv` and `summary.txt` are the latest session (9 interleaved runs, 0.5.0 at
`7158372` with the fixes of #83, 2026-11-14), the one in the table of
[`giong-compat/README.md`](../../../giong-compat/README.md#what-it-costs).

`sessions/` keeps the earlier sessions on the same machine type (4-vCPU cloud VM, Xeon at
2.1 GHz, Linux 6.18), each 9 interleaved runs, named by session number and commit:

| Folder | Commit | Note |
|---|---|---|
| `sessions/1-f992163` | `f992163` | The session the README quoted first (#63) |
| `sessions/2-0bd788a` | `0bd788a` | The pre-1.0 review of giong-compat, first rerun |
| `sessions/3-0bd788a` | `0bd788a` | The same review, second rerun |
| `sessions/4-8632eb7` | `8632eb7` | The table 0.5.0 shipped with (#69) |
| `sessions/5-v0.5.0-fixes` | `7158372` + #83 | First session on 0.5.0 with the review fixes |

`send-by-version/` is one session of the send workload only, with three builds
interleaved: `8632eb7`, 0.5.0 (`7158372`) and 0.5.0 with #83 (`run.sh` there). giong's own
sends measured the same in all three (2,377 to 2,422 MiB/s, 400 to 410 ns per KiB), so
0.5.0 did not change them; Tokio, whose code is the same in all three builds, ranged from
2,351 to 2,853 MiB/s, which is how far identical binaries drift within one session here.

The cost of `Compat` moves between sessions by several percentage points, so the README
gives the range over all of them.
