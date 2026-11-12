#!/usr/bin/env python3
"""Release tooling for the giong workspace. Python 3.8+, standard library only.

    python3 scripts/release.py check
        Repository invariants that CI enforces on every pull request: the three
        crates share one version, README images use absolute URLs that exist,
        CHANGELOG.md is well formed, and site/public matches site/build.py.

    python3 scripts/release.py prepare [auto|patch|minor|major|X.Y.Z] [--pr] [--dispatch-ci]
        Turn the working tree into the release commit for the next version:
        bump the crates and Cargo.lock, move "Unreleased" in CHANGELOG.md into a
        dated section (drafting it from the commit messages if it is empty),
        update the compare links and README install snippets, and rebuild the
        site. With --pr it also commits on release/X.Y.Z, pushes and opens the
        release pull request; --dispatch-ci then starts CI on that branch, which
        a pull request opened by GitHub Actions does not do by itself.

    python3 scripts/release.py notes X.Y.Z [--github-notes FILE]
        Print the GitHub Release notes: the CHANGELOG.md section, install lines,
        a thank-you to everyone who contributed, and GitHub's generated list of
        pull requests and new contributors (FILE, from the generate-notes API).

    python3 scripts/release.py version
        Print the workspace version.

    python3 scripts/release.py crates
        Print the publishable workspace members, one per line, in dependency
        order: the order release.yml publishes them in. A new member crate is
        picked up from Cargo.toml without a change here (its first publish is
        by hand, see docs/RELEASING.md).

See docs/RELEASING.md for the whole process.
"""

import argparse
import datetime
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPO = "giong-run/Giong"
REPO_URL = "https://github.com/" + REPO
# The published crates are found from the workspace (see workspace_crates), so a
# new member crate is released with the others without a change here.
CHANGELOG = "CHANGELOG.md"
SEMVER = re.compile(r"^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$")

# Keep a Changelog sections, in the order they are written.
SECTIONS = ["Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"]
# Conventional commit types that belong in the changelog, and where.
TYPE_SECTION = {"feat": "Added", "fix": "Fixed", "perf": "Changed", "refactor": "Changed"}


# ------------------------------------------------------------------ helpers
def read(rel):
    with open(ROOT / rel, encoding="utf-8", newline="") as f:
        return f.read()


def write(rel, text):
    with open(ROOT / rel, "w", encoding="utf-8", newline="") as f:
        f.write(text)


def run(*cmd, check=True, capture=True):
    r = subprocess.run(cmd, cwd=ROOT, text=True, encoding="utf-8",
                       stdout=subprocess.PIPE if capture else None)
    if check and r.returncode != 0:
        sys.exit(f"error: {' '.join(cmd)} exited with {r.returncode}")
    return (r.stdout or "").strip()


def fail(msg):
    sys.exit("error: " + msg)


def parse(v):
    m = SEMVER.match(v)
    if not m:
        fail(f"{v!r} is not a version like 1.2.3 or 1.2.3-rc.1")
    return int(m[1]), int(m[2]), int(m[3]), m[4]


def newer(a, b):
    """a > b in SemVer order (pre-releases sort before their release)."""
    pa, pb = parse(a), parse(b)
    if pa[:3] != pb[:3]:
        return pa[:3] > pb[:3]
    if pa[3] == pb[3]:
        return False
    if pa[3] is None or pb[3] is None:
        return pa[3] is None
    return pa[3] > pb[3]


def section(text, table):
    """The body of a top-level [table] in a manifest."""
    m = re.search(r"^\[" + re.escape(table) + r"\]\s*$(.*?)(?=^\[|\Z)", text, re.M | re.S)
    return m[1] if m else None


def workspace_crates():
    """{name: directory} of the publishable workspace members, in dependency order
    (each crate after the workspace crates it depends on), which is the order they
    are published in."""
    ws = section(read("Cargo.toml"), "workspace")
    members = ws and re.search(r"^members\s*=\s*\[(.*?)\]", ws, re.M | re.S)
    dirs = re.findall(r'"([^"]+)"', members[1]) if members else ["."]
    crates, deps = {}, {}
    for d in dirs:
        if "*" in d:
            fail(f"workspace member glob {d!r} is not supported; list the members explicitly")
        text = read(manifest(d))
        pkg = section(text, "package")
        if pkg is None or re.search(r"^publish\s*=\s*false", pkg, re.M):
            continue
        name = re.search(r'^name\s*=\s*"([^"]+)"', pkg, re.M)[1]
        crates[name] = d
        deps[name] = set(re.findall(r"^([A-Za-z0-9_-]+)\s*=\s*\{[^}]*\bpath\s*=", text, re.M))
    order = []

    def visit(name, stack=()):
        if name in order:
            return
        if name in stack:
            fail("dependency cycle between workspace crates: " + " -> ".join(stack + (name,)))
        for dep in sorted(deps[name] & crates.keys()):
            visit(dep, stack + (name,))
        order.append(name)

    for name in crates:
        visit(name)
    return {n: crates[n] for n in order}


def manifest(crate_dir):
    return "Cargo.toml" if crate_dir in (".", "") else f"{crate_dir}/Cargo.toml"


def readmes():
    return sorted({p for d in workspace_crates().values() for p in (ROOT / d).glob("README*.md")})


def crate_names_re():
    """A regex alternation of the workspace crate names, longest first."""
    return "(?:" + "|".join(re.escape(n) for n in sorted(workspace_crates(), key=len, reverse=True)) + ")"


def package_version(manifest):
    """The `version` of the [package] table."""
    m = re.search(r'^\[package\]\s*$(.*?)(?=^\[|\Z)', manifest, re.M | re.S)
    v = m and re.search(r'^version\s*=\s*"([^"]+)"', m[1], re.M)
    return v[1] if v else None


def workspace_version():
    return package_version(read("Cargo.toml"))


def tag_exists(tag):
    return bool(run("git", "tag", "-l", tag))


def changelog_section(text, version):
    """The body of '## [version]', without the heading."""
    out, on = [], False
    for line in text.splitlines():
        if line.startswith("## ["):
            if on:
                break
            on = line.startswith(f"## [{version}]")
            continue
        if on:
            out.append(line)
    return "\n".join(out).strip("\n") if on else None


# ------------------------------------------------------------------ check
IMG_SRC = re.compile(r'<(?:img|source)\b[^>]*?\b(src|srcset)\s*=\s*"([^"]*)"', re.I)
MD_IMG = re.compile(r'!\[[^\]]*\]\(\s*<?([^)\s>]+)')
OWN_FILE = re.compile(
    r"^https://(?:raw\.githubusercontent\.com/" + re.escape(REPO) + r"|github\.com/" + re.escape(REPO)
    + r"/(?:raw|blob))/main/([^?#]+)", re.I)


def check_readmes():
    """crates.io renders a crate's README away from the repository: it rewrites
    relative <img src> and Markdown images but not <source srcset>, so a relative
    path there silently breaks (the dark-mode logo of 0.2.0). Require absolute
    https URLs everywhere, and that links into this repository point at files
    that exist."""
    problems = []
    for path in readmes():
        rel = path.relative_to(ROOT).as_posix()
        for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            urls = [m[1] for m in MD_IMG.finditer(line)]
            for attr, value in IMG_SRC.findall(line):
                urls += [c.strip().split()[0] for c in value.split(",") if c.strip()] if attr.lower() == "srcset" else [value]
            for url in urls:
                if not url.startswith("https://"):
                    problems.append(f"{rel}:{n}: image {url!r} must be an absolute https:// URL "
                                    f"(crates.io cannot resolve relative paths), e.g. "
                                    f"https://raw.githubusercontent.com/{REPO}/main/<path>")
                    continue
                own = OWN_FILE.match(url)
                if own and not (ROOT / own[1]).is_file():
                    problems.append(f"{rel}:{n}: {url} points at {own[1]}, which does not exist")
    return problems


def check_versions():
    problems, v = [], workspace_version()
    for crate, d in workspace_crates().items():
        rel = manifest(d)
        text = read(rel)
        if package_version(text) != v:
            problems.append(f"{rel}: {crate} is at {package_version(text)}, the workspace at {v}")
        for dep, req in re.findall(r'^(' + crate_names_re() + r')\s*=\s*\{[^}]*\bpath\s*=[^}]*\bversion\s*=\s*"([^"]+)"', text, re.M):
            if req != v:
                problems.append(f"{rel}: the dependency on {dep} asks for {req}, the workspace is at {v}")
    lock = read("Cargo.lock")
    for crate in workspace_crates():
        m = re.search(r'^name = "' + re.escape(crate) + r'"\r?\nversion = "([^"]+)"', lock, re.M)
        if m and m[1] != v:
            problems.append(f"Cargo.lock: {crate} is at {m[1]}, the workspace at {v}")
    return problems


def check_changelog():
    problems, text, v = [], read(CHANGELOG), workspace_version()
    if not re.search(r"^## \[Unreleased\]\s*$", text, re.M):
        problems.append(f"{CHANGELOG}: the '## [Unreleased]' heading is missing")
    if not re.search(r"^\[Unreleased\]: " + re.escape(REPO_URL) + r"/compare/v[^.\s]+\.[^\s]+\.\.\.HEAD$", text, re.M):
        problems.append(f"{CHANGELOG}: the '[Unreleased]: {REPO_URL}/compare/vX.Y.Z...HEAD' link is missing")
    if changelog_section(text, v) is None:
        problems.append(f"{CHANGELOG}: there is no '## [{v}]' section for the current version")
    for heading in re.findall(r"^## \[([^\]]+)\]", text, re.M):
        if not re.search(r"^\[" + re.escape(heading) + r"\]: https://", text, re.M):
            problems.append(f"{CHANGELOG}: '## [{heading}]' has no link reference at the bottom")
    return problems


def check_site():
    build = ROOT / "site" / "build.py"
    if not build.exists():
        return []
    problems = []
    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run([sys.executable, str(build), tmp], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
        for f in sorted(Path(tmp).iterdir()):
            committed = ROOT / "site" / "public" / f.name
            if not committed.exists() or committed.read_bytes().replace(b"\r\n", b"\n") != f.read_bytes():
                problems.append(f"site/public/{f.name} is out of date: run `python3 site/build.py` and commit the result")
    return problems


def cmd_check(_args):
    problems = check_versions() + check_readmes() + check_changelog() + check_site()
    for p in problems:
        print(("::error::" if _in_actions() else "") + p)
    if problems:
        sys.exit(1)
    print(f"ok: giong {workspace_version()}, READMEs, CHANGELOG.md and site/public are consistent")


def _in_actions():
    import os
    return os.environ.get("GITHUB_ACTIONS") == "true"


# ------------------------------------------------------------------ prepare
def commits_since(tag):
    rng = f"{tag}..HEAD" if tag_exists(tag) else "HEAD"
    log = run("git", "log", "--no-merges", "--format=%H%x1f%s%x1f%b%x1e", rng)
    out = []
    for rec in log.split("\x1e"):
        if not rec.strip():
            continue
        sha, subject, body = (rec.strip("\n").split("\x1f") + ["", ""])[:3]
        m = re.match(r"^(\w+)(?:\(([^)]*)\))?(!)?:\s*(.+)$", subject)
        if not m or subject.startswith("chore(release)"):
            continue
        breaking = bool(m[3]) or bool(re.search(r"^BREAKING[ -]CHANGE:", body, re.M))
        out.append({"sha": sha, "type": m[1], "scope": m[2], "summary": m[4], "breaking": breaking})
    return out


def next_version(current, how, commits):
    if SEMVER.match(how):
        return how
    major, minor, patch, pre = parse(current)
    if how == "auto":
        if not any(c["type"] in TYPE_SECTION or c["breaking"] for c in commits):
            fail("no feat, fix, perf or breaking commits since the last release; "
                 "pass a version explicitly to release anyway")
        breaking = any(c["breaking"] for c in commits)
        feat = any(c["type"] == "feat" for c in commits)
        # While 0.x, a breaking change bumps the minor version and anything else the patch.
        how = ("major" if breaking else "minor" if feat else "patch") if major else ("minor" if breaking else "patch")
    if pre and how == "patch":
        return f"{major}.{minor}.{patch}"
    return {"major": f"{major + 1}.0.0", "minor": f"{major}.{minor + 1}.0", "patch": f"{major}.{minor}.{patch + 1}"}[how]


def draft_entries(commits):
    """Changelog entries drafted from conventional commit subjects."""
    by_section = {s: [] for s in SECTIONS}
    for c in reversed(commits):
        section = "Changed" if c["breaking"] else TYPE_SECTION.get(c["type"])
        if not section:
            continue
        text = c["summary"]
        text = text[0].upper() + text[1:]
        text = re.sub(r"\s*\(#(\d+)\)$", lambda m: f" ([#{m[1]}]({REPO_URL}/pull/{m[1]}))", text)
        if c["breaking"]:
            text = "**Breaking:** " + text
        by_section[section].append(f"- {text}.")
    blocks = [f"### {s}\n\n" + "\n".join(items) for s, items in by_section.items() if items]
    return "\n\n".join(blocks)


def bump_files(old, new, date, commits):
    changed = []
    # Manifests: the package version and the workspace path dependencies.
    names = crate_names_re()
    for d in workspace_crates().values():
        rel = manifest(d)
        text = read(rel)
        text = re.sub(r'(^\[package\]\s*$.*?^version\s*=\s*")' + re.escape(old) + '"',
                      lambda m: m[1] + new + '"', text, count=1, flags=re.M | re.S)
        text = re.sub(r'(^' + names + r'\s*=\s*\{[^}]*\bversion\s*=\s*")' + re.escape(old) + '"',
                      lambda m: m[1] + new + '"', text, flags=re.M)
        write(rel, text)
        changed.append(rel)
    lock = read("Cargo.lock")
    for crate in workspace_crates():
        lock = re.sub(r'(^name = "' + re.escape(crate) + r'"\r?\nversion = ")' + re.escape(old) + '"',
                      lambda m: m[1] + new + '"', lock, flags=re.M)
    write("Cargo.lock", lock)
    changed.append("Cargo.lock")

    # CHANGELOG.md: Unreleased becomes the new section.
    text = read(CHANGELOG)
    m = re.search(r"^## \[Unreleased\][ \t]*\r?\n(.*?)(?=^## \[)", text, re.M | re.S)
    if not m:
        fail(f"{CHANGELOG} has no '## [Unreleased]' section followed by a release")
    body = m[1].strip()
    drafted = not body
    if drafted:
        body = draft_entries(commits) or "### Changed\n\n- Maintenance release."
        body = ("<!-- Drafted from the commit messages by scripts/release.py. Rewrite it for users "
                "before merging: one line on what the release is about, then the entries. -->\n\n" + body)
    text = text[:m.start()] + f"## [Unreleased]\n\n## [{new}] - {date}\n\n{body}\n\n" + text[m.end():]
    text = re.sub(r"^\[Unreleased\]: (\S+)/compare/v\S+?\.\.\.HEAD$",
                  lambda mm: f"[Unreleased]: {mm[1]}/compare/v{new}...HEAD\n[{new}]: {mm[1]}/compare/v{old}...v{new}",
                  text, count=1, flags=re.M)
    write(CHANGELOG, text)
    changed.append(CHANGELOG)

    # README install snippets: `giong = "0.2"` / `giong-http = { version = "0.2", ... }`.
    o, n = parse(old), parse(new)
    short_old, short_new = f"{o[0]}.{o[1]}", f"{n[0]}.{n[1]}"
    pairs = [(old, new)] + ([(short_old, short_new)] if short_old != short_new and not n[3] else [])
    for path in readmes():
        rel = path.relative_to(ROOT).as_posix()
        text = before = read(rel)
        for a, b in pairs:
            text = re.sub(r'(\b' + names + r'\s*=\s*(?:\{\s*version\s*=\s*)?")' + re.escape(a) + '"',
                          lambda mm: mm[1] + b + '"', text)
        if text != before:
            write(rel, text)
            changed.append(rel)

    # The site reads the version from Cargo.toml; regenerate it.
    if (ROOT / "site" / "build.py").exists():
        subprocess.run([sys.executable, str(ROOT / "site" / "build.py")], cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
        changed.append("site/public")
    return changed, drafted, short_old if short_old != short_new else None


def cmd_prepare(args):
    old = workspace_version()
    commits = commits_since("v" + old)
    new = next_version(old, args.version, commits)
    if not newer(new, old):
        fail(f"{new} is not newer than the current version {old}")
    if tag_exists("v" + new):
        fail(f"the tag v{new} already exists")
    if args.pr and run("git", "status", "--porcelain"):
        fail("the working tree has uncommitted changes")
    if problems := check_versions():
        fail("the workspace is inconsistent before the bump:\n  " + "\n  ".join(problems))

    date = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
    changed, drafted, old_minor = bump_files(old, new, date, commits)
    print(f"giong {old} -> {new} ({date})")
    for rel in changed:
        print("  updated", rel)
    if drafted:
        print("  CHANGELOG.md: 'Unreleased' was empty, so the entries were drafted from commit messages")
    hints = []
    if old_minor:
        # Prose such as "Status: 0.2." is for a person to rewrite.
        for rel in [p.relative_to(ROOT).as_posix() for p in readmes()] + ["site/build.py", "src/lib.rs"]:
            if (ROOT / rel).exists():
                for n, line in enumerate(read(rel).splitlines(), 1):
                    if re.search(r"(?<![\d.])" + re.escape(old_minor) + r"(?![\d.])", line):
                        hints.append(f"{rel}:{n}: {line.strip()[:100]}")
    if hints:
        print(f"\nStill mentions {old_minor}; check whether these should say {'.'.join(new.split('.')[:2])}:")
        print("\n".join("  " + h for h in hints))

    _write_output("version", new)
    if args.pr:
        open_pr(new, old, drafted, hints, args.dispatch_ci)


def _write_output(key, value):
    import os
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a", encoding="utf-8") as f:
            f.write(f"{key}={value}\n")


def open_pr(new, old, drafted, hints, dispatch_ci):
    branch = f"release/{new}"
    title = f"chore(release): giong {new}"
    run("git", "switch", "-c", branch)
    run("git", "add", "-A")
    run("git", "commit", "-q", "-m", title)
    run("git", "push", "-u", "origin", branch, capture=False)
    run("gh", "label", "create", "release", "--color", "0E8A16",
        "--description", "A release pull request; left out of generated release notes", check=False)
    todo = [
        "- [ ] The `CHANGELOG.md` section reads well for users: a one-line summary, then the entries"
        + (" (it was drafted from commit messages, so rewrite it)" if drafted else ""),
        "- [ ] Every outside contributor is thanked in the entry for their change (`thanks to [@name](https://github.com/name)`)",
        "- [ ] Breaking changes are marked **Breaking:** and say how to migrate",
    ]
    if hints:
        todo.append("- [ ] Prose that still names the old version is updated (see the list in the workflow log)")
    crates = ", ".join(f"`{c}`" for c in workspace_crates())
    body = f"""Releases {crates} **{new}** (from {old}).

Merging this pull request tags `v{new}` and runs the release: crates.io (Trusted Publishing), the GitHub Release with notes from `CHANGELOG.md` and the generated contributor list, then giong.run. See [docs/RELEASING.md]({REPO_URL}/blob/main/docs/RELEASING.md).

## Before merging

{chr(10).join(todo)}
- [ ] CI is green
"""
    url = run("gh", "pr", "create", "--base", "main", "--head", branch, "--title", title,
              "--label", "release", "--body", body)
    print("opened", url)
    _write_output("pr", url)
    if dispatch_ci:
        # Events caused by GITHUB_TOKEN start no workflows, except workflow_dispatch;
        # the required checks then report on the release commit as usual.
        run("gh", "workflow", "run", "ci.yml", "--ref", branch)
        print("started CI on", branch)


# ------------------------------------------------------------------ notes
def cmd_notes(args):
    version = args.version
    section = changelog_section(read(CHANGELOG), version)
    if section is None:
        fail(f"{CHANGELOG} has no '## [{version}]' section")
    section = re.sub(r"<!--.*?-->\s*", "", section, flags=re.S).strip()
    generated = Path(args.github_notes).read_text(encoding="utf-8").strip() if args.github_notes else ""

    handles = []
    for h in re.findall(r"(?<![\w/])@([A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\[bot\])?)", section + "\n" + generated):
        if h.endswith("[bot]") or h.lower() in (x.lower() for x in handles):
            continue
        handles.append(h)

    pre = parse(version)[3] is not None
    crates = list(workspace_crates())
    toml = "\n".join(f'{c} = "{"=" if pre else ""}{version}"' for c in crates)
    links = " · ".join(f"[{c}](https://crates.io/crates/{c}/{version}) ([docs](https://docs.rs/{c}/{version}))"
                       for c in crates)
    parts = [section, "## Install", f"```toml\n[dependencies]\n{toml}\n```", f"On crates.io: {links}"]
    if handles:
        names = [f"@{h}" for h in handles]
        joined = names[0] if len(names) == 1 else ", ".join(names[:-1]) + " and " + names[-1]
        parts.append(f"## Contributors\n\nThank you to {joined} for contributing to this release.")
    if generated:
        # GitHub's own headings sit one level below ours.
        parts.append(re.sub(r"^## ", "### ", generated, flags=re.M))
    else:
        parts.append(f"**Full changelog**: {REPO_URL}/blob/v{version}/CHANGELOG.md")
    print("\n\n".join(parts))


def cmd_version(_args):
    print(workspace_version())


def cmd_crates(_args):
    print("\n".join(workspace_crates()))


def main():
    sys.stdout.reconfigure(encoding="utf-8")  # release notes go through a pipe on Windows too
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check").set_defaults(fn=cmd_check)
    sub.add_parser("version").set_defaults(fn=cmd_version)
    sub.add_parser("crates").set_defaults(fn=cmd_crates)
    pp = sub.add_parser("prepare")
    pp.add_argument("version", nargs="?", default="auto", help="auto, patch, minor, major or X.Y.Z")
    pp.add_argument("--pr", action="store_true", help="commit on release/X.Y.Z, push and open the pull request")
    pp.add_argument("--dispatch-ci", action="store_true", help="start CI on the release branch (for GitHub Actions)")
    pp.set_defaults(fn=cmd_prepare)
    pn = sub.add_parser("notes")
    pn.add_argument("version")
    pn.add_argument("--github-notes", help="markdown from the generate-notes API")
    pn.set_defaults(fn=cmd_notes)
    args = p.parse_args()
    if getattr(args, "version", None) not in (None, "auto", "patch", "minor", "major") and args.cmd != "notes":
        parse(args.version)
    args.fn(args)


if __name__ == "__main__":
    main()
