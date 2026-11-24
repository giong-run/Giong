#!/usr/bin/env python3
"""Compare two modes of bench/placement.sh round by round, as distributions.

    python3 bench/placement/compare.py <raw.csv> [case] [mode-a] [mode-b]

For every metric of the case: the quartiles of the rounds in each mode, and
the two-sided p-value of a Mann-Whitney U test (normal approximation with a
tie correction) of whether one mode's rounds tend to be larger. A single
maximum says little: a stall of one request in one round sets it.
"""
import csv
import math
import statistics
import sys


def kv(s):
    return {k: float(v) for k, v in (p.split("=") for p in s.split())}


def quartiles(xs):
    q = statistics.quantiles(xs, n=4, method="inclusive")
    return q[0], statistics.median(xs), q[2]


def mann_whitney(a, b):
    both = sorted([(x, 0) for x in a] + [(x, 1) for x in b])
    ranks, i = [0.0] * len(both), 0
    ties = 0.0
    while i < len(both):
        j = i
        while j + 1 < len(both) and both[j + 1][0] == both[i][0]:
            j += 1
        for k in range(i, j + 1):
            ranks[k] = (i + j) / 2 + 1
        t = j - i + 1
        ties += t**3 - t
        i = j + 1
    n1, n2 = len(a), len(b)
    r1 = sum(r for r, (_, g) in zip(ranks, both) if g == 0)
    u = r1 - n1 * (n1 + 1) / 2
    n = n1 + n2
    sigma = math.sqrt(n1 * n2 / 12 * ((n + 1) - ties / (n * (n - 1))))
    if sigma == 0:
        return u, 1.0
    z = (u - n1 * n2 / 2) / sigma
    return u, math.erfc(abs(z) / math.sqrt(2))


def main():
    path = sys.argv[1]
    case = sys.argv[2] if len(sys.argv) > 2 else "mixed"
    a = sys.argv[3] if len(sys.argv) > 3 else "before"
    b = sys.argv[4] if len(sys.argv) > 4 else "balanced"
    rows = [r for r in csv.DictReader(open(path)) if r["case"] == case]
    va = [kv(r["metrics"]) for r in rows if r["mode"] == a]
    vb = [kv(r["metrics"]) for r in rows if r["mode"] == b]
    print(f"{case}: {len(va)} rounds of {a}, {len(vb)} of {b}; quartiles of the rounds (p25 / median / p75)")
    print(f"{'metric':<15}{a:>26}{b:>26}{'U test p':>10}")
    for k in va[0]:
        xa, xb = [v[k] for v in va], [v[k] for v in vb]
        qa, qb = quartiles(xa), quartiles(xb)
        _, p = mann_whitney(xa, xb)
        fa = " / ".join(f"{x:.0f}" for x in qa)
        fb = " / ".join(f"{x:.0f}" for x in qb)
        print(f"{k:<15}{fa:>26}{fb:>26}{p:>10.2f}")
    print("\nrange of the rounds (min to max)")
    for k in va[0]:
        xa, xb = [v[k] for v in va], [v[k] for v in vb]
        print(f"{k:<15}{f'{min(xa):.0f} to {max(xa):.0f}':>26}{f'{min(xb):.0f} to {max(xb):.0f}':>26}")


if __name__ == "__main__":
    main()
