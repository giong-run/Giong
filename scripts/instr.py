#!/usr/bin/env python3
"""Speed and memory thresholds: instructions per operation on giong's hot
paths, counted by callgrind, and heap per idle connection, checked against
bench/instr/baseline.csv.

    scripts/instr.py check     measure, compare with the baseline, exit 1 past a limit
    scripts/instr.py update    measure and write the baseline (keeps each limit)
    scripts/instr.py measure   measure only

Options: --rounds N (default 3; the median of the rounds counts), --out DIR
(default target/instr), --cases a,b (default all). BENCH_INSTR=path uses a
binary built already instead of building bench/instr.

Why instructions: on a shared CI runner the time a request takes moves by
tens of percent from run to run, the number of instructions it executes by
less than 0.1% (bench/instr/README.md has the measurements). The count
leaves out the kernel and cache misses, so it complements bench/run.sh.

The counts depend on the compiler (pinned in bench/instr/rust-toolchain.toml),
valgrind's version and the io_uring features the kernel offers; the
baseline records all three and a check refuses to compare across a change.
"""

import argparse
import csv
import io
import os
import platform
import re
import statistics
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATE = os.path.join(ROOT, "bench", "instr")
BASELINE = os.path.join(CRATE, "baseline.csv")
DRIVERS = ["io_uring", "epoll"]
ITERATIONS = 2000
WARMUP = 500
IDLE_CONNECTIONS = 2000
# Tolerances (percent) for metrics that have none in the baseline yet.
DEFAULT_TOLERANCE = {
    "instructions": 1.0,
    "allocations": 1.0,
    "heap_bytes": 2.0,
    "rss_bytes": 10.0,
}


def run(cmd, env=None, cwd=None):
    return subprocess.run(cmd, env=env, cwd=cwd, check=True, capture_output=True, text=True).stdout


def toolchain():
    with open(os.path.join(CRATE, "rust-toolchain.toml")) as f:
        return re.search(r'channel\s*=\s*"([^"]+)"', f.read()).group(1)


def build():
    if os.environ.get("BENCH_INSTR"):
        return os.environ["BENCH_INSTR"]
    print("== building bench/instr", file=sys.stderr)
    subprocess.run(["cargo", "build", "--release", "--locked"], cwd=CRATE, check=True)
    return os.path.join(CRATE, "target", "release", "bench-instr")


def environment(binary):
    env = {
        "rustc": run(["rustc", "--version"], cwd=CRATE).strip(),
        "valgrind": run(["valgrind", "--version"]).strip(),
        "kernel": platform.release(),
    }
    for d in DRIVERS:
        env[f"driver {d}"] = run([binary, "info"], env={**os.environ, "GIONG_DRIVER": d}).strip()
    try:
        with open("/proc/cpuinfo") as f:
            env["cpu"] = re.search(r"model name\s*:\s*(.*)", f.read()).group(1)
    except (OSError, AttributeError):
        env["cpu"] = "unknown"
    return env


def cases(binary):
    names = [line.split()[0] for line in run([binary, "list"]).splitlines()]
    return [n for n in names if not n.startswith("idle-")], [n for n in names if n.startswith("idle-")]


def parse_fields(line):
    return dict(kv.split("=", 1) for kv in line.split() if "=" in kv)


def measure_case(binary, driver, case, tmp):
    for f in os.listdir(tmp):
        os.remove(os.path.join(tmp, f))
    env = {**os.environ, "GIONG_DRIVER": driver}
    out = subprocess.run(
        ["valgrind", "--tool=callgrind", f"--callgrind-out-file={tmp}/cg.%p",
         binary, "run", case, str(ITERATIONS), str(WARMUP)],
        env=env, capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.exit(f"{case} on {driver} failed:\n{out.stderr[-3000:]}")
    fields = parse_fields(next(l for l in out.stdout.splitlines() if l.startswith(f"case={case} ")))
    dumps = [f for f in os.listdir(tmp) if re.fullmatch(r"cg\.\d+\.1", f)]
    if len(dumps) != 1:
        sys.exit(f"{case} on {driver}: expected one callgrind dump, found {dumps}")
    with open(os.path.join(tmp, dumps[0])) as f:
        text = f.read()
    if f"Client Request: {case}" not in text:
        sys.exit(f"{case} on {driver}: the dump is not the case's")
    total = int(re.search(r"^totals:\s+(\d+)", text, re.M).group(1))
    return {
        "instructions": total / ITERATIONS,
        "allocations": float(fields["allocations_per_op"]),
    }


def measure_idle(binary, driver, case):
    env = {**os.environ, "GIONG_DRIVER": driver}
    # 2,000 sockets on each side of the loopback connections.
    out = subprocess.run(
        ["bash", "-c", f'ulimit -n 8192 && exec "$0" idle {case} {IDLE_CONNECTIONS}', binary],
        env=env, capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.exit(f"{case} on {driver} failed:\n{out.stderr[-3000:]}")
    fields = parse_fields(next(l for l in out.stdout.splitlines() if l.startswith(f"case={case} ")))
    return {
        "heap_bytes": float(fields["heap_bytes_per_conn"]),
        "rss_bytes": float(fields["rss_bytes_per_conn"]),
        "allocations": float(fields["allocations_per_conn"]),
    }


def measure(args):
    binary = build()
    env = environment(binary)
    ops, idle = cases(binary)
    if args.cases:
        wanted = set(args.cases.split(","))
        ops = [c for c in ops if c in wanted]
        idle = [c for c in idle if c in wanted]
    raw = []  # (round, driver, case, metric, value)
    with tempfile.TemporaryDirectory() as tmp:
        for r in range(1, args.rounds + 1):
            for driver in DRIVERS:
                for case in ops:
                    for metric, value in measure_case(binary, driver, case, tmp).items():
                        raw.append((r, driver, case, metric, value))
                for case in idle:
                    for metric, value in measure_idle(binary, driver, case).items():
                        raw.append((r, driver, case, metric, value))
            print(f"== round {r} of {args.rounds} done", file=sys.stderr)
    medians = {}
    for key in dict.fromkeys((d, c, m) for _, d, c, m, _ in raw):
        medians[key] = statistics.median(v for _, d, c, m, v in raw if (d, c, m) == key)
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "raw.csv"), "w", newline="") as f:
        header(f, env, args)
        w = csv.writer(f, lineterminator="\n")
        w.writerow(["round", "driver", "case", "metric", "value"])
        for row in raw:
            w.writerow(row[:4] + (f"{row[4]:.3f}",))
    return env, medians, raw


def header(f, env, args):
    f.write(f"# scripts/instr.py, {args.rounds} rounds, {ITERATIONS} operations after "
            f"{WARMUP} of warm-up, {IDLE_CONNECTIONS} idle connections\n")
    for k, v in env.items():
        f.write(f"# {k}: {v}\n")


def read_baseline(path=BASELINE):
    env, rows = {}, {}
    if not os.path.exists(path):
        return env, rows
    with open(path) as f:
        lines = f.read().splitlines()
    for line in lines:
        m = re.match(r"# ([^:]+): (.*)", line)
        if m:
            env[m.group(1)] = m.group(2)
    for row in csv.DictReader(io.StringIO("\n".join(l for l in lines if not l.startswith("#")))):
        rows[(row["driver"], row["case"], row["metric"])] = (float(row["value"]), float(row["tolerance_pct"]))
    return env, rows


def write_baseline(path, env, medians, old, args):
    with open(path, "w", newline="") as f:
        header(f, env, args)
        f.write("# The median of the rounds; a measurement more than tolerance_pct away from\n"
                "# value, either way, fails `scripts/instr.py check`. Raw rounds:\n"
                "# bench/results/instr/raw.csv. Refresh: bench/instr/README.md.\n")
        w = csv.writer(f, lineterminator="\n")
        w.writerow(["driver", "case", "metric", "value", "tolerance_pct"])
        for (d, c, m), v in medians.items():
            tol = old.get((d, c, m), (None, DEFAULT_TOLERANCE[m]))[1]
            w.writerow([d, c, m, f"{v:.1f}" if m != "allocations" else f"{v:.3f}", f"{tol:g}"])


# What must match for two counts to be comparable. The kernel version and
# the CPU model are recorded but not compared: what of them matters is the
# driver's io_uring features (compared) and the code paths glibc and ring
# pick by CPU feature, which move the counts by at most 0.25% (README).
COMPARABLE = ["rustc", "valgrind"] + [f"driver {d}" for d in DRIVERS]


def check(args):
    base_env, base = read_baseline()
    if not base:
        sys.exit(f"no baseline at {BASELINE}; run scripts/instr.py update")
    env, medians, _ = measure(args)
    write_baseline(os.path.join(args.out, "baseline.csv"), env, medians, base, args)
    lines = ["| driver | case | metric | baseline | now | change | limit | |",
             "|---|---|---|---:|---:|---:|---:|---|"]
    failures, mismatched = [], []
    for k in COMPARABLE:
        if base_env.get(k) != env.get(k):
            mismatched.append(f"{k}: baseline `{base_env.get(k)}`, now `{env.get(k)}`")
    for key, now in medians.items():
        if key not in base:
            failures.append(f"{'/'.join(key)} has no baseline")
            continue
        value, tol = base[key]
        change = (now - value) / value * 100 if value else (0.0 if now == 0 else float("inf"))
        ok = abs(change) <= tol
        if not ok:
            failures.append(f"{key[1]} ({key[0]}) {key[2]}: {value:g} -> {now:.1f} ({change:+.2f}%, limit {tol:g}%)")
        lines.append(f"| {key[0]} | {key[1]} | {key[2]} | {value:g} | {now:.1f} | {change:+.2f}% | "
                     f"±{tol:g}% | {'ok' if ok else '**FAIL**'} |")
    for key in base:
        if key not in medians and not args.cases:
            failures.append(f"{'/'.join(key)} is in the baseline but was not measured")
    report = ["## Instructions per operation and memory per idle connection", ""]
    if mismatched:
        report += ["The baseline was measured with another setup, so the counts cannot be compared:", ""]
        report += [f"- {m}" for m in mismatched] + [""]
    report += lines + [""]
    if failures or mismatched:
        report += [
            "Outside the limits. If the change is meant to cost or save this much, take the "
            "new baseline this job measured (the `instr` artifact, `baseline.csv`, also printed "
            "below) and commit it as `bench/instr/baseline.csv` in the same pull request, saying "
            "why in its description. Otherwise, find what made the hot path slower: "
            "`callgrind_annotate` on a dump from `bench/instr` (README) shows where.", ""]
    text = "\n".join(report)
    print(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(text + "\n")
    if failures or mismatched:
        print("\n".join(failures), file=sys.stderr)
        print("\n== the baseline measured now:\n", file=sys.stderr)
        with open(os.path.join(args.out, "baseline.csv")) as f:
            print(f.read(), file=sys.stderr)
        sys.exit(1)


def update(args):
    if args.cases:
        sys.exit("update measures every case; leave out --cases")
    _, old = read_baseline()
    env, medians, _ = measure(args)
    write_baseline(BASELINE, env, medians, old, args)
    results = os.path.join(ROOT, "bench", "results", "instr")
    os.makedirs(results, exist_ok=True)
    os.replace(os.path.join(args.out, "raw.csv"), os.path.join(results, "raw.csv"))
    print(f"wrote {os.path.relpath(BASELINE, ROOT)} and bench/results/instr/raw.csv", file=sys.stderr)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("command", choices=["check", "update", "measure"])
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--out", default=os.path.join(ROOT, "target", "instr"))
    p.add_argument("--cases", default="")
    args = p.parse_args()
    want = toolchain()
    have = run(["rustc", "--version"], cwd=CRATE)
    if f"rustc {want} " not in have:
        sys.exit(f"bench/instr is pinned to Rust {want} (rust-toolchain.toml) but rustc there is "
                 f"{have.strip()}: rustup toolchain install {want}")
    if args.command == "check":
        check(args)
    elif args.command == "update":
        update(args)
    else:
        env, medians, _ = measure(args)
        for (d, c, m), v in medians.items():
            print(f"{d:9} {c:16} {m:13} {v:12.1f}")


if __name__ == "__main__":
    main()
