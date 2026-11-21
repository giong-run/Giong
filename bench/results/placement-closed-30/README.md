# Skewed, back to back, 30 rounds

`CASES=closed ONLY_MODES="before balanced" BEFORE=<giong 1.0.0 checkout> bench/placement.sh 30 4`,
Linux 6.18, 4 vCPUs, server on CPUs 0-1, client on 2-3. `compare.txt` is
`python3 bench/placement/compare.py raw.csv skew-closed`. `client.log` gives each worker's
share of the requests and its own p50 and p99.
