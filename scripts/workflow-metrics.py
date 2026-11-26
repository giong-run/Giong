#!/usr/bin/env python3
"""How long changes take to land: the numbers behind docs/WORKFLOW.md.

    python3 scripts/workflow-metrics.py fetch --since 2026-11-25T03:16:00Z \\
        --until 2026-11-26T13:19:00Z --out docs/workflow/data/v1
    python3 scripts/workflow-metrics.py report docs/workflow/data/v1 [--csv FILE]

`fetch` saves GitHub's answers as they came (merged pull requests, each one's timeline,
every Actions run of the window and its jobs) under --out, gzipped, so that `report` can
be run again, changed or checked later without the API. It reads GITHUB_TOKEN when set.

`report` prints, for the pull requests merged into main in the window:
  - ready -> merged: from the moment the pull request was ready for review (opened ready,
    or marked ready the last time before it merged) to the merge;
  - ready -> first enqueue: waiting for the pull request's own CI (and anything else that
    held it back, such as an open review thread);
  - time in the merge queue, added up over every entry, and the number of entries;
  - ejections: a removal from the queue that was not the merge, with the failed jobs of
    the queue run that tested the pull request just before;
  - runner time: the sum of every job's duration over every run for the pull request's
    branch (pull_request events) and its merge-queue groups, split into Linux, macOS and
    Windows (a macOS minute costs ten Linux ones on GitHub's price list, Windows two).
It also prints how the runs ended (cancelled by a newer push, failed, passed) and the
queue's throughput. Medians and the 90th percentile; no number is extrapolated.
"""
import argparse
import concurrent.futures
import csv
import gzip
import json
import os
import re
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

API = "https://api.github.com"
REPO = "giong-run/giong"
QUEUE_BRANCH = re.compile(r"^gh-readonly-queue/main/pr-(\d+)-[0-9a-f]+$")


def ts(s):
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc) if s else None


def iso(d):
    return d.strftime("%Y-%m-%dT%H:%M:%SZ")


# ---------------------------------------------------------------- fetch


def get(path, params=None):
    url = path if path.startswith("http") else f"{API}{path}"
    if params:
        url += ("&" if "?" in url else "?") + urllib.parse.urlencode(params)
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "workflow-metrics"}
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    for attempt in range(6):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60) as r:
                link = r.headers.get("Link", "")
                return json.load(r), link
        except urllib.error.HTTPError as e:
            if e.code == 404:
                raise
            time.sleep(2 ** attempt)
        except (OSError, ValueError):
            # A reset connection or a cut-off answer: try again.
            time.sleep(2 ** attempt)
    raise RuntimeError(f"giving up on {url}")


def get_all(path, params=None, key=None):
    out = []
    url, first = path, True
    while url:
        data, link = get(url, params if first else None)
        first = False
        out.extend(data[key] if key else data)
        m = re.search(r'<([^>]+)>;\s*rel="next"', link)
        url = m.group(1) if m else None
    return out


# The fields `report` reads, and enough around them to check a number by hand; the rest
# of GitHub's answers (URLs, repository objects, every step of every job) would make the
# committed data ten times larger.
RUN_KEYS = ("id", "name", "event", "head_branch", "head_sha", "status", "conclusion",
            "run_attempt", "run_started_at", "created_at", "updated_at", "workflow_id")
JOB_KEYS = ("id", "run_id", "run_attempt", "name", "status", "conclusion", "started_at",
            "completed_at", "created_at", "labels", "runner_name")
PR_KEYS = ("number", "title", "created_at", "merged_at", "closed_at", "draft")
EVENT_KEYS = ("event", "created_at", "sha")


def trim_run(r):
    return {k: r.get(k) for k in RUN_KEYS}


def trim_job(j):
    out = {k: j.get(k) for k in JOB_KEYS}
    out["failed_steps"] = [s["name"] for s in j.get("steps") or []
                           if s.get("conclusion") in ("failure", "timed_out")]
    return out


def trim_pr(p):
    out = {k: p.get(k) for k in PR_KEYS}
    out["user"] = {"login": (p.get("user") or {}).get("login")}
    out["head"] = {"ref": p["head"]["ref"], "sha": p["head"]["sha"]}
    out["base"] = {"ref": p["base"]["ref"]}
    return out


def trim_event(e):
    return {k: e[k] for k in EVENT_KEYS if k in e}


def save(path, obj):
    path.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(path, "wt", encoding="utf-8") as f:
        json.dump(obj, f, separators=(",", ":"), sort_keys=True)


def load(path):
    with gzip.open(path, "rt", encoding="utf-8") as f:
        return json.load(f)


def fetch(args):
    out = Path(args.out)
    since, until = ts(args.since), ts(args.until)
    # Merged pull requests: closed ones, newest update first, until updates predate the window.
    prs = []
    page = 1
    while True:
        batch, _ = get(f"/repos/{REPO}/pulls", {"state": "closed", "sort": "updated",
                                                 "direction": "desc", "per_page": 100, "page": page})
        if not batch:
            break
        for p in batch:
            m = ts(p.get("merged_at"))
            if m and since <= m <= until and p["base"]["ref"] == "main":
                prs.append(p)
        if ts(batch[-1]["updated_at"]) < since:
            break
        page += 1
    prs.sort(key=lambda p: p["merged_at"])
    save(out / "prs.json.gz", [trim_pr(p) for p in prs])
    print(f"{len(prs)} pull requests merged", file=sys.stderr)

    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        timelines = dict(zip([p["number"] for p in prs], pool.map(
            lambda p: get_all(f"/repos/{REPO}/issues/{p['number']}/timeline", {"per_page": 100}), prs)))
    save(out / "timelines.json.gz", {str(k): [trim_event(e) for e in v] for k, v in timelines.items()})

    # Runs, in slices of a few hours: the API lists at most 1,000 per query. Starting
    # a day before the window catches the runs of pull requests opened before it.
    runs, seen = [], set()
    start = since - timedelta(days=args.lead_days)
    step = timedelta(hours=2)
    t = start
    while t < until + timedelta(hours=1):
        q = f"{iso(t)}..{iso(t + step)}"
        for r in get_all(f"/repos/{REPO}/actions/runs", {"created": q, "per_page": 100}, "workflow_runs"):
            if r["id"] not in seen:
                seen.add(r["id"])
                runs.append(r)
        t += step
    save(out / "runs.json.gz", [trim_run(r) for r in runs])
    print(f"{len(runs)} runs", file=sys.stderr)

    # Jobs of every attempt of every run, except the gate's and other one-job bookkeeping
    # runs, which take seconds.
    wanted = [r for r in runs if r["name"] not in ("CI gate", "Review threads")]

    def jobs_of(r):
        try:
            return r["id"], get_all(f"/repos/{REPO}/actions/runs/{r['id']}/jobs",
                                    {"filter": "all", "per_page": 100}, "jobs")
        except urllib.error.HTTPError:
            return r["id"], []

    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        jobs = dict(pool.map(jobs_of, wanted))
    save(out / "jobs.json.gz", {str(k): [trim_job(j) for j in v] for k, v in jobs.items()})
    meta = {"repo": REPO, "since": args.since, "until": args.until,
            "fetched": iso(datetime.now(timezone.utc)), "lead_days": args.lead_days}
    (out / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")
    print(f"saved to {out}", file=sys.stderr)


# ---------------------------------------------------------------- report


def os_of(job):
    labels = " ".join(job.get("labels") or []).lower()
    name = (job.get("runner_name") or "").lower()
    if "macos" in labels or "mac" in name:
        return "macos"
    if "windows" in labels or "windows" in name:
        return "windows"
    return "linux"


def job_seconds(job):
    a, b = ts(job.get("started_at")), ts(job.get("completed_at"))
    if not a or not b or b < a:
        return 0
    return (b - a).total_seconds()


def q(values, p):
    if not values:
        return None
    v = sorted(values)
    return v[min(len(v) - 1, int(round(p * (len(v) - 1))))]


def fmt_min(s):
    return "-" if s is None else f"{s / 60:.0f} min"


def fmt_h(s):
    return "-" if s is None else f"{s / 3600:.1f} h"


def analyse(data_dir):
    d = Path(data_dir)
    meta = json.loads((d / "meta.json").read_text())
    prs = load(d / "prs.json.gz")
    timelines = load(d / "timelines.json.gz")
    runs = load(d / "runs.json.gz")
    jobs = load(d / "jobs.json.gz")

    by_branch, by_queue_pr = {}, {}
    for r in runs:
        m = QUEUE_BRANCH.match(r.get("head_branch") or "")
        if r["event"] == "merge_group" and m:
            by_queue_pr.setdefault(int(m.group(1)), []).append(r)
        elif r["event"] == "pull_request":
            by_branch.setdefault(r.get("head_branch"), []).append(r)

    rows = []
    for p in prs:
        n = p["number"]
        ev = timelines.get(str(n), [])
        merged = ts(p["merged_at"])
        created = ts(p["created_at"])
        readies = [ts(e["created_at"]) for e in ev if e.get("event") == "ready_for_review"]
        drafts = [ts(e["created_at"]) for e in ev if e.get("event") == "convert_to_draft"]
        # Ready since: the last ready_for_review before the merge, or the opening when it
        # was opened ready and never converted.
        ready = max([t for t in readies if t <= merged], default=None)
        if ready is None:
            ready = created
        adds = sorted(ts(e["created_at"]) for e in ev if e.get("event") == "added_to_merge_queue")
        removes = sorted(ts(e["created_at"]) for e in ev if e.get("event") == "removed_from_merge_queue")
        # Pair each add with the next removal.
        in_queue, ejections, ri = 0.0, [], 0
        for a in adds:
            while ri < len(removes) and removes[ri] < a:
                ri += 1
            if ri < len(removes):
                rm = removes[ri]
                in_queue += (rm - a).total_seconds()
                if abs((rm - merged).total_seconds()) > 5:
                    ejections.append((a, rm))
                ri += 1
        first_add = next((a for a in adds if a >= ready), adds[0] if adds else None)
        pushes = sum(1 for e in ev if e.get("event") == "committed")
        pr_runs = by_branch.get(p["head"]["ref"], [])
        # Only runs of this pull request's life (a branch name can be reused).
        pr_runs = [r for r in pr_runs if created - timedelta(minutes=1) <= ts(r["created_at"]) <= merged]
        q_runs = [r for r in by_queue_pr.get(n, []) if ts(r["created_at"]) <= merged + timedelta(minutes=1)]
        secs = {"linux": 0.0, "macos": 0.0, "windows": 0.0}
        qsecs = {"linux": 0.0, "macos": 0.0, "windows": 0.0}
        for r in pr_runs:
            for j in jobs.get(str(r["id"]), []):
                secs[os_of(j)] += job_seconds(j)
        for r in q_runs:
            for j in jobs.get(str(r["id"]), []):
                qsecs[os_of(j)] += job_seconds(j)
        ci_pr = [r for r in pr_runs if r["name"] == "CI"]
        ci_q = [r for r in q_runs if r["name"] == "CI"]
        # Why each ejection happened: the failed jobs of queue runs for this pull request
        # that ended within ten minutes before the removal.
        causes = []
        for a, rm in ejections:
            failed = []
            for r in q_runs:
                end = ts(r["updated_at"])
                if r.get("conclusion") in ("failure", "timed_out", "cancelled") and \
                        a - timedelta(minutes=5) <= ts(r["created_at"]) and end <= rm + timedelta(minutes=10):
                    for j in jobs.get(str(r["id"]), []):
                        if j.get("conclusion") in ("failure", "timed_out"):
                            failed.append(f"{r['name']}/{j['name']}")
            causes.append(sorted(set(failed)) or ["(no failed queue job: dequeued by hand, a group ahead failed, or a push)"])
        rows.append({
            "pr": n, "title": p["title"], "author": (p.get("user") or {}).get("login"),
            "created": iso(created), "ready": iso(ready), "merged": iso(merged),
            "first_enqueue": iso(first_add) if first_add else "",
            "ready_to_merged_s": (merged - ready).total_seconds(),
            "created_to_merged_s": (merged - created).total_seconds(),
            "ready_to_enqueue_s": (first_add - ready).total_seconds() if first_add else None,
            "queue_s": in_queue, "queue_entries": len(adds), "ejections": len(ejections),
            "ejection_causes": causes, "pushes": pushes, "drafted": bool(readies or drafts),
            "ci_runs_pr": len(ci_pr),
            "ci_runs_pr_cancelled": sum(1 for r in ci_pr if r.get("conclusion") == "cancelled"),
            "ci_runs_queue": len(ci_q),
            "runner_s_pr": secs, "runner_s_queue": qsecs,
        })
    return meta, rows, runs, jobs


def report(args):
    meta, rows, runs, jobs = analyse(args.dir)
    out = []
    w = out.append
    w(f"# Workflow metrics: {meta['since']} to {meta['until']}\n")
    w(f"{len(rows)} pull requests merged into main (fetched {meta['fetched']}).\n")

    def line(label, vals, f=fmt_min):
        vals = [v for v in vals if v is not None]
        if not vals:
            w(f"| {label} | - | - | - | - |")
            return
        w(f"| {label} | {f(statistics.median(vals))} | {f(q(vals, 0.9))} | {f(max(vals))} | {f(sum(vals))} |")

    w("| Per pull request | median | p90 | max | total |")
    w("|---|---|---|---|---|")
    line("ready -> merged", [r["ready_to_merged_s"] for r in rows])
    line("ready -> first enqueue (own CI, review threads)", [r["ready_to_enqueue_s"] for r in rows])
    line("time in the merge queue (all entries)", [r["queue_s"] for r in rows])
    line("runner time, PR runs, Linux", [r["runner_s_pr"]["linux"] for r in rows], fmt_h)
    line("runner time, PR runs, macOS", [r["runner_s_pr"]["macos"] for r in rows], fmt_h)
    line("runner time, PR runs, Windows", [r["runner_s_pr"]["windows"] for r in rows], fmt_h)
    line("runner time, queue runs, Linux", [r["runner_s_queue"]["linux"] for r in rows], fmt_h)
    line("runner time, queue runs, macOS", [r["runner_s_queue"]["macos"] for r in rows], fmt_h)
    line("runner time, queue runs, Windows", [r["runner_s_queue"]["windows"] for r in rows], fmt_h)
    w("")
    ent = sum(r["queue_entries"] for r in rows)
    ej = sum(r["ejections"] for r in rows)
    w(f"- Queue entries: {ent} for {len(rows)} merges; ejections: {ej} "
      f"({sum(1 for r in rows if r['ejections'])} pull requests ejected at least once).")
    w(f"- CI runs on pull requests: {sum(r['ci_runs_pr'] for r in rows)}, of which "
      f"{sum(r['ci_runs_pr_cancelled'] for r in rows)} cancelled by a newer push; "
      f"CI runs in the queue: {sum(r['ci_runs_queue'] for r in rows)}.")
    w(f"- Pushes per pull request: median {statistics.median([r['pushes'] for r in rows]):.0f}, "
      f"max {max(r['pushes'] for r in rows)}.")
    tot_pr = sum(sum(r["runner_s_pr"].values()) for r in rows)
    tot_q = sum(sum(r["runner_s_queue"].values()) for r in rows)
    w(f"- Runner time for these pull requests: {fmt_h(tot_pr)} on pull request runs, "
      f"{fmt_h(tot_q)} in the queue.")

    # Queue throughput: merge times.
    merges = sorted(ts(r["merged"]) for r in rows)
    if len(merges) > 1:
        gaps = [(b - a).total_seconds() for a, b in zip(merges, merges[1:])]
        span = (merges[-1] - merges[0]).total_seconds()
        w(f"- Merges: {len(merges)} over {fmt_h(span)}; gap between merges median "
          f"{fmt_min(statistics.median(gaps))}, p90 {fmt_min(q(gaps, 0.9))}.")

    # Queue CI runs: duration and outcome.
    qci = [r for r in runs if r["event"] == "merge_group" and r["name"] == "CI"]
    dur = [(ts(r["updated_at"]) - ts(r["run_started_at"])).total_seconds() for r in qci
           if r.get("conclusion") == "success"]
    w(f"- Queue CI runs in the data: {len(qci)}; passed {sum(1 for r in qci if r.get('conclusion') == 'success')}, "
      f"failed {sum(1 for r in qci if r.get('conclusion') == 'failure')}, "
      f"cancelled {sum(1 for r in qci if r.get('conclusion') == 'cancelled')}; "
      f"a passing one took median {fmt_min(statistics.median(dur) if dur else None)}, p90 {fmt_min(q(dur, 0.9))}.")
    # Concurrency of queue runs: the most CI merge_group runs in progress at once.
    evs = []
    for r in qci:
        evs.append((ts(r["run_started_at"]), 1))
        evs.append((ts(r["updated_at"]), -1))
    cur = peak = 0
    for _, dlt in sorted(evs):
        cur += dlt
        peak = max(peak, cur)
    w(f"- Most queue CI runs in progress at once: {peak}.")
    prci = [r for r in runs if r["event"] == "pull_request" and r["name"] == "CI" and r.get("conclusion") == "success"]
    pdur = [(ts(r["updated_at"]) - ts(r["run_started_at"])).total_seconds() for r in prci]
    if pdur:
        w(f"- A passing pull request CI run took median {fmt_min(statistics.median(pdur))}, p90 {fmt_min(q(pdur, 0.9))}.")
    w("")
    w("## Ejections\n")
    w("| PR | removed | failed jobs of the queue run before |")
    w("|---|---|---|")
    for r in rows:
        for c in r["ejection_causes"]:
            w(f"| #{r['pr']} | | {'; '.join(c)} |")
    w("")
    w("## Slowest ten, ready -> merged\n")
    w("| PR | ready -> merged | own CI wait | in queue | entries | title |")
    w("|---|---|---|---|---|---|")
    for r in sorted(rows, key=lambda r: -r["ready_to_merged_s"])[:10]:
        w(f"| #{r['pr']} | {fmt_min(r['ready_to_merged_s'])} | {fmt_min(r['ready_to_enqueue_s'])} | "
          f"{fmt_min(r['queue_s'])} | {r['queue_entries']} | {r['title'][:60]} |")
    print("\n".join(out))

    if args.csv:
        with open(args.csv, "w", newline="") as f:
            cols = ["pr", "author", "created", "ready", "first_enqueue", "merged", "ready_to_merged_s",
                    "ready_to_enqueue_s", "queue_s", "queue_entries", "ejections", "pushes",
                    "ci_runs_pr", "ci_runs_pr_cancelled", "ci_runs_queue",
                    "runner_s_pr_linux", "runner_s_pr_macos", "runner_s_pr_windows",
                    "runner_s_queue_linux", "runner_s_queue_macos", "runner_s_queue_windows",
                    "ejection_causes", "title"]
            wr = csv.writer(f, lineterminator="\n")
            wr.writerow(cols)
            for r in rows:
                flat = dict(r)
                for k in ("linux", "macos", "windows"):
                    flat[f"runner_s_pr_{k}"] = round(r["runner_s_pr"][k])
                    flat[f"runner_s_queue_{k}"] = round(r["runner_s_queue"][k])
                flat["ejection_causes"] = " | ".join("; ".join(c) for c in r["ejection_causes"])
                for k in ("ready_to_merged_s", "ready_to_enqueue_s", "queue_s"):
                    flat[k] = "" if r[k] is None else round(r[k])
                wr.writerow([flat[c] for c in cols])


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fetch")
    f.add_argument("--since", required=True)
    f.add_argument("--until", required=True)
    f.add_argument("--out", required=True)
    f.add_argument("--lead-days", type=int, default=1)
    r = sub.add_parser("report")
    r.add_argument("dir")
    r.add_argument("--csv")
    args = ap.parse_args()
    {"fetch": fetch, "report": report}[args.cmd](args)


if __name__ == "__main__":
    main()
