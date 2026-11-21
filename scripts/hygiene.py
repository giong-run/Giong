#!/usr/bin/env python3
"""Open pull requests, issues and branches that need someone: a weekly report. Python 3.8+,
standard library and the `gh` CLI only.

    python3 scripts/hygiene.py [--stale-days N] [--issue]

Lists, for giong-run/Giong:

- every branch other than `main` that is neither the head nor the base of an open pull
  request, with what happened to its last pull request: merged or closed ones can be
  deleted (GitHub keeps their commits at refs/pull/N/head), and one that never had a pull
  request is either work in progress or forgotten;
- open pull requests with no update for N days (default 7), and open pull requests whose
  base branch is gone;
- open issues with no update for twice as long (default 14 days): each needs an owner, a
  pull request that says "Fixes #N", or a comment saying what it waits for.

Branches kept on purpose are listed, with the reason, in .github/hygiene-keep.txt (one
`<branch> <reason>` per line) and reported apart. With --issue the report goes into the
open issue labelled `hygiene` (created when needed), which is closed when nothing is left.
The Hygiene workflow (.github/workflows/hygiene.yml) runs it every Monday.
It only reads, apart from that one issue: it never deletes a branch or closes a pull request.
"""

import argparse
import datetime
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPO = os.environ.get("GITHUB_REPOSITORY", "giong-run/Giong")
KEEP = ROOT / ".github" / "hygiene-keep.txt"
LABEL = "hygiene"
TITLE = "Repository hygiene: branches and pull requests that need someone"


def gh(*args, input=None):
    r = subprocess.run(["gh", *args], text=True, input=input, capture_output=True)
    if r.returncode != 0:
        sys.exit(f"error: gh {' '.join(args)}: {r.stderr.strip()}")
    return r.stdout


def api_list(path):
    out = gh("api", "--paginate", "-H", "Accept: application/vnd.github+json", path)
    # --paginate prints one JSON array per page.
    items, dec, i = [], json.JSONDecoder(), 0
    while i < len(out):
        while i < len(out) and out[i].isspace():
            i += 1
        if i >= len(out):
            break
        page, i = dec.raw_decode(out, i)
        items.extend(page)
    return items


def days_since(stamp, now):
    t = datetime.datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
    return (now - t).days


def keep_list():
    keep = {}
    if KEEP.exists():
        for line in KEEP.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                name, _, reason = line.partition(" ")
                keep[name] = reason.strip()
    return keep


def report(stale_days):
    now = datetime.datetime.now(datetime.timezone.utc)
    branches = {b["name"]: b["commit"]["sha"] for b in api_list(f"repos/{REPO}/branches?per_page=100")}
    prs = api_list(f"repos/{REPO}/pulls?state=all&per_page=100&sort=updated&direction=desc")
    open_prs = [p for p in prs if p["state"] == "open"]
    in_use = {p["head"]["ref"] for p in open_prs} | {p["base"]["ref"] for p in open_prs}
    last_pr = {}
    for p in prs:  # newest update first: keep the first pull request seen per head branch
        if p["head"]["repo"] and p["head"]["repo"]["full_name"] == REPO:
            last_pr.setdefault(p["head"]["ref"], p)
    keep = keep_list()

    delete, unknown, kept = [], [], []
    for name, sha in sorted(branches.items()):
        if name == "main" or name in in_use:
            continue
        if name in keep:
            kept.append(f"- `{name}`: {keep[name]}")
            continue
        p = last_pr.get(name)
        if p and p["merged_at"]:
            delete.append(f"- `{name}`: #{p['number']} merged; delete the branch")
        elif p and p["head"]["sha"] == sha:
            delete.append(f"- `{name}`: #{p['number']} closed without merging; its commits stay at "
                          f"`refs/pull/{p['number']}/head`, delete the branch or reopen the pull request")
        elif p:
            unknown.append(f"- `{name}`: #{p['number']} closed, but the branch has commits it does not; "
                           "open a pull request or delete it")
        else:
            unknown.append(f"- `{name}`: no pull request; open one (a draft is fine), list it in "
                           "`.github/hygiene-keep.txt` with a reason, or delete it")

    issues = [i for i in api_list(f"repos/{REPO}/issues?state=open&per_page=100")
              if "pull_request" not in i and LABEL not in {l["name"] for l in i["labels"]}]
    quiet = []
    for i in sorted(issues, key=lambda i: i["number"]):
        age = days_since(i["updated_at"], now)
        if age >= 2 * stale_days:
            who = ", ".join("@" + a["login"] for a in i["assignees"]) or "nobody assigned"
            quiet.append(f"- #{i['number']} {i['title']} ({who}, no update for {age} days): link the "
                         "pull request that fixes it, say what it waits for, or close it")

    stale, orphaned = [], []
    for p in sorted(open_prs, key=lambda p: p["number"]):
        if p["base"]["ref"] not in branches:
            orphaned.append(f"- #{p['number']} {p['title']}: its base `{p['base']['ref']}` is gone; retarget it")
        age = days_since(p["updated_at"], now)
        if age >= stale_days:
            stale.append(f"- #{p['number']} {p['title']} ({'draft, ' if p['draft'] else ''}"
                         f"no update for {age} days): say what it waits for, or close it")

    sections = [
        ("Branches to delete", delete),
        ("Branches with no open pull request", unknown),
        ("Open pull requests whose base branch is gone", orphaned),
        (f"Open pull requests with no update for {stale_days} days or more", stale),
        (f"Open issues with no update for {2 * stale_days} days or more", quiet),
    ]
    body = [f"Generated by `scripts/hygiene.py` on {now:%Y-%m-%d %H:%M} UTC: {len(open_prs)} open pull "
            f"requests, {len(issues)} open issues, {len(branches)} branches."]
    findings = 0
    for title, items in sections:
        if items:
            findings += len(items)
            body += ["", f"### {title}", "", *items]
    if kept:
        body += ["", "### Kept on purpose (`.github/hygiene-keep.txt`)", "", *kept]
    if not findings:
        body += ["", "Nothing to do."]
    return "\n".join(body), findings


def sync_issue(body, findings):
    issues = json.loads(gh("issue", "list", "--repo", REPO, "--label", LABEL, "--state", "open",
                           "--json", "number", "--limit", "5"))
    number = issues[0]["number"] if issues else None
    if findings == 0:
        if number:
            gh("issue", "close", str(number), "--repo", REPO, "--comment", body)
            print(f"closed #{number}")
        return
    if number:
        gh("issue", "edit", str(number), "--repo", REPO, "--body-file", "-", input=body)
        print(f"updated #{number}")
    else:
        gh("label", "create", LABEL, "--repo", REPO, "--color", "BFD4F2",
           "--description", "Branches and pull requests that need someone", "--force")
        print(gh("issue", "create", "--repo", REPO, "--title", TITLE, "--label", LABEL,
                 "--body-file", "-", input=body).strip())


def main():
    sys.stdout.reconfigure(encoding="utf-8")
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--stale-days", type=int, default=7)
    p.add_argument("--issue", action="store_true", help="write the report into the `hygiene` issue")
    args = p.parse_args()
    body, findings = report(args.stale_days)
    print(body)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write("## Repository hygiene\n\n" + body + "\n")
    if args.issue:
        sync_issue(body, findings)


if __name__ == "__main__":
    main()
