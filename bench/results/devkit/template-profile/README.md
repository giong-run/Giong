# The dev profile `giong new` writes

Every project `giong new` makes carries this in its Cargo.toml:

```toml
[profile.dev]
debug = "line-tables-only"

[profile.dev.package."*"]
debug = false
```

Measured on the project of `giong new shop --db postgres,redis --tls` (this checkout's
crates by path, one Cargo.lock for both), against the same project with Cargo's default
profile. Three rounds, interleaved; the table gives the median. Machine and commit in
[machine.txt](machine.txt), every round in [raw.csv](raw.csv), the script in
[run.sh](run.sh).

| | Cargo's default | `giong new` profile | change |
|---|---:|---:|---:|
| clean debug build | 18.04 s | 14.72 s | -18% |
| target directory | 652 MB | 309 MB | -53% |
| rebuild after a one-line change | 0.76 s | 0.59 s | -22% |

What it costs: a debugger shows the project's own lines and no variable values in
dependency frames; panics and backtraces keep file and line for the project's code.
Delete the two tables to get Cargo's default back.
