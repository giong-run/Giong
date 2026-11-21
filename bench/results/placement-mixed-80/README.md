# Mixed arrivals, 80 rounds

`bench/placement.sh` run twice as
`CASES=mixed ONLY_MODES="before balanced" BEFORE=<giong 1.0.0 checkout> bench/placement.sh 40 4`
(rounds 41-80 are the second run, seeds 1-40 again), Linux 6.18, 4 vCPUs, server on CPUs
0-1, client on 2-3. `compare.txt` is `python3 bench/placement/compare.py raw.csv mixed`.
`client.log` also says where the hot and light connections ended up in each round.
