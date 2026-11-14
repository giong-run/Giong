# bench/compat.sh results

`raw.csv` and `summary.txt` are the latest session (9 interleaved runs, `main` at
`8632eb7`, 2026-11-14), the one in the table of
[`giong-compat/README.md`](../../../giong-compat/README.md#what-it-costs).

`sessions/` keeps the earlier sessions on the same machine type (4-vCPU cloud VM, Xeon at
2.1 GHz, Linux 6.18), each 9 interleaved runs, named by session number and commit:

| Folder | Commit | Note |
|---|---|---|
| `sessions/1-f992163` | `f992163` | The session the README quoted first (#63) |
| `sessions/2-0bd788a` | `0bd788a` | The pre-1.0 review of giong-compat, first rerun |
| `sessions/3-0bd788a` | `0bd788a` | The same review, second rerun |

The cost of `Compat` moves between sessions by several percentage points, so the README
gives the range over all of them.
