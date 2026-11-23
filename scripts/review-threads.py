#!/usr/bin/env python3
"""Resolve the review threads a pull request has answered, and list the rest.

    python3 scripts/review-threads.py <pull request number> [--dry-run]

Run by .github/workflows/review-threads.yml with GH_TOKEN set; works the same from a
maintainer's machine with `gh` logged in.

A thread counts as answered when its last comment is a reply from someone with write
access to the repository (author association OWNER, MEMBER or COLLABORATOR) that starts
with `Fixed in <commit>` (or `Done in`, `Addressed in`), and that commit is one of the
pull request's. Such a thread is resolved. Every thread still open is listed, with its
link, in the job summary: the branch protection holds the merge back until each is
resolved, and an unresolved thread is the usual reason a pull request with auto-merge on
never enters the merge queue.
"""
import json
import os
import re
import subprocess
import sys

REPLY = re.compile(r"^\s*(?:fixed|done|addressed)\s+in\s+`?([0-9a-f]{7,40})`?", re.I)
WRITERS = {"OWNER", "MEMBER", "COLLABORATOR"}

QUERY = """
query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      commits(last: 250) { nodes { commit { oid } } }
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved path line
          comments(last: 1) { nodes { body url authorAssociation author { login } } }
          first: comments(first: 1) { nodes { body url author { login } } }
        }
      }
    }
  }
}
"""

RESOLVE = """
mutation($id: ID!) { resolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }
"""


def gh_graphql(query, **variables):
    cmd = ["gh", "api", "graphql", "-f", f"query={query}"]
    for k, v in variables.items():
        if v is None:
            continue
        cmd += ["-F" if isinstance(v, int) else "-f", f"{k}={v}"]
    out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    return json.loads(out)["data"]


def summary(lines):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    text = "\n".join(lines) + "\n"
    if path:
        with open(path, "a", encoding="utf-8") as f:
            f.write(text)
    print(text, end="")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    dry = "--dry-run" in sys.argv
    if len(args) != 1 or not args[0].isdigit():
        sys.exit(__doc__)
    number = int(args[0])
    owner, name = os.environ.get("GITHUB_REPOSITORY", "giong-run/Giong").split("/")

    threads, commits, after = [], set(), None
    while True:
        pr = gh_graphql(QUERY, owner=owner, name=name, number=number, after=after)["repository"]["pullRequest"]
        commits |= {n["commit"]["oid"] for n in pr["commits"]["nodes"]}
        page = pr["reviewThreads"]
        threads += page["nodes"]
        if not page["pageInfo"]["hasNextPage"]:
            break
        after = page["pageInfo"]["endCursor"]

    resolved, still_open = [], []
    for t in threads:
        if t["isResolved"]:
            continue
        last = (t["comments"]["nodes"] or [None])[0]
        first = (t["first"]["nodes"] or [None])[0]
        m = last and REPLY.match(last["body"] or "")
        if m and last["authorAssociation"] in WRITERS and any(c.startswith(m[1].lower()) for c in commits):
            if not dry:
                gh_graphql(RESOLVE, id=t["id"])
            resolved.append((t, m[1]))
        else:
            still_open.append((t, first))

    lines = [f"### Review threads of #{number}", ""]
    for t, sha in resolved:
        lines.append(f"- resolved{' (dry run)' if dry else ''}: `{t['path']}` answered by {sha}")
    for t, first in still_open:
        title = (first["body"] if first else "").strip().splitlines()[0:1]
        lines.append(f"- **open**: [{t['path']}:{t['line']}]({first['url'] if first else ''}) "
                     f"{(title[0] if title else '')[:100]}")
    if not resolved and not still_open:
        lines.append("No unresolved review thread.")
    elif still_open:
        lines += ["", f"{len(still_open)} thread(s) still open: the merge waits for them. Reply "
                  "`Fixed in <commit>` once the fix is pushed (it resolves the thread), or resolve "
                  "it by hand."]
    summary(lines)
    if still_open and os.environ.get("GITHUB_ACTIONS"):
        print(f"::warning::{len(still_open)} unresolved review thread(s) hold the merge back")


if __name__ == "__main__":
    main()
