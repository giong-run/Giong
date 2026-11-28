# giong::task: instruction counts with scopes unused

Raw rounds of `scripts/instr.py measure --rounds 1` on a 4-vCPU cloud VM (Linux 6.18.44,
rustc 1.97.0, valgrind 3.22), main 8e0f0b8 (`instr-main-8e0f0b8.csv`) and the task-scopes
branch (`instr-branch.csv`). The counts of the branch's first commit matched its CI run to
the decimal. The table and the reasons are in `changelog.d/task-scopes.md`'s pull request;
`bench/instr/baseline.csv` takes the branch's instruction rows.
