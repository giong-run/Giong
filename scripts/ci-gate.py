#!/usr/bin/env python3
"""The CI gate: one commit status, "CI gate", that says whether a run of CI passed, and
that reruns only the jobs that failed for a reason outside the code.

    python3 scripts/ci-gate.py requested --run-id N
    python3 scripts/ci-gate.py completed --run-id N --attempt K
    python3 scripts/ci-gate.py classify --jobs JOBS.json [--annotations DIR] [--logs DIR]
        [--attempt K] [--flakes FILE] [--open-issues N,N]

.github/workflows/ci-gate.yml runs the first two when a run of CI starts and when it
ends. `classify` reads saved API answers and prints the decision, for the tests in
scripts/test_ci_gate.py and for looking at a past run by hand.

Why: GitHub's merge queue removes a pull request as soon as a required check fails, and
starts every group behind it again, and "Re-run failed jobs" only exists once a run has
ended, by which time the queue has moved on. One mirror that resets a connection used to
cost a whole queue run (three of them for release 1.4.1 on 2026-11-23). With this status
as the only required check, the queue waits while the gate reruns the failed jobs of the
same run, and only those (GitHub reruns the jobs that depend on them too, such as
`all-checks`); the jobs that passed keep their result.

What counts as outside the code, and so is rerun, at most twice per run:
  - a failed step whose name starts with "net: " (a download, an apt install, a cache:
    every such step goes through scripts/ci-net.sh or .github/actions/apt-install), or
    one of GitHub's own setup steps (checkout, the toolchain, caches, artifacts);
  - a job the runner lost (GitHub's annotation says it received a shutdown signal or
    lost communication), or that never got a runner;
  - a job that ran out of time inside such a step.

Everything else fails the gate at once: a test, a lint, a build, and a job that ran out
of time anywhere else (a hang is a bug). A failing test is never rerun to make it pass
(CONTRIBUTING.md), with one exception, the one the project's rules make: a known flake
that has an open issue with a fix on the way, listed in .github/known-flakes.txt. When
every failing test of every failed job is such a flake, the failed jobs are rerun once,
and the run's summary and the status name the issue.

The jobs that only collect others' results (`all-checks`, `fallback`, `tsan`, the macOS
and Windows `*-checks`) fail with them and are rerun with them; they decide nothing here.
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONTEXT = "CI gate"
# Attempts of one run, the first included: two reruns.
MAX_ATTEMPTS = 3

# Steps that GitHub or an action runs to set a job up or tear it down. A failure there
# is the platform's (a git fetch, a toolchain download, the cache or artifact service).
SETUP_STEPS = (
    "net: ",
    "Set up job",
    "Complete job",
    "Initialize containers",
    "Stop containers",
    "Run actions/checkout@",
    "Post Run actions/checkout@",
    "Run actions/cache@",
    "Post Run actions/cache@",
    "Run actions/cache/restore@",
    "Run actions/cache/save@",
    "Run actions/download-artifact@",
    "Run actions/upload-artifact@",
    "Run actions/setup-go@",
    "Post Run actions/setup-go@",
    "Run dtolnay/rust-toolchain@",
    "Run taiki-e/install-action@",
)

# Steps that only fail because a job they wait for failed: the aggregating jobs, and the
# first step of `test`, which checks that `plan` and `build` passed.
NEEDS_STEPS = (
    "Run jq -r 'to_entries",
    'Run test "',
    "The plan and the build must have succeeded",
)

TIMED_OUT = "exceeded the maximum execution time"
RUNNER_LOST = (
    "received a shutdown signal",
    "lost communication with the server",
    "runner has received",
    "did not respond",
    "could not be started",
)

# A libtest summary: "failures:" then one indented name per line.
LOG_PREFIX = re.compile(r"^\d{4}-\d\d-\d\dT[\d:.]+Z ")
FAILURE_NAME = re.compile(r"^    (\S+)$")


def step_kind(name):
    if name.startswith(NEEDS_STEPS):
        return "needs"
    if name.startswith(SETUP_STEPS):
        return "infra"
    return "code"


def failed_tests(log):
    """The names libtest listed under "failures:" (each suite prints the list once with
    its output and once without; a name is kept once)."""
    names = []
    listing = False
    for raw in log.splitlines():
        line = LOG_PREFIX.sub("", raw)
        if line.strip() == "failures:":
            listing = True
            continue
        if listing:
            m = FAILURE_NAME.match(line)
            if m and not line.strip().startswith("----"):
                if m.group(1) not in names:
                    names.append(m.group(1))
                continue
            listing = False
    return names


def read_flakes(path):
    """`.github/known-flakes.txt`: lines `ISSUE TEST-NAME-REGEX`, # starts a comment."""
    flakes = []
    try:
        text = Path(path).read_text()
    except FileNotFoundError:
        return flakes
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        issue, pattern = line.split(None, 1)
        flakes.append((int(issue), re.compile(pattern)))
    return flakes


def classify_job(job, annotations):
    """("needs" | "infra" | "code", why) for a job that did not succeed."""
    steps = job.get("steps") or []
    bad = [s for s in steps if s.get("conclusion") == "failure"]
    if bad:
        kinds = [(step_kind(s["name"]), s["name"]) for s in bad]
        code = [n for k, n in kinds if k == "code"]
        if code:
            return "code", f"step failed: {code[0]}"
        infra = [n for k, n in kinds if k == "infra"]
        if infra:
            return "infra", f"setup or network step failed: {infra[0]}"
        return "needs", "a job it waits for failed"
    if any(TIMED_OUT in a for a in annotations):
        stopped = [s for s in steps if s.get("conclusion") == "cancelled"]
        if stopped and step_kind(stopped[0]["name"]) == "infra":
            return "infra", f"ran out of time in a network step: {stopped[0]['name']}"
        where = stopped[0]["name"] if stopped else "an unknown step"
        return "code", f"ran out of time in {where} (a hang is a bug)"
    if any(lost in a for a in annotations for lost in RUNNER_LOST):
        return "infra", "the runner was lost"
    if not steps:
        return "infra", "never got a runner"
    return "code", "stopped without a failed step or a known reason"


def decide(jobs, attempt, annotations_of, log_of, flakes, issue_open):
    """What to do with a failed run: ("rerun" | "fail", summary, rows).

    annotations_of(job) and log_of(job) fetch what they name; issue_open(n) says whether
    issue n is open. rows are (job name, kind, why) for the summary."""
    rows = []
    code, infra = [], []
    for job in jobs:
        if job.get("conclusion") in (None, "success", "skipped", "neutral"):
            continue
        kind, why = classify_job(job, annotations_of(job))
        rows.append([job["name"], kind, why])
        if kind == "code":
            code.append((job, rows[-1]))
        elif kind == "infra":
            infra.append(job)
    if code:
        if attempt > 1:
            return "fail", "failed: " + ", ".join(r[0] for _, r in code), rows
        # A known flake, once: every failing test of every such job is listed and open.
        issues = set()
        for job, row in code:
            names = failed_tests(log_of(job))
            if not names:
                return "fail", f"failed: {row[0]}", rows
            for name in names:
                hit = [n for n, rx in flakes if rx.fullmatch(name) and issue_open(n)]
                if not hit:
                    return "fail", f"failed: {row[0]} ({name})", rows
                issues.update(hit)
            row[1] = "flake"
            row[2] += "; known flake " + ", ".join(f"#{n}" for n in sorted(issues))
        flaky = ", ".join(f"#{n}" for n in sorted(issues))
        return "rerun", f"known flake {flaky}: rerunning the failed jobs once", rows
    if infra:
        if attempt >= MAX_ATTEMPTS:
            return "fail", f"still failing outside the code after {attempt} attempts", rows
        return "rerun", f"{len(infra)} job(s) failed outside the code: rerunning only those", rows
    return "fail", "failed, and no job says why", rows


class Api:
    def __init__(self, repo, token):
        self.repo = repo
        self.token = token

    def call(self, method, path, body=None, raw=False):
        url = path if path.startswith("https://") else f"https://api.github.com/repos/{self.repo}/{path}"
        data = json.dumps(body).encode() if body is not None else None
        for attempt in range(5):
            req = urllib.request.Request(url, data=data, method=method)
            req.add_header("Accept", "application/vnd.github+json")
            req.add_header("X-GitHub-Api-Version", "2022-11-28")
            if self.token:
                req.add_header("Authorization", f"Bearer {self.token}")
            try:
                with urllib.request.urlopen(req, timeout=60) as resp:
                    out = resp.read()
                    if raw:
                        return out.decode("utf-8", "replace")
                    return json.loads(out) if out else None
            except urllib.error.HTTPError as e:
                if e.code in (404, 409, 422) or attempt == 4:
                    raise
            except (urllib.error.URLError, TimeoutError):
                if attempt == 4:
                    raise
            time.sleep(5 * 2**attempt)

    def jobs(self, run_id, attempt):
        out, page = [], 1
        while True:
            got = self.call("GET", f"actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100&page={page}")
            out += got["jobs"]
            if len(got["jobs"]) < 100:
                return out
            page += 1

    def annotations(self, job_id):
        try:
            return [a.get("message", "") for a in self.call("GET", f"check-runs/{job_id}/annotations")]
        except urllib.error.HTTPError:
            return []

    def log(self, job_id):
        try:
            return self.call("GET", f"actions/jobs/{job_id}/logs", raw=True)
        except urllib.error.HTTPError:
            return ""

    def issue_open(self, number):
        try:
            return self.call("GET", f"issues/{number}")["state"] == "open"
        except urllib.error.HTTPError:
            return False

    def status(self, sha, state, description, url):
        print(f"{CONTEXT}: {state}: {description}")
        self.call("POST", f"statuses/{sha}", {
            "state": state, "context": CONTEXT, "description": description[:140], "target_url": url,
        })


def summary(lines):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    text = "\n".join(lines) + "\n"
    print(text)
    if path:
        with open(path, "a") as f:
            f.write(text)


def still_current(api, run):
    """Whether rerunning this run can still matter: a merge-queue group still queued, a
    pull request whose head is still this commit. A rerun of an older pull-request run
    would join the newer run's concurrency group and cancel it."""
    event = run["event"]
    if event == "merge_group":
        try:
            api.call("GET", f"git/ref/heads/{run['head_branch']}")
            return True
        except urllib.error.HTTPError:
            return False
    if event == "pull_request":
        for pr in run.get("pull_requests") or []:
            head = api.call("GET", f"pulls/{pr['number']}")["head"]["sha"]
            if head != run["head_sha"]:
                return False
    return True


def completed(api, run_id, attempt, flakes):
    run = api.call("GET", f"actions/runs/{run_id}")
    if run["run_attempt"] != attempt:
        print(f"attempt {attempt} is not the latest ({run['run_attempt']}): its gate decides")
        return 0
    deadline = time.time() + 110 * 60
    while True:
        sha, url = run["head_sha"], run["html_url"]
        if run["conclusion"] == "success":
            api.status(sha, "success", f"CI passed (attempt {attempt})", url)
            return 0
        if run["conclusion"] != "failure":
            api.status(sha, "error", f"CI run ended: {run['conclusion']}", url)
            return 0
        jobs = api.jobs(run_id, attempt)
        verdict, why, rows = decide(
            jobs, attempt,
            lambda j: api.annotations(j["id"]) if not [s for s in j.get("steps") or [] if s.get("conclusion") == "failure"] else [],
            lambda j: api.log(j["id"]),
            flakes, api.issue_open,
        )
        summary([f"### CI gate, run {run_id} attempt {attempt}: {verdict}", "", why, "",
                 "| job | kind | why |", "|---|---|---|"]
                + [f"| {n} | {k} | {w} |" for n, k, w in rows])
        if verdict == "rerun" and not still_current(api, run):
            print("the commit left the queue or the pull request moved on: no rerun")
            api.status(sha, "failure", why.split(":")[0] + "; not rerun (superseded)", url)
            return 0
        if verdict == "fail":
            api.status(sha, "failure", why, url)
            return 0
        api.call("POST", f"actions/runs/{run_id}/rerun-failed-jobs")
        attempt += 1
        api.status(sha, "pending", f"attempt {attempt}: {why}", url)
        # Wait for the rerun here rather than for its workflow_run event, which a rerun
        # started with GITHUB_TOKEN may not send.
        while True:
            time.sleep(60)
            run = api.call("GET", f"actions/runs/{run_id}")
            if run["run_attempt"] >= attempt and run["status"] == "completed":
                attempt = run["run_attempt"]
                break
            if time.time() > deadline:
                api.status(sha, "error", "the rerun did not end within 110 minutes", url)
                return 1


def requested(api, run_id):
    run = api.call("GET", f"actions/runs/{run_id}")
    if run["status"] == "completed":
        return 0
    api.status(run["head_sha"], "pending", f"CI is running (attempt {run['run_attempt']})", run["html_url"])
    return 0


def classify(args):
    jobs = json.loads(Path(args.jobs).read_text())
    jobs = jobs.get("jobs", jobs) if isinstance(jobs, dict) else jobs

    def annotations_of(job):
        p = Path(args.annotations or "/nonexistent") / f"{job['id']}.json"
        return [a["message"] for a in json.loads(p.read_text())] if p.exists() else []

    def log_of(job):
        p = Path(args.logs or "/nonexistent") / f"{job['id']}.txt"
        return p.read_text() if p.exists() else ""

    open_issues = {int(n) for n in args.open_issues.split(",") if n} if args.open_issues else set()
    verdict, why, rows = decide(jobs, args.attempt, annotations_of, log_of,
                                read_flakes(args.flakes), lambda n: n in open_issues)
    for n, k, w in rows:
        print(f"{k:6} {n}: {w}")
    print(f"{verdict}: {why}")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("requested")
    r.add_argument("--run-id", type=int, required=True)
    c = sub.add_parser("completed")
    c.add_argument("--run-id", type=int, required=True)
    c.add_argument("--attempt", type=int, required=True)
    k = sub.add_parser("classify")
    k.add_argument("--jobs", required=True)
    k.add_argument("--annotations")
    k.add_argument("--logs")
    k.add_argument("--attempt", type=int, default=1)
    k.add_argument("--flakes", default=str(ROOT / ".github/known-flakes.txt"))
    k.add_argument("--open-issues", default="")
    args = ap.parse_args()
    if args.cmd == "classify":
        return classify(args)
    api = Api(os.environ["GITHUB_REPOSITORY"], os.environ.get("GITHUB_TOKEN"))
    if args.cmd == "requested":
        return requested(api, args.run_id)
    return completed(api, args.run_id, args.attempt, read_flakes(ROOT / ".github/known-flakes.txt"))


if __name__ == "__main__":
    sys.exit(main())
