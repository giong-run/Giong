#!/usr/bin/env python3
"""The benchmark lab: giong against Tokio, Monoio and Glommio, one method for
all, raw data kept, losses published. bench/lab/README.md says how to read
the results.

    bench/lab/lab.py [--rounds 3] [--only cores,payload] [--runtimes giong,tokio]
                     [--out results/lab/<machine>] [--quick]

Every runtime serves the same program (bench/giong, bench/tokio-raw,
bench/monoio, bench/glommio: same parser, same buffers, same routes), started
with one worker per CPU it is pinned to. The load generator is
bench/lab/loadgen (raw epoll and threads, none of the runtimes measured),
pinned to other CPUs. Within each scenario point the runtimes run in
interleaved rounds, so that drift in the machine's speed hits all of them;
the summary reports medians with min and max.

Scenarios (MATRIX below lists every row and where it can run):
  cores     closed loop, /json, server on 1..N cores: requests per second and
            server CPU time per request
  conns     open loop at a fixed rate over 100..100k connections: p50..p99.9
  memory    connections held open and idle: server memory per connection
  payload   closed loop, answers of 1 KB..1 MB and uploads of 1 KB..1 MB
  mixed     open loop, 5% of requests compute for ~1 ms (fib), the rest are
            /json: the latency of the short requests next to long ones
  overload  open loop at more than any runtime can answer: goodput, latency,
            errors and peak memory, then whether latency recovers
  loss      open loop with 0.1% and 1% of packets dropped on loopback
            (iptables, needs root): the latency tail under retransmission
  latency   injected delay and jitter (tc netem): needs a kernel with sch_netem
  soak      hours of steady load (--only soak): memory and latency drift
"""

import argparse
import csv
import json
import os
import platform
import resource
import socket
import statistics
import subprocess
import sys
import threading
import time

LAB = os.path.dirname(os.path.abspath(__file__))
BENCH = os.path.dirname(LAB)
ROOT = os.path.dirname(BENCH)
LOADGEN = os.path.join(LAB, "loadgen", "target", "release", "lab-loadgen")

RUNTIMES = {
    # name: (crate dir, binary, port, extra environment)
    "giong": ("giong", "bench-giong", 3006, {}),
    "giong-epoll": ("giong", "bench-giong", 3006, {"GIONG_DRIVER": "epoll"}),
    "tokio": ("tokio-raw", "bench-tokio-raw", 3005, {}),
    "monoio": ("monoio", "bench-monoio", 3007, {}),
    "glommio": ("glommio", "bench-glommio", 3008, {}),
}

# Every row of the matrix, and what it needs. "cloud": runs on a cloud VM of
# 4 vCPUs (this lab's baseline). "bare-metal": needs more cores than a small
# VM has, or a quiet machine (the lab runs it when the machine has them).
# "netem": needs tc netem (sch_netem in the kernel). "root": needs iptables.
MATRIX = [
    ("cores", "1 core", "cloud"),
    ("cores", "2 cores", "cloud"),
    ("cores", "4, 8, 16+ cores", "bare-metal (needs 2N CPUs: N for the server, N for the load)"),
    ("conns", "100, 1k, 8k connections", "cloud (the VM allows 20,000 file descriptors)"),
    ("conns", "30k, 100k connections", "bare-metal (needs 2N file descriptors)"),
    ("memory", "1k, 8k idle connections", "cloud"),
    ("memory", "30k, 100k idle connections", "bare-metal (needs 2N file descriptors)"),
    ("payload", "answers 1 KB, 16 KB, 64 KB, 1 MB; uploads 1 KB, 64 KB, 1 MB", "cloud"),
    ("mixed", "5% compute (~1 ms) among /json, 1 and 2 cores", "cloud"),
    ("overload", "open loop past capacity, then recovery", "cloud"),
    ("loss", "0.1% and 1% packet loss", "root (iptables)"),
    ("latency", "delay 1 ms +- 0.5 ms, spikes", "netem (not on the cloud VM: no sch_netem)"),
    ("soak", "hours at steady load", "bare-metal or a dedicated VM (long)"),
]


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw).stdout


def raise_nofile():
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (hard, hard))
    return hard


def build(runtimes):
    sh(["cargo", "build", "--release", "-q"], cwd=os.path.join(LAB, "loadgen"))
    for d in sorted({RUNTIMES[r][0] for r in runtimes}):
        sh(["cargo", "build", "--release", "-q"], cwd=os.path.join(BENCH, d))


def wait_port(port, timeout=10.0):
    start = time.time()
    while time.time() - start < timeout:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return True
        except OSError:
            time.sleep(0.05)
    return False


def proc_cpu(pid):
    """User + system CPU seconds of a process (all its threads)."""
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    tick = os.sysconf("SC_CLK_TCK")
    return (int(fields[11]) + int(fields[12])) / tick


def proc_mem(pid):
    out = {}
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            k, _, v = line.partition(":")
            if k in ("VmRSS", "VmHWM"):
                out[k] = int(v.split()[0])  # KiB
    return out


class Server:
    def __init__(self, name, cpus):
        d, binary, port, env = RUNTIMES[name]
        self.name, self.port = name, port
        e = {**os.environ, **env}
        self.p = subprocess.Popen(
            ["taskset", "-c", cpus, os.path.join(BENCH, d, "target", "release", binary)],
            env=e, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if not wait_port(port):
            self.stop()
            raise RuntimeError(f"{name} did not start")

    def stop(self):
        self.p.terminate()
        try:
            self.p.wait(5)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()
        # The port must be free before the next server binds it.
        time.sleep(0.3)


def loadgen(port, load_cpus, threads, args, duration, warmup, server=None):
    """Runs the load generator; returns its JSON plus the server's CPU time
    and memory during the measurement."""
    cmd = ["taskset", "-c", load_cpus, LOADGEN, "--addr", f"127.0.0.1:{port}",
           "--threads", str(threads), "--duration", str(duration), "--warmup", str(warmup)] + args
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    cpu = {}
    if server:
        def sample():
            time.sleep(warmup)
            try:
                cpu["start"] = proc_cpu(server.p.pid)
                time.sleep(duration)
                cpu["end"] = proc_cpu(server.p.pid)
                cpu["mem"] = proc_mem(server.p.pid)
            except (FileNotFoundError, ProcessLookupError):
                pass
        t = threading.Thread(target=sample)
        t.start()
    out, err = p.communicate(timeout=duration + warmup + 120)
    if server:
        t.join()
    if p.returncode != 0:
        raise RuntimeError(f"loadgen failed: {err[-500:]}")
    r = json.loads(out.strip().splitlines()[-1])
    if "end" in cpu:
        r["server_cpu_s"] = round(cpu["end"] - cpu["start"], 3)
        r["server_rss_kib"] = cpu["mem"].get("VmRSS")
        r["server_hwm_kib"] = cpu["mem"].get("VmHWM")
        if r["requests"]:
            r["cpu_us_per_req"] = round(r["server_cpu_s"] / r["requests"] * 1e6, 3)
    return r


def hold(port, load_cpus, conns, server, src_ips):
    """Opens `conns` connections, one request answered on each, then idle;
    returns the server's memory per connection."""
    time.sleep(0.5)
    before = proc_mem(server.p.pid)["VmRSS"]
    threads = 2 if conns >= 1000 else 1
    p = subprocess.Popen(
        ["taskset", "-c", load_cpus, LOADGEN, "--addr", f"127.0.0.1:{port}", "--hold",
         "--conns", str(conns), "--threads", str(threads), "--src-ips", str(src_ips)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    line = p.stdout.readline().split()
    held = int(line[1]) if len(line) > 1 and line[0] == "HELD" else 0
    time.sleep(1.0)
    after = proc_mem(server.p.pid)["VmRSS"]
    p.stdin.close()
    p.wait(30)
    return {
        "conns": conns, "held": held, "rss_before_kib": before, "rss_held_kib": after,
        "kib_per_conn": round((after - before) / max(held, 1), 3),
    }


def iptables_loss(port, prob, add):
    op = "-A" if add else "-D"
    for d in ("--dport", "--sport"):
        subprocess.run(["iptables", op, "OUTPUT", "-o", "lo", "-p", "tcp", d, str(port),
                        "-m", "statistic", "--mode", "random", "--probability", str(prob),
                        "-j", "DROP"], check=add)


class Lab:
    def __init__(self, a):
        self.a = a
        self.out = a.out
        os.makedirs(os.path.join(self.out, "raw"), exist_ok=True)
        self.rows = []
        self.csv_path = os.path.join(self.out, "raw.csv")
        self.ncpu = len(os.sched_getaffinity(0))
        self.dur = 2 if a.quick else a.duration
        self.warm = 1 if a.quick else a.warmup

    def record(self, scenario, point, rt, rnd, r):
        name = f"{scenario}--{point}--{rt}--r{rnd}.json".replace("/", "_").replace(" ", "_")
        with open(os.path.join(self.out, "raw", name), "w") as f:
            json.dump(r, f, indent=1)
        lat = r.get("latency", {})
        row = {
            "scenario": scenario, "point": point, "runtime": rt, "round": rnd,
            "rps": r.get("rps"), "p50_us": lat.get("p50_us"), "p99_us": lat.get("p99_us"),
            "p999_us": lat.get("p999_us"), "max_us": lat.get("max_us"),
            "errors": r.get("errors"), "unfinished": r.get("unfinished"),
            "cpu_us_per_req": r.get("cpu_us_per_req"), "payload_mb_s": r.get("payload_mb_s"),
            "server_hwm_kib": r.get("server_hwm_kib"), "kib_per_conn": r.get("kib_per_conn"),
            "extra": r.get("extra", ""),
        }
        self.rows.append(row)
        new = not os.path.exists(self.csv_path)
        with open(self.csv_path, "a", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(row), lineterminator="\n")
            if new:
                w.writeheader()
            w.writerow(row)
        show = {k: v for k, v in row.items() if v not in (None, "") and k not in ("scenario", "round")}
        print(f"  {scenario} {show}", flush=True)

    def cpus(self, server_n):
        """CPU lists for a server on `server_n` CPUs and the load on the rest."""
        allc = sorted(os.sched_getaffinity(0))
        if len(allc) < server_n + 1:
            return None
        server = allc[-server_n:]
        load = allc[: len(allc) - server_n]
        return ",".join(map(str, server)), ",".join(map(str, load)), len(load)

    def each(self, scenario, point, server_n, fn):
        """Runs `fn(server, load_cpus, load_threads)` for every runtime, in
        interleaved rounds."""
        c = self.cpus(server_n)
        if c is None:
            print(f"  {scenario} {point}: skipped, needs {server_n + 1}+ CPUs (bare metal)")
            return
        scpu, lcpu, lthreads = c
        for rnd in range(1, self.a.rounds + 1):
            for rt in self.a.runtimes:
                s = Server(rt, scpu)
                try:
                    r = fn(s, lcpu, lthreads)
                finally:
                    s.stop()
                self.record(scenario, point, rt, rnd, r)

    # -------------------------------------------------------------- scenarios

    def cores(self):
        for n in (1, 2, 4, 8, 16):
            if self.ncpu < 2 * n:
                print(f"  cores {n}: needs {2 * n} CPUs, this machine has {self.ncpu} (bare metal)")
                continue
            self.each("cores", f"{n} core{'s' * (n > 1)}", n, lambda s, l, t, n=n: loadgen(
                s.port, l, t, ["--conns", str(64 * n)], self.dur, self.warm, s))

    def conns(self):
        rate = self.a.conns_rate
        for n in (100, 1000, 8000, 30000, 100000):
            if 2 * n + 1000 > raise_nofile():
                print(f"  conns {n}: needs {2 * n + 1000} file descriptors (bare metal)")
                continue
            src = max(1, n // 20000 + 1)
            self.each("conns", f"{n} conns at {rate} rps", 1, lambda s, l, t, n=n, src=src: loadgen(
                s.port, l, t, ["--conns", str(n), "--rate", str(rate), "--src-ips", str(src)],
                self.dur + 2, self.warm + 2, s))

    def memory(self):
        for n in (1000, 8000, 30000, 100000):
            if 2 * n + 1000 > raise_nofile():
                print(f"  memory {n}: needs {2 * n + 1000} file descriptors (bare metal)")
                continue
            self.each("memory", f"{n} idle conns", 1, lambda s, l, t, n=n: hold(
                s.port, l, n, s, max(1, n // 20000 + 1)))

    def payload(self):
        def run(s, l, t, args, size):
            r = loadgen(s.port, l, t, ["--conns", "16"] + args, self.dur, self.warm, s)
            # The payload carried per second, in either direction.
            r["payload_mb_s"] = round(r["rps"] * size / 1e6, 1)
            return r
        for size in (1024, 16384, 65536, 1 << 20):
            self.each("payload", f"answer {size // 1024} KB", 1, lambda s, l, t, size=size: run(
                s, l, t, ["--mix", f"/bytes?n={size}"], size))
        for size in (1024, 65536, 1 << 20):
            self.each("payload", f"upload {size // 1024} KB", 1, lambda s, l, t, size=size: run(
                s, l, t, ["--mix", "/upload", "--body", str(size)], size))

    def mixed(self):
        for n in (1, 2):
            rate = self.a.mixed_rate * n
            def run(s, l, t, rate=rate, n=n):
                r = loadgen(s.port, l, t, ["--conns", str(64 * n), "--rate", str(rate),
                                           "--mix", "/json@19,/compute?n=27@1"],
                            self.dur + 2, self.warm, s)
                j = next(p for p in r["paths"] if p["path"] == "/json")["latency"]
                r["extra"] = f"json p99 {j['p99_us']} us, p99.9 {j['p999_us']} us"
                r["latency"] = j  # the short requests are the point
                return r
            self.each("mixed", f"{n} core{'s' * (n > 1)} at {rate} rps", n, run)

    def overload(self):
        rate = self.a.overload_rate
        def run(s, l, t):
            r = loadgen(s.port, l, t, ["--conns", "1000", "--rate", str(rate)], self.dur + 3, self.warm, s)
            # Recovery: the same server at a rate it can answer.
            rec = loadgen(s.port, l, t, ["--conns", "100", "--rate", str(self.a.conns_rate)],
                          self.dur, 1, s)
            r["extra"] = (f"after: p99 {rec['latency']['p99_us']} us, p99.9 {rec['latency']['p999_us']} us,"
                          f" errors {rec['errors']}")
            r["recovery"] = rec
            return r
        self.each("overload", f"1000 conns offered {rate} rps", 1, run)

    def loss(self):
        if os.geteuid() != 0:
            print("  loss: needs root for iptables")
            return
        for prob in (0.001, 0.01):
            def run(s, l, t, prob=prob):
                iptables_loss(s.port, prob, True)
                try:
                    return loadgen(s.port, l, t, ["--conns", "100", "--rate", str(self.a.conns_rate)],
                                   self.dur + 4, self.warm, s)
                finally:
                    iptables_loss(s.port, prob, False)
            self.each("loss", f"{prob * 100:g}% loss at {self.a.conns_rate} rps", 1, run)

    def latency(self):
        ok = subprocess.run(["tc", "qdisc", "add", "dev", "lo", "root", "netem", "delay", "1ms"],
                            capture_output=True).returncode == 0
        if not ok:
            print("  latency: tc netem is not available here (needs sch_netem)")
            return
        subprocess.run(["tc", "qdisc", "del", "dev", "lo", "root"])
        for spec in (["delay", "1ms", "500us"], ["delay", "0ms", "10ms", "distribution", "pareto"]):
            def run(s, l, t, spec=spec):
                subprocess.run(["tc", "qdisc", "add", "dev", "lo", "root", "netem"] + spec, check=True)
                try:
                    return loadgen(s.port, l, t, ["--conns", "100", "--rate", str(self.a.conns_rate)],
                                   self.dur + 4, self.warm, s)
                finally:
                    subprocess.run(["tc", "qdisc", "del", "dev", "lo", "root"])
            self.each("latency", "netem " + " ".join(spec), 1, run)

    def soak(self):
        secs = self.a.soak_secs
        self.each("soak", f"{secs} s at {self.a.conns_rate} rps", 1, lambda s, l, t: loadgen(
            s.port, l, t, ["--conns", "1000", "--rate", str(self.a.conns_rate)], secs, 5, s))

    # ---------------------------------------------------------------- report

    def conditions(self):
        def cmd(c):
            try:
                return sh(c).strip()
            except Exception:
                return "unknown"
        with open(os.path.join(self.out, "conditions.txt"), "w") as f:
            f.write(f"date: {time.strftime('%Y-%m-%d %H:%M:%SZ', time.gmtime())}\n")
            f.write(f"kernel: {platform.release()}\n")
            cpu = cmd(["sh", "-c", "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2"])
            f.write(f"cpu: {cpu} ({self.ncpu} CPUs)\n")
            f.write(f"memory: {cmd(['sh', '-c', 'grep MemTotal /proc/meminfo'])}\n")
            f.write(f"rustc: {cmd(['rustc', '--version'])}\n")
            f.write(f"commit: {cmd(['git', '-C', ROOT, 'rev-parse', '--short', 'HEAD'])}\n")
            f.write(f"rounds: {self.a.rounds}, seconds per measurement: {self.dur} (+{self.warm} warm-up)\n")
            f.write(f"file descriptors: {raise_nofile()}\n")
            for d in ("tokio-raw", "monoio", "glommio"):
                lock = os.path.join(BENCH, d, "Cargo.lock")
                name = {"tokio-raw": "tokio"}.get(d, d)
                ver = cmd(["sh", "-c", f"grep -A1 '^name = \"{name}\"$' {lock} | sed -n 's/version = //p'"])
                f.write(f"{name}: {ver}\n")
            f.write(f"giong: this commit ({cmd(['sh', '-c', 'grep -m1 ^version ' + ROOT + '/Cargo.toml'])})\n")

    def summary(self):
        rows = list(csv.DictReader(open(self.csv_path)))
        keys = []
        for r in rows:
            k = (r["scenario"], r["point"])
            if k not in keys:
                keys.append(k)
        lines = ["# Benchmark lab results", "",
                 "Medians of all rounds (min to max in brackets). For each point, **bold** marks the best",
                 "runtime on that metric; the last column says whether giong (io_uring) is ahead, level",
                 "(within 3%) or behind the best of the others. Raw data: `raw.csv` and `raw/`.", "",
                 "On a shared cloud VM the tail (p99.9) moves by several times between rounds, as the",
                 "ranges show: a single stall of the VM lands there. Treat tail rows from a cloud VM as",
                 "indicative and decide on a bare-metal run.", ""]

        def med(vals):
            vals = [float(v) for v in vals if v not in ("", None)]
            if not vals:
                return None
            return statistics.median(vals), min(vals), max(vals)

        tail = [("p99_us", False), ("p999_us", False)]
        metric_of = {
            "cores": [("rps", True), ("cpu_us_per_req", False)], "payload": [("payload_mb_s", True)],
            "conns": tail, "memory": [("kib_per_conn", False)], "mixed": tail,
            "overload": [("rps", True), ("server_hwm_kib", False)], "loss": tail, "latency": tail,
            "soak": tail,
        }
        cols = [("rps", "req/s"), ("p50_us", "p50 us"), ("p99_us", "p99 us"), ("p999_us", "p99.9 us"),
                ("cpu_us_per_req", "CPU us/req"), ("payload_mb_s", "payload MB/s"), ("kib_per_conn", "KiB/conn"),
                ("server_hwm_kib", "peak KiB"), ("errors", "errors")]
        verdicts = []
        for scen, point in keys:
            sub = [r for r in rows if r["scenario"] == scen and r["point"] == point]
            rts = [rt for rt in self.a.runtimes if any(r["runtime"] == rt for r in sub)]
            used = [c for c in cols if any(r[c[0]] not in ("", None) for r in sub)]
            stats = {rt: {c: med([r[c] for r in sub if r["runtime"] == rt]) for c, _ in used} for rt in rts}
            best = {}
            for c, _ in used:
                vals = [(stats[rt][c][0], rt) for rt in rts if stats[rt][c]]
                if vals:
                    hi = c in ("rps", "payload_mb_s")
                    if len({v for v, _ in vals}) == 1:
                        continue
                    best[c] = (max if hi else min)(vals)[1]
            lines.append(f"## {scen}: {point}")
            lines.append("")
            lines.append("| runtime | " + " | ".join(n for _, n in used) + " |")
            lines.append("|---|" + "---:|" * len(used))
            for rt in rts:
                cells = []
                for c, _ in used:
                    s = stats[rt][c]
                    if not s:
                        cells.append("")
                        continue
                    m, lo, hi = s
                    txt = f"{m:,.1f}" if m < 100 else f"{m:,.0f}"
                    if lo != hi:
                        txt += f" ({lo:,.0f} to {hi:,.0f})" if m >= 100 else f" ({lo:.1f} to {hi:.1f})"
                    if best.get(c) == rt and len(rts) > 1:
                        txt = f"**{txt}**"
                    cells.append(txt)
                lines.append(f"| {rt} | " + " | ".join(cells) + " |")
            extras = [r["extra"] for r in sub if r["extra"]]
            if extras:
                lines.append("")
                for rt in rts:
                    ex = [r["extra"] for r in sub if r["runtime"] == rt and r["extra"]]
                    if ex:
                        lines.append(f"- {rt}: " + "; ".join(ex))
            lines.append("")
            for metric, higher in metric_of[scen]:
                if "giong" not in rts or not stats["giong"].get(metric):
                    continue
                g = stats["giong"][metric][0]
                others = [(stats[rt][metric][0], rt) for rt in rts
                          if rt not in ("giong", "giong-epoll") and stats[rt].get(metric)]
                if others:
                    o, ort = (max if higher else min)(others)
                    ratio = g / o if o else float("inf")
                    better = ratio > 1.03 if higher else ratio < 0.97
                    worse = ratio < 0.97 if higher else ratio > 1.03
                    word = "ahead" if better else "behind" if worse else "level"
                    verdicts.append((scen, point, metric, g, ort, o, ratio, word))
                head = ["## Where giong stands", "",
                "| scenario | point | metric | giong | best other | ratio | |", "|---|---|---|---:|---|---:|---|"]
        for scen, point, metric, g, ort, o, ratio, word in verdicts:
            head.append(f"| {scen} | {point} | {metric} | {g:,.1f} | {ort} {o:,.1f} | {ratio:.2f} | {word} |")
        head.append("")
        head.append("Higher is better for req/s and MB/s, lower for latency and memory.")
        head.append("")
        lines[10:10] = head
        lines += ["## The matrix", "", "| scenario | rows | where |", "|---|---|---|"]
        lines += [f"| {s} | {r} | {w} |" for s, r, w in MATRIX]
        with open(os.path.join(self.out, "summary.md"), "w") as f:
            f.write("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--duration", type=float, default=6)
    ap.add_argument("--warmup", type=float, default=2)
    ap.add_argument("--only", default="cores,conns,memory,payload,mixed,overload,loss,latency")
    ap.add_argument("--runtimes", default="giong,giong-epoll,tokio,monoio,glommio")
    ap.add_argument("--out", default=os.path.join(BENCH, "results", "lab", "cloud-4vcpu"))
    ap.add_argument("--conns-rate", type=int, default=20000, help="open-loop rate for conns, loss, latency")
    ap.add_argument("--mixed-rate", type=int, default=4000, help="open-loop rate per core for mixed")
    ap.add_argument("--overload-rate", type=int, default=600000)
    ap.add_argument("--soak-secs", type=int, default=3600)
    ap.add_argument("--quick", action="store_true", help="short measurements, to try the lab")
    ap.add_argument("--summary-only", action="store_true")
    a = ap.parse_args()
    a.runtimes = a.runtimes.split(",")
    for r in a.runtimes:
        if r not in RUNTIMES:
            sys.exit(f"unknown runtime {r}; known: {', '.join(RUNTIMES)}")
    lab = Lab(a)
    if not a.summary_only:
        raise_nofile()
        build(a.runtimes)
        lab.conditions()
        for scen in a.only.split(","):
            print(f"== {scen}", flush=True)
            getattr(lab, scen)()
    lab.summary()
    print(open(os.path.join(a.out, "summary.md")).read())


if __name__ == "__main__":
    main()
