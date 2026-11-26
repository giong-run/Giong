#!/usr/bin/env python3
"""Which CI jobs a change needs: the `plan` job of .github/workflows/ci.yml.

    git diff --name-only BASE HEAD | python3 scripts/ci-scope.py --event pull_request [--draft]
    python3 scripts/ci-scope.py --explain < files.txt

Prints `key=value` lines for $GITHUB_OUTPUT:

    scope   docs   only files no build, test or tool reads (site, changelog, prose):
                   lint and msrv run, nothing else. On a pull request, and in the merge
                   queue when the whole group (every pull request it holds) is such files
            draft  a draft pull request: the same jobs as `pr` in ci.yml; the other
                   workflows (Fuzz, gRPC, Proto, macOS, Windows) skip drafts
            pr     a pull request ready for review: the fast set (lint, msrv, default
                   features, the build, one round of the tests on io_uring and of the
                   epoll tier). Since Workflow v2 (docs/WORKFLOW.md) every other job runs
                   in the merge queue only, before anything lands
            full   the merge queue, a dispatch, a push to main: every job, every
                   repetition (ROUNDS=5, every fallback tier, three ThreadSanitizer
                   runs), macOS and Windows included
    heavy   true when every job runs (scope full)
    rounds  how often scripts/check.sh repeats each suite
    tsan    the ThreadSanitizer matrix, as JSON
    fallback  the tiers of scripts/check.sh fallback to run, as JSON
    platform  with --platform: whether that platform's workflow runs (see below)

`--relevant PREFIX...` narrows a workflow to the files it tests: a pull request that
changes none of them gets scope `docs` (that workflow's tests skip).

`--platform macos|windows`: on a pull request ready for review, the macOS or Windows
workflow runs only when the change is likely to behave differently there: a file of the
platform layer, a Rust file that names the platform in a `cfg`, a manifest, or the
workflow itself. Everything else reaches macOS and Windows in the merge queue, which
calls both workflows for every group with code in it. A pull request that wants them
anyway dispatches CI on its branch (docs/WORKFLOW.md).

Anything it cannot place counts as code, and anything but a pull request or a docs-only
merge group is `full`: when in doubt, more runs, never fewer.
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
             ".github/ISSUE_TEMPLATE/", "docs/workflow/")
# .github/known-flakes.txt is read by the CI gate only, from main: a pull request that
# adds an entry (docs/WORKFLOW.md, "Flaky tests") lands in minutes.
DOCS_FILES = {"LICENSE-APACHE", "LICENSE-MIT", ".github/PULL_REQUEST_TEMPLATE.md",
              ".github/hygiene-keep.txt", ".github/known-flakes.txt", "llms.txt"}
# Prose that code reads all the same: tests check the error pages, and doc tests run
# the code blocks of every file a crate pulls in with include_str!.
CODE_PROSE = ("docs/errors/",)

TSAN_FULL = [{"driver": "io_uring", "run": 1}, {"driver": "io_uring", "run": 2},
             {"driver": "epoll", "run": 1}]
TSAN_PR = [{"driver": "io_uring", "run": 1}, {"driver": "epoll", "run": 1}]
FALLBACK_FULL = ["no-features", "coop", "basic", "epoll"]
# On a pull request: the second driver, one round. The other tiers run in the queue.
FALLBACK_PR = ["epoll"]

# The files that make a pull request run a platform's workflow (--platform), and the
# words in a changed Rust file that do. A manifest can change a dependency or a feature
# that builds differently there.
PLATFORM_PATHS = {
    "macos": ("src/sys/", "src/driver/kqueue.rs", "src/driver/readiness",
              ".github/workflows/macos.yml", ".github/macos-tests.sh",
              "tests/enobufs.rs", "src/inject_enobufs.rs"),
    "windows": ("src/sys/", "src/driver/iocp", ".github/workflows/windows-runtime.yml",
                ".github/workflows/templates-windows.yml", "scripts/templates.sh",
                "templates/"),
}
PLATFORM_WORDS = {
    "macos": re.compile(r'target_os\s*=\s*"(macos|ios)"|target_vendor\s*=\s*"apple"'
                        r'|\bkqueue\b|\bEVFILT_|std::os::darwin'),
    "windows": re.compile(r'cfg(_attr)?\([^\n]*\bwindows\b|target_os\s*=\s*"windows"'
                          r'|std::os::windows|windows_sys::|\bWSA[A-Z]'),
}
MANIFESTS = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml")


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


def touches_platform(files, name, root=ROOT):
    """Whether a pull request changing these files should run platform `name`'s workflow."""
    for f in files:
        if f.startswith(PLATFORM_PATHS[name]):
            return True
        if f == MANIFESTS[0] or f.endswith("/Cargo.toml") or f in MANIFESTS:
            return True
        if f.endswith(".rs"):
            try:
                text = (root / f).read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                # Deleted, or not readable: the queue will tell.
                continue
            if PLATFORM_WORDS[name].search(text):
                return True
    return False


def plan(files, event, draft, relevant=None):
    included = included_markdown()
    code = [f for f in files if not is_docs(f, included)]
    if event == "merge_group":
        # Docs only when every file of the whole group is: --relevant never narrows
        # the queue, where macOS and Windows run for any code.
        scope = "docs" if files and not code else "full"
        return scope, code
    if relevant is not None:
        code = [f for f in code if f.startswith(tuple(relevant))]
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
    ap.add_argument("--relevant", nargs="+", metavar="PREFIX",
                    help="only files under these paths count as code for this workflow")
    ap.add_argument("--platform", choices=sorted(PLATFORM_PATHS),
                    help="also say whether this platform's workflow runs (output platform)")
    args = ap.parse_args()
    files = [line.strip() for line in sys.stdin if line.strip()]
    scope, code = plan(files, args.event, args.draft, args.relevant)
    heavy = scope == "full"
    print(f"scope={scope}")
    print(f"heavy={'true' if heavy else 'false'}")
    print(f"rounds={5 if scope == 'full' else 1}")
    print(f"tsan={json.dumps(TSAN_FULL if scope == 'full' else TSAN_PR)}")
    # Never empty: a matrix of nothing is an error even in a job that is skipped.
    fallback = FALLBACK_FULL if scope == "full" else FALLBACK_PR
    print(f"fallback={json.dumps(fallback)}")
    if args.platform:
        on = scope == "full" or (scope == "pr" and touches_platform(code, args.platform))
        print(f"platform={'true' if on else 'false'}")
    if args.explain or scope != "full":
        print(f"# {len(files)} files changed, {len(code)} of them code", file=sys.stderr)
        for f in code[:20]:
            print(f"#   code: {f}", file=sys.stderr)


if __name__ == "__main__":
    main()
