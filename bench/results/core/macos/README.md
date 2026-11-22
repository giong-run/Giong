# Core primitives on macOS: giong against Tokio and smol

`bench/core.sh 5` on GitHub's `macos-15` and `macos-26` runners (Apple Silicon virtual
machines, 3 cores), commit 66f3b90 of #173, run by `.github/workflows/macos-bench.yml`.
`summary.txt` in each directory has the median wall-clock ns per operation over 5 rounds,
with min, max and CPU time; lower is better. The comparison and what it shows are in
[docs/design/portability.md](../../../../docs/design/portability.md), section 7.
