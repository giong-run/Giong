#!/usr/bin/env python3
"""Summaries of the 1.5.x bare-metal runs: medians over rounds, with min and max.

    summarize.py ab    raw.csv [--base LABEL]   builds against each other (ab-net.sh)
    summarize.py rate  raw.csv                  rate sweep across worker counts (rate.sh)
    summarize.py gzip  raw.csv [--base LABEL]   gzip builds of PR #429 (gzip.sh)
    summarize.py h3    raw.csv [--base LABEL]   HTTP/3 servers (h3.sh)

The base of a comparison is the first build of round 1 unless --base names another. A row is
marked "server" when the server's cores were at least 95% busy, "load" when br-2's CPUs were
at least 90% busy, "link" when the interface carried at least 23 Gbit/s, else "neither".
"""
import csv
import statistics
import sys


def rows_of(path):
    with open(path) as f:
        return list(csv.DictReader(l for l in f if not l.startswith("#")))


def num(v):
    """A number from a CSV field; wrk's '1.23ms' and '456.00us' become microseconds."""
    if v in (None, "", "-"):
        return None
    for suffix, scale in (("us", 1), ("ms", 1e3), ("s", 1e6)):
        if v.endswith(suffix):
            try:
                return float(v[: -len(suffix)]) * scale
            except ValueError:
                return None
    try:
        return float(v)
    except ValueError:
        return None


def med(rows, col):
    xs = [x for x in (num(r.get(col)) for r in rows) if x is not None]
    return statistics.median(xs) if xs else None


def rng(rows, col):
    xs = [x for x in (num(r.get(col)) for r in rows) if x is not None]
    return (min(xs), max(xs)) if xs else (None, None)


def fmt(x, digits=0):
    if x is None:
        return "-"
    if digits == 0:
        return f"{x:,.0f}"
    return f"{x:,.{digits}f}"


def pct(new, old):
    if new is None or not old:
        return "-"
    return f"{(new / old - 1) * 100:+.1f}%"


def limit(rows):
    s, l, g = med(rows, "server_busy"), med(rows, "load_busy"), med(rows, "gbit_s")
    if g is not None and g >= 23:
        return "link"
    if s is not None and s >= 0.95:
        return "server"
    if l is not None and l >= 0.90:
        return "load"
    return "neither"


def base_of(rows, key, argv):
    if "--base" in argv:
        return argv[argv.index("--base") + 1]
    first = [r for r in rows if r.get("round") == "1" and r.get("order", "1") == "1"]
    return (first or rows)[0][key]


def ab(rows, argv):
    base = base_of(rows, "build", argv)
    seen = list(dict.fromkeys(r["build"] for r in rows))
    builds = sorted(seen, key=lambda b: (b != base, seen.index(b)))
    keys = list(dict.fromkeys((r["workers"], r["case"]) for r in rows))
    print(f"base: {base}; medians over rounds (min-max); change against the base")
    print(f"{'workers':>7} {'case':<15} {'build':<10}{'req/s':>11} {'(min-max)':<21}{'change':>8}"
          f"{'p50 us':>9}{'p99 us':>9}{'us/req':>8}{'change':>8}{'srv busy':>9}{'load':>6}{'errors':>7}  limited by")
    for w, c in keys:
        for b in builds:
            g = [r for r in rows if (r["workers"], r["case"], r["build"]) == (w, c, b)]
            if not g:
                continue
            gb = [r for r in rows if (r["workers"], r["case"], r["build"]) == (w, c, base)]
            lo, hi = rng(g, "rps")
            errors = sum(int(num(r["errors"]) or 0) for r in g)
            print(f"{w:>7} {c:<15} {b:<10}{fmt(med(g, 'rps')):>11} {fmt(lo) + '-' + fmt(hi):<21}"
                  f"{pct(med(g, 'rps'), med(gb, 'rps')) if b != base else '':>8}"
                  f"{fmt(med(g, 'p50')):>9}{fmt(med(g, 'p99')):>9}{fmt(med(g, 'us_per_req'), 2):>8}"
                  f"{pct(med(g, 'us_per_req'), med(gb, 'us_per_req')) if b != base else '':>8}"
                  f"{fmt(med(g, 'server_busy'), 2):>9}{fmt(med(g, 'load_busy'), 2):>6}{errors:>7}  {limit(g)}")
        print()
    print("p99 is '-' for h2load cases (h2load reports a mean, shown as p50).")


def rate(rows, argv):
    servers = list(dict.fromkeys(r["server"] for r in rows))
    paths = list(dict.fromkeys(r["path"] for r in rows))
    workers = sorted({int(r["workers"]) for r in rows})
    print("Closed loop (64 connections per worker): requests/s, median over rounds (min-max), gain over one worker")
    for p in paths:
        for s in servers:
            one = None
            for w in workers:
                g = [r for r in rows if r["path"] == p and r["server"] == s and int(r["workers"]) == w and r["rate"] == "0"]
                if not g:
                    continue
                m = med(g, "goodput_rps")
                one = one or m
                lo, hi = rng(g, "goodput_rps")
                print(f"  {p:<7} {s:<11} {w:>2} workers {fmt(m):>11} ({fmt(lo)}-{fmt(hi)})  x{m / one:.2f}  "
                      f"p99 {fmt(med(g, 'p99_us'))} us  {fmt(med(g, 'us_per_req'), 2)} us/req  "
                      f"server {fmt(med(g, 'server_busy'), 2)} load {fmt(med(g, 'load_busy'), 2)} "
                      f"{fmt(med(g, 'gbit_s'), 1)} Gbit/s  errors {sum(int(num(r['errors']) or 0) for r in g)}  [{limit(g)}]")
    print()
    print("Open loop: p99 in microseconds at each offered rate (median over rounds); '!' = answered under 99% of offered")
    for p in paths:
        rates = sorted({int(r["rate"]) for r in rows if r["path"] == p and r["rate"] != "0"})
        print(f"  {p}")
        print("    " + f"{'req/s':>10}" + "".join(f"{s[:10] + ' x' + str(w):>16}" for s in servers for w in workers))
        for rt in rates:
            cells = []
            for s in servers:
                for w in workers:
                    g = [r for r in rows if r["path"] == p and r["server"] == s and int(r["workers"]) == w and int(r["rate"]) == rt]
                    if not g:
                        cells.append(f"{'':>16}")
                        continue
                    ok = (med(g, "goodput_rps") or 0) >= 0.99 * rt
                    cells.append(f"{fmt(med(g, 'p99_us')) + ('' if ok else '!'):>16}")
            print("    " + f"{rt:>10,}" + "".join(cells))
    print()
    print("Highest sustained rate: answered at least 99% of offered, with p99 at most 5 ms (medians)")
    for p in paths:
        for s in servers:
            for w in workers:
                best = None
                for rt in sorted({int(r["rate"]) for r in rows if r["rate"] != "0"}):
                    g = [r for r in rows if r["path"] == p and r["server"] == s and int(r["workers"]) == w and int(r["rate"]) == rt]
                    if g and (med(g, "goodput_rps") or 0) >= 0.99 * rt and (med(g, "p99_us") or 1e9) <= 5000:
                        best = (rt, g)
                if best:
                    rt, g = best
                    print(f"  {p:<7} {s:<11} {w:>2} workers {rt:>11,} req/s  p50 {fmt(med(g, 'p50_us'))} p99 {fmt(med(g, 'p99_us'))} "
                          f"p99.9 {fmt(med(g, 'p999_us'))} us  server {fmt(med(g, 'server_busy'), 2)}  {fmt(med(g, 'us_per_req'), 2)} us/req")


def gzip(rows, argv):
    base = base_of(rows, "build", argv)
    seen = list(dict.fromkeys(r["build"] for r in rows))
    builds = sorted(seen, key=lambda b: (b != base, seen.index(b)))
    keys = list(dict.fromkeys((r["workers"], r["path"], r["mode"]) for r in rows))
    print(f"base: {base}; medians over rounds (min-max); change against the base")
    print(f"{'workers':>7} {'path':<7} {'mode':<7} {'build':<7}{'req/s':>10}{'change':>8}{'us/req':>8}{'change':>8}"
          f"{'p50 us':>8}{'p99 us':>8}{'p99.9':>8}{'max':>8}{'RSS+ KiB':>9}{'peak KiB':>9}{'errors':>7}")
    for w, p, m in keys:
        for b in builds:
            g = [r for r in rows if (r["workers"], r["path"], r["mode"], r["build"]) == (w, p, m, b)]
            gb = [r for r in rows if (r["workers"], r["path"], r["mode"], r["build"]) == (w, p, m, base)]
            if not g:
                continue
            grow = statistics.median((num(r["rss_kib"]) or 0) - (num(r["rss0_kib"]) or 0) for r in g)
            print(f"{w:>7} {p:<7} {m:<7} {b:<7}{fmt(med(g, 'rps')):>10}{pct(med(g, 'rps'), med(gb, 'rps')) if b != base else '':>8}"
                  f"{fmt(med(g, 'us_per_req'), 2):>8}{pct(med(g, 'us_per_req'), med(gb, 'us_per_req')) if b != base else '':>8}"
                  f"{fmt(med(g, 'p50_us')):>8}{fmt(med(g, 'p99_us')):>8}{fmt(med(g, 'p999_us')):>8}{fmt(med(g, 'max_us')):>8}"
                  f"{fmt(grow):>9}{fmt(med(g, 'hwm_kib')):>9}{sum(int(num(r['errors']) or 0) for r in g):>7}")
        print()
    print("open50/open80: open loop at 50% and 80% of the base build's closed-loop rate in the same round.")


def h3(rows, argv):
    base = base_of(rows, "server", argv)
    seen = list(dict.fromkeys(r["server"] for r in rows))
    servers = sorted(seen, key=lambda s: (s != base, seen.index(s)))
    keys = list(dict.fromkeys((r["workers"], r["case"]) for r in rows))
    print(f"base: {base}; medians over rounds (min-max)")
    for w, c in keys:
        for s in servers:
            g = [r for r in rows if (r["workers"], r["case"], r["server"]) == (w, c, s)]
            gb = [r for r in rows if (r["workers"], r["case"], r["server"]) == (w, c, base)]
            if not g:
                continue
            lo, hi = rng(g, "rps")
            print(f"{w:>3} {c:<14} {s:<12}{fmt(med(g, 'rps')):>10} ({fmt(lo)}-{fmt(hi)}) {pct(med(g, 'rps'), med(gb, 'rps')) if s != base else '':>8}"
                  f"  p50 {fmt(med(g, 'p50_us'))} p99 {fmt(med(g, 'p99_us'))} us  {fmt(med(g, 'us_per_req'), 2)} us/req "
                  f"{pct(med(g, 'us_per_req'), med(gb, 'us_per_req')) if s != base else '':>8}  server {fmt(med(g, 'server_busy'), 2)}"
                  f" load {fmt(med(g, 'load_busy'), 2)}  errors {sum(int(num(r['errors']) or 0) for r in g)}")
        print()


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] not in ("ab", "rate", "gzip", "h3"):
        sys.exit(__doc__)
    globals()[sys.argv[1]](rows_of(sys.argv[2]), sys.argv[3:])
