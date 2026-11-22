#!/usr/bin/env python3
"""Which CI jobs a change needs: the `plan` job of .github/workflows/ci.yml.

    git diff --name-only BASE HEAD | python3 scripts/ci-scope.py --event pull_request [--draft]
    python3 scripts/ci-scope.py --explain < files.txt

Prints `key=value` lines for $GITHUB_OUTPUT:

    scope   docs   only files no build, test or tool reads (site, changelog, prose):
                   lint and msrv run, nothing else
            draft  a draft pull request: lint, msrv, default features, the build and one
                   round of the tests on io_uring
            pr     a pull request ready for review: every job, each test suite once
            full   the merge queue, a dispatch, a push to main: every job, every
                   repetition (ROUNDS=5, three ThreadSanitizer runs)
    heavy   true when every job runs (scope pr or full)
    rounds  how often scripts/check.sh repeats each suite
    tsan    the ThreadSanitizer matrix, as JSON

Anything it cannot place counts as code, and anything but a pull request is `full`: when
in doubt, more runs, never fewer. The merge queue always runs everything, so main is
tested exactly as before whatever this script decides for a pull request.
"""
import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Files that nothing builds, tests or checks beyond `lint` (release.py check, which
# builds the site and checks the changelog, runs there on every change).
DOCS_DIRS = ("site/", "changelog.d/", "bench/results/", "assets/", "skills/",
             ".github/ISSUE_TEMPLATE/")
DOCS_FILES = {"LICENSE-APACHE", "LICENSE-MIT", ".github/PULL_REQUEST_TEMPLATE.md",
              ".github/hygiene-keep.txt", "llms.txt"}
# Prose that code reads all the same: tests check the error pages, and doc tests run
# the code blocks of every file a crate pulls in with include_str!.
CODE_PROSE = ("docs/errors/",)

TSAN_FULL = [{"driver": "io_uring", "run": 1}, {"driver": "io_uring", "run": 2},
             {"driver": "epoll", "run": 1}]
TSAN_PR = [{"driver": "io_uring", "run": 1}, {"driver": "epoll", "run": 1}]


def included_markdown():
    """Every .md file some .rs file includes with include_str!, repo-relative."""
    found = set()
    pat = re.compile(r'include_str!\(\s*"([^"]+\.md)"\s*\)')
    for rs in ROOT.rglob("*.rs"):
        rel = rs.relative_to(ROOT).as_posix()
        if rel.startswith("target/") or "/target/" in rel:
            continue
        try:
            text = rs.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for m in pat.finditer(text):
            try:
                found.add((rs.parent / m[1]).resolve().relative_to(ROOT).as_posix())
            except ValueError:
                pass
    return found


def is_docs(path, included):
    if path in DOCS_FILES or path.startswith(DOCS_DIRS):
        return True
    if not path.endswith(".md"):
        return False
    if path in included or path.startswith(CODE_PROSE):
        return False
    # Markdown inside a crate's sources or tests may be read by them.
    if re.search(r"(^|/)(src|tests|benches|examples)/", path):
        return False
    return True


def plan(files, event, draft):
    included = included_markdown()
    code = [f for f in files if not is_docs(f, included)]
    if event != "pull_request":
        scope = "full"
    elif files and not code:
        scope = "docs"
    elif draft:
        scope = "draft"
    else:
        scope = "pr"
    return scope, code


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--event", default="pull_request")
    ap.add_argument("--draft", action="store_true")
    ap.add_argument("--explain", action="store_true", help="also say why, on stderr")
    args = ap.parse_args()
    files = [line.strip() for line in sys.stdin if line.strip()]
    scope, code = plan(files, args.event, args.draft)
    heavy = scope in ("pr", "full")
    print(f"scope={scope}")
    print(f"heavy={'true' if heavy else 'false'}")
    print(f"rounds={5 if scope == 'full' else 1}")
    print(f"tsan={json.dumps(TSAN_FULL if scope == 'full' else TSAN_PR)}")
    if args.explain or scope != "full":
        print(f"# {len(files)} files changed, {len(code)} of them code", file=sys.stderr)
        for f in code[:20]:
            print(f"#   code: {f}", file=sys.stderr)


if __name__ == "__main__":
    main()
