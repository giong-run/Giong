#!/usr/bin/env python3
"""Builds giong.run into site/public/.

    python3 site/build.py              # write the site into site/public/
    python3 site/build.py OUT          # write it somewhere else
    python3 site/build.py --preview    # links between pages end in .html, for a local look

What a person writes lives in content.py. Everything the repository already knows is
read from it here, so the site follows the project without an edit:

    the version and the crates   Cargo.toml files (through scripts/release.py)
    lines of code                each crate's src/
    runtime dependencies         [dependencies] of the root Cargo.toml
    the roadmap                  ROADMAP.md
    the release notes            CHANGELOG.md
    the error codes              docs/errors/README.md

The generated pages are not committed: the Site workflow builds them when it deploys.
Python 3.8 or newer, nothing to install.
"""
import datetime
import html
import json
import math
import re
import subprocess
import sys
import textwrap
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(ROOT / "scripts"))
import art  # noqa: E402
import content as C  # noqa: E402
import md  # noqa: E402
import release  # noqa: E402

REPO = "https://github.com/giong-run/Giong"
BLOB = REPO + "/blob/main/"
TREE = REPO + "/tree/main/"
SITE = "https://giong.run/"
X_URL = "https://x.com/giongrun"
SECURITY = REPO + "/security/advisories/new"   # GitHub private vulnerability reporting
AUTHOR = "thekivashyn"
AUTHOR_URL = "https://github.com/thekivashyn"

PREVIEW = "--preview" in sys.argv


# ================================================================ the repository
def read(rel):
    return (ROOT / rel).read_text(encoding="utf-8")


VERSION = release.workspace_version()
TAG = "v" + VERSION
SITE_TAG = TAG          # the header always names the newest release
# What the site says about platforms follows the released version (content.apply_version).
C.apply_version(tuple(int(x) for x in VERSION.split("-")[0].split(".")[:2]))
MINOR = ".".join(VERSION.split(".")[:2])
# The documentation as released: what `cargo add` installs is what the tag
# holds, while main may already describe the next version.
REL_BLOB = f"{REPO}/blob/{TAG}/"
REL_TREE = f"{REPO}/tree/{TAG}/"


def git(*args):
    try:
        return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return None


# A shallow clone has no tags: then the working tree stands in for the tag,
# and the build says so. The deploy job fetches the tags.
TAG_HERE = git("rev-parse", "--verify", "--quiet", TAG + "^{commit}") is not None


def released(path):
    """`path` as the release tag has it, or None if it has no such file."""
    if TAG_HERE:
        out = git("show", f"{TAG}:{path}")
        return None if out is None else out.decode("utf-8")
    f = ROOT / path
    return f.read_text(encoding="utf-8") if f.is_file() else None
CRATE_DIRS = release.workspace_crates()          # {name: dir}, in dependency order


def cargo_field(d, field):
    pkg = release.section(read(release.manifest(d)), "package") or ""
    m = re.search(rf'^{field}\s*=\s*"([^"]*)"', pkg, re.M)
    return m.group(1) if m else ""


def count_lines(d):
    return sum(f.read_text(encoding="utf-8").count("\n") for f in (ROOT / d / "src").rglob("*.rs"))


def about(n):
    """8,812 -> 'about 8,800'; under 1,000 to the nearest ten."""
    step = 100 if n >= 1000 else 10
    return f"about {round(n / step) * step:,}"


def runtime_deps():
    deps = release.section(read("Cargo.toml"), "dependencies") or ""
    return [m.group(1) for m in re.finditer(r"^([A-Za-z0-9_-]+)\s*=(.*)$", deps, re.M)
            if "optional = true" not in m.group(2)]


CRATES = []
for _name, _dir in CRATE_DIRS.items():
    _c = dict(C.CRATES.get(_name, {}))
    _c.update(name=_name, dir=_dir, lines=count_lines(_dir), description=cargo_field(_dir, "description"))
    _c.setdefault("layer", "more")
    _c.setdefault("short", _c["description"])
    _c.setdefault("lede", _c["description"])
    _c.setdefault("items", [])
    CRATES.append(_c)
COMING = [dict(v, name=k, coming=True) for k, v in C.COMING.items() if k not in CRATE_DIRS]
TOTAL_TESTS = sum(c.get("tests", 0) for c in CRATES)
DEPS = runtime_deps()


# ---------------------------------------------------------------- ROADMAP.md
def parse_roadmap():
    text = read("ROADMAP.md")
    parts = re.split(r"^## +(.+?)\s*$", text, flags=re.M)
    intro, sections = parts[0], list(zip(parts[1::2], parts[2::2]))
    intro = re.sub(r"^# .*\n", "", intro).strip()
    named, milestones = {}, []
    for title, body in sections:
        if title in ("Goal", "Agent-first", "Non-goals"):
            named[title] = body.strip()
            continue
        boxes = re.findall(r"^\s*[-*] \[([ xX])\]", body, re.M)
        done = sum(1 for b in boxes if b != " ")
        m = re.match(r"^(\d+(?:\.\d+)*(?:\.x)?)\s+—\s+(.+?)(?:\s+\((done)\))?$", title)
        if m:
            ver, name, flag = m.group(1), m.group(2), m.group(3)
        else:
            ver, flag = None, None
            name = re.sub(r"\s+\(done\)$", "", title)
            flag = "done" if title.endswith("(done)") else None
            mv = re.match(r"^(\d+\.\d+)$", title)
            if mv:
                ver, name = mv.group(1), "Stable"
        if flag or (boxes and done == len(boxes)):
            # ROADMAP.md on main can call a milestone done before its release is out; the
            # site describes released versions, so until then it is "releasing".
            state = "releasing" if ver and ver_key(ver) > ver_key(MINOR) else "done"
        elif done:
            state = "current"
        else:
            state = "planned"
        proposed = bool(re.match(r"^Proposed\b", body.strip()))
        milestones.append(dict(title=title, ver=ver, name=name, state=state, done=done,
                               total=len(boxes), body=body.strip(), id=md.slug(title),
                               proposed=proposed))
    if not any(m["state"] in ("current", "releasing") for m in milestones):
        for m in milestones:
            if m["state"] == "planned":
                m["state"] = "current"
                break
    return intro, named, milestones


def ver_key(ver):
    """'1.4' -> (1, 4), '1.4.x' -> (1, 4, 0.5): a patch series comes after its minor."""
    return tuple(0.5 if p == "x" else int(p) for p in ver.split("."))


ROADMAP_INTRO, ROADMAP_NAMED, MILESTONES = parse_roadmap()


def milestone_name(version):
    """'0.3.0' -> 'Prove it', from the roadmap heading '0.3 — Prove it'."""
    minor = ".".join(version.split(".")[:2])
    for m in MILESTONES:
        if m["ver"] == minor:
            return m["name"]
    return ""


# ---------------------------------------------------------------- CHANGELOG.md
def parse_changelog():
    text = read("CHANGELOG.md")
    refs = dict(re.findall(r"^\[([^\]]+)\]:\s+(\S+)", text, re.M))
    parts = re.split(r"^## \[([^\]]+)\](?:\s+-\s+(\S+))?\s*$", text, flags=re.M)
    out = []
    for i in range(1, len(parts), 3):
        version, date, body = parts[i], parts[i + 1], parts[i + 2]
        body = re.sub(r"^\[[^\]]+\]:\s+\S+\s*$", "", body, flags=re.M).strip()
        intro = body.split("\n### ", 1)[0].strip() if not body.startswith("### ") else ""
        out.append(dict(version=version, date=date, body=body, intro=intro,
                        compare=refs.get(version, ""), id="v" + version.replace(".", "-").lower()))
    return out


RELEASES = parse_changelog()
LATEST = next((r for r in RELEASES if r["version"] != "Unreleased"), None)


def nice_date(iso):
    try:
        d = datetime.date.fromisoformat(iso)
    except (TypeError, ValueError):
        return iso or ""
    return f"{d.strftime('%B')} {d.day}, {d.year}"


# ---------------------------------------------------------------- docs/errors
def parse_errors():
    text = read("docs/errors/README.md")
    groups, current = [], None
    for line in text.splitlines():
        h = re.match(r"^## (.+)$", line)
        if h:
            current = dict(name=h.group(1).strip(), rows=[])
            groups.append(current)
            continue
        row = re.match(r"^\|\s*\[(GIONG-E\d+)\]\(([^)]+)\)\s*\|\s*`?([^|`]*)`?\s*\|\s*(.*?)\s*\|\s*$", line)
        if row and current is not None:
            current["rows"].append(dict(code=row.group(1), file=row.group(2),
                                        variant=row.group(3).strip(), meaning=row.group(4)))
    return [g for g in groups if g["rows"]]


ERRORS = parse_errors()
ERROR_COUNT = sum(len(g["rows"]) for g in ERRORS)


# ================================================================ helpers
def link(page, anchor=""):
    """The address of one of the site's own pages. Documentation pages are "docs/<slug>"."""
    if PREVIEW:
        base = "index.html" if page == "home" else f"{page.replace('/', '-')}.html"
    else:
        base = "/" if page == "home" else f"/{page}/"
    return base + (f"#{anchor}" if anchor else "")


def page_file(page):
    """Where a page is written: docs/index.html, so that /docs/ works on any static host."""
    if page == "home":
        return "index.html"
    return f"{page.replace('/', '-')}.html" if PREVIEW else f"{page}/index.html"


def asset(path):
    return path if PREVIEW else "/" + path


def repo_link(url, src_dir=""):
    """A link from a repository document, made absolute."""
    if re.match(r"^[a-z]+:", url) or url.startswith("#"):
        return url
    path = (Path(src_dir) / url).as_posix() if src_dir else url
    path = re.sub(r"(^|/)\./", r"\1", path)
    while "/../" in path:
        path = re.sub(r"[^/]+/\.\./", "", path, count=1)
    return (TREE if path.endswith("/") else BLOB) + path


def doc_md(src, src_dir="", **kw):
    return md.render(src, link=lambda u: repo_link(u, src_dir), **kw)


def split_intro(src):
    """The opening paragraphs of a Markdown body, and the rest from its first list or heading."""
    m = re.search(r"^(?:#{2,6} |[-*] |\d+\. |\|)", src, re.M)
    return (src, "") if not m else (src[:m.start()].strip(), src[m.start():].strip())


def more(label, inner, cls="more-box"):
    return f'<details class="{cls}"><summary>{label}</summary>{inner}</details>'


def ilm(s, src_dir=""):
    """Inline Markdown in a content string, links resolved against the repository."""
    return md.inline(s, link=lambda u: repo_link(u, src_dir))


KEYWORDS = ("use fn let async await move match while loop if else return "
            "struct impl pub mut for in as drop").split()
TOKEN = re.compile(
    r"(?P<c>//[^\n]*)"
    r"|(?P<s>b?\"(?:[^\"\\]|\\.)*\")"
    r"|(?P<a>#\[[^\]]*\])"
    r"|(?P<k>\b(?:" + "|".join(KEYWORDS) + r")\b)"
    r"|(?P<t>\b[A-Z][A-Za-z0-9]*\b)"
)


def rust(src):
    out, pos = [], 0
    for m in TOKEN.finditer(src):
        out.append(html.escape(src[pos:m.start()], quote=False))
        kind = "c" if m.lastgroup == "a" else m.lastgroup
        out.append(f'<span class="{kind}">{html.escape(m.group(), quote=False)}</span>')
        pos = m.end()
    out.append(html.escape(src[pos:], quote=False))
    return "".join(out)


def shell(src):
    lines = []
    for line in src.split("\n"):
        if line.lstrip().startswith("#"):
            lines.append(f'<span class="c">{html.escape(line, quote=False)}</span>')
            continue
        code, sep, comment = line.partition("  #")
        line = html.escape(code, quote=False)
        if sep:
            line += f'<span class="c">  #{html.escape(comment, quote=False)}</span>'
        lines.append(line)
    return "\n".join(lines)


def ring(lit=14, start=225.0, n=24, gap=3.0, r_out=190.0, r_in=152.0, c=200.0):
    """The submission ring: n segments, clockwise, the first `lit` of them on.
    The oldest three wear the fading tones of the mane in the logo."""
    step = 360.0 / n
    paths = []
    for i in range(n):
        a0 = math.radians(start + i * step + gap / 2)
        a1 = math.radians(start + (i + 1) * step - gap / 2)

        def pt(r, a):
            return f"{c + r * math.sin(a):.1f} {c - r * math.cos(a):.1f}"
        d = (f"M{pt(r_out, a0)}A{r_out:g} {r_out:g} 0 0 1 {pt(r_out, a1)}"
             f"L{pt(r_in, a1)}A{r_in:g} {r_in:g} 0 0 0 {pt(r_in, a0)}Z")
        cls = "seg"
        if i < lit:
            cls += " on" + (f" a{i + 1}" if i < 3 else "")
        paths.append(f'<path class="{cls}" d="{d}"/>')
    return "".join(paths)


ICONS = {
    "github": ('<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 '
               '5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-'
               '.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87'
               '.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 '
               '2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51'
               '.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 '
               '.21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8Z"/></svg>'),
    "x": ('<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M18.9 1.15h3.68l-8.04 9.19L24 22.85h-7.4'
          'l-5.8-7.59-6.64 7.59H.47l8.6-9.83L0 1.15h7.59l5.25 6.93Zm-1.29 19.5h2.04L6.49 3.24H4.3Z"/></svg>'),
    "sun": ('<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" '
            'stroke-linecap="round"><circle cx="12" cy="12" r="4.2"/><path d="M12 2.5v2.2M12 19.3v2.2M2.5 12h2.2'
            'M19.3 12h2.2M5.3 5.3l1.6 1.6M17.1 17.1l1.6 1.6M5.3 18.7l1.6-1.6M17.1 6.9l1.6-1.6"/></svg>'),
    "moon": ('<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" '
             'stroke-linejoin="round"><path d="M20.5 14.6A8.5 8.5 0 0 1 9.4 3.5a8.5 8.5 0 1 0 11.1 11.1Z"/></svg>'),
    "menu": ('<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" '
             'stroke-linecap="round"><path d="M4 7h16M4 12h16M4 17h16"/></svg>'),
    "rss": ('<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 3a16 16 0 0 1 16 16h-3A13 13 0 0 0 5 6Z'
            'm0 6a10 10 0 0 1 10 10h-3a7 7 0 0 0-7-7Zm1.5 6.5a2 2 0 1 1 0 4 2 2 0 0 1 0-4Z"/></svg>'),
    "copy": ('<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" '
             'stroke-linejoin="round"><rect x="8.5" y="8.5" width="12" height="12" rx="2.2"/>'
             '<path d="M15.5 8.5V5.7a2.2 2.2 0 0 0-2.2-2.2H5.7a2.2 2.2 0 0 0-2.2 2.2v7.6a2.2 2.2 0 0 0 2.2 2.2h2.8"/></svg>'),
    "spark": ('<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" '
              'stroke-linejoin="round"><path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9Z"/>'
              '<path d="M18.5 15.5l.8 2.2 2.2.8-2.2.8-.8 2.2-.8-2.2-2.2-.8 2.2-.8Z"/></svg>'),
    "file": ('<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" '
             'stroke-linejoin="round" stroke-linecap="round"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10'
             'a2 2 0 0 0 2-2V8Z"/><path d="M14 3v5h5M9 13h6M9 17h4"/></svg>'),
}

NAV = [("docs", "Docs"), ("crates", "Crates"), ("benchmarks", "Benchmarks"),
       ("roadmap", "Roadmap"), ("releases", "Releases"), ("docs/agent-skill", "Agent skill")]


def head_block(eyebrow, h2, lede="", more=None):
    m = f'<a class="more" href="{more[0]}">{more[1]} <span aria-hidden="true">→</span></a>' if more else ""
    lede_html = f'<p class="head-lede">{ilm(lede)}</p>' if lede else ""
    return (f'<header class="head"><div class="head-text"><p class="eyebrow">{eyebrow}</p><h2>{h2}</h2>'
            f'{lede_html}</div>{m}</header>')


def copy_cmd(ident, cmd):
    return (f'<div class="cmd"><code id="{ident}">{html.escape(cmd)}</code>'
            f'<button class="copy" type="button" data-copy="{ident}" data-done="copied">copy</button></div>')


# ================================================================ components
def chart(c, home=False):
    rows = []
    for row in c["rows"]:
        if isinstance(row, str):
            rows.append(f'<li class="bar-group">{ilm(row)}</li>')
            continue
        key, value, label = row[0], row[1], row[2]
        cls = "bar" + (" is-giong" if key in C.G else " is-alt" if key in C.ALT else "")

        def pct(v):
            return f"{100 * v / c['max']:.2f}"
        rng = ""
        if len(row) == 5:
            rng = f'<span class="bar-range" style="--lo:{pct(row[3])};--hi:{pct(row[4])}"></span>'
        rows.append(
            f'<li class="{cls}"><span class="bar-name">{C.NAMES[key]}</span>'
            f'<span class="bar-track"><span class="bar-fill" style="--v:{pct(value)}"></span>{rng}</span>'
            f'<span class="bar-val">{label}</span></li>')
    note = c.get("short", c["note"]) if home else c["note"]
    src = " · ".join(f'<a href="{BLOB}{s}">{s}</a>' for s in c["src"])
    return (f'<figure class="chart" id="{c["id"]}"><figcaption class="chart-head"><h3>{c["title"]}</h3>'
            f'<p class="chart-sub">{ilm(c["sub"])}</p></figcaption>'
            f'<ol class="bars">{"".join(rows)}</ol>'
            f'<p class="chart-note">{ilm(note)}</p>'
            f'<p class="chart-src">{src}</p></figure>')


def stack(detail=False):
    """The crates as layers: what sits on what. New crates find their layer by name."""
    by_layer = {}
    for c in CRATES + COMING:
        by_layer.setdefault(c["layer"], []).append(c)
    layers = list(C.LAYERS) + ([("more", "More")] if "more" in by_layer else [])
    rows = []
    for key, label in layers:
        items = by_layer.get(key)
        if not items:
            continue
        tiles = []
        for c in items:
            if c.get("coming"):
                tiles.append(f'<div class="tile is-coming"><b>{c["name"]}</b><span>{ilm(c["short"])}</span>'
                             f'<i>on the roadmap</i></div>')
                continue
            href = link("crates", c["name"]) if not detail else f"#{c['name']}"
            tiles.append(f'<a class="tile{" is-runtime" if key == "runtime" else ""}" href="{href}">'
                         f'<b>{c["name"]}</b><span>{ilm(c["short"])}</span></a>')
        rows.append(f'<div class="layer is-{key}"><p class="layer-name">{label}</p>'
                    f'<div class="layer-tiles">{"".join(tiles)}</div></div>')
    base = ('<div class="layer is-kernel"><p class="layer-name">Linux</p><div class="layer-tiles">'
            '<div class="kernel"><span>io_uring</span><span>one thread and one ring per core</span>'
            '<span>kernel 6.0+</span></div></div></div>')
    return f'<div class="stack">{"".join(rows)}{base}</div>'


def code_window(samples, ident="code"):
    tabs = "".join(
        f'<button class="tab" role="tab" type="button" id="{ident}-tab-{n}" aria-controls="{ident}-panel-{n}" '
        f'aria-selected="{"true" if n == 0 else "false"}">{s["tab"]}<span>{s["crate"]}</span></button>'
        for n, s in enumerate(samples))
    panels = "".join(
        f'<div class="panel" role="tabpanel" id="{ident}-panel-{n}" aria-labelledby="{ident}-tab-{n}">'
        f'<p class="panel-title">{s["tab"]} · {s["crate"]}</p>'
        f'<pre class="code" tabindex="0"><code>{rust(s["src"])}</code></pre>'
        f'<p class="panel-foot">{ilm(s["foot"])}</p></div>'
        for n, s in enumerate(samples))
    return (f'<div class="codewin"><div class="codewin-bar"><div class="tabs" role="tablist" '
            f'aria-label="Code samples">{tabs}</div><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span></div>'
            f'<div class="panels">{panels}</div></div>')


def crate_links(c, guide=False):
    page = "cli" if c["name"] == "giong-cli" and any(e[0] == "cli" for e in DOCS) else c["name"]
    links = [("guide", link(doc_page_name(page)))] if guide and any(e[0] == page for e in DOCS) else []
    links += [("docs.rs", f"https://docs.rs/{c['name']}/{VERSION}" if DOC_PREFIX else f"https://docs.rs/{c['name']}"),
             ("crates.io", f"https://crates.io/crates/{c['name']}"),
             ("source", REPO if c["dir"] == "." else TREE + c["dir"])]
    return " · ".join(f'<a href="{u}">{label}</a>' for label, u in links)


# ================================================================ the frame
def frame(page, title, desc, body, path=""):
    canonical = SITE + path
    current = ' aria-current="page"'
    nav = "".join(f'<a href="{link(p)}"{current if p == page or (p == "docs" and page.startswith("docs/") and page not in dict(NAV)) else ""}>{label}</a>'
                  for p, label in NAV if p != "docs/agent-skill" or "agent-skill" in VERSION_SLUGS[""])
    mark = (f'<picture><source media="(prefers-color-scheme: light)" srcset="{asset("assets/giong-mark-light.svg")}">'
            f'<img src="{asset("assets/giong-mark.svg")}" alt="" width="28" height="28"></picture>')
    release_url = link("releases", LATEST["id"]) if LATEST else link("releases")
    header = f'''<header class="hdr">
  <div class="wrap hdr-in">
    <a class="brand" href="{link("home")}" aria-label="giong, home">{mark}<span>giong</span></a>
    <nav class="nav" aria-label="Site">{nav}</nav>
    <div class="hdr-side">
      <a class="ver" href="{release_url}" title="Release notes for {SITE_TAG}">{SITE_TAG}</a>
      <a class="icon-btn" href="{REPO}" aria-label="giong on GitHub">{ICONS["github"]}</a>
      <a class="icon-btn hide-sm" href="{X_URL}" aria-label="giong on X">{ICONS["x"]}</a>
      <button class="icon-btn theme" type="button" aria-label="Switch between light and dark" hidden>
        <span class="if-dark">{ICONS["sun"]}</span><span class="if-light">{ICONS["moon"]}</span></button>
      <details class="menu">
        <summary class="icon-btn" aria-label="Menu">{ICONS["menu"]}</summary>
        <nav class="menu-panel" aria-label="Site">
          {nav}
          <span class="menu-social"><a href="{REPO}">GitHub</a><a href="{X_URL}">X</a></span>
        </nav>
      </details>
    </div>
  </div>
</header>'''
    mark_sm = mark.replace('width="28" height="28"', 'width="22" height="22"')
    docs_rs = f"https://docs.rs/giong/{VERSION}/giong/"
    footer = f'''<footer class="ftr">
  <div class="wrap">
    <div class="ftr-grid">
      <div class="ftr-brand">
        <a class="brand" href="{link("home")}" aria-label="giong, home">{mark_sm}<span>giong</span></a>
        <p>A thread-per-core async runtime for Rust, built on io_uring and made for the code agents write.</p>
        <p class="ftr-social">
          <a class="icon-btn" href="{REPO}" aria-label="GitHub">{ICONS["github"]}</a>
          <a class="icon-btn" href="{X_URL}" aria-label="X">{ICONS["x"]}</a>
          <a class="icon-btn" href="{REPO}/releases.atom" aria-label="Release feed">{ICONS["rss"]}</a>
        </p>
      </div>
      <nav class="ftr-col" aria-label="Learn">
        <h2>Learn</h2>
        <a href="{link("docs")}">Get started</a>
        <a href="{link("docs", "examples")}">Examples</a>
        <a href="{docs_rs}">API reference</a>
        <a href="{link("docs", "errors")}">Error codes</a>
        {footer_docs()}
        <a href="{asset("llms.txt")}">llms.txt</a>
      </nav>
      <nav class="ftr-col" aria-label="Project">
        <h2>Project</h2>
        <a href="{link("crates")}">Crates</a>
        <a href="{link("benchmarks")}">Benchmarks</a>
        <a href="{link("roadmap")}">Roadmap</a>
        <a href="{link("releases")}">Releases</a>
      </nav>
      <nav class="ftr-col" aria-label="Community">
        <h2>Community</h2>
        <a href="{REPO}">GitHub</a>
        <a href="{X_URL}">X · @giongrun</a>
        <a href="{REPO}/issues">Issues</a>
        <a href="{BLOB}CONTRIBUTING.md">Contributing</a>
        <a href="{SECURITY}">Report a vulnerability</a>
      </nav>
    </div>
    <div class="ftr-base">
      <span>© 2026 <a href="{AUTHOR_URL}">{AUTHOR}</a> and the giong contributors. MIT or Apache-2.0.</span>
      <span class="ramp" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i></span>
    </div>
  </div>
</footer>'''
    full_title = title if page == "home" else f"{title} · giong"
    return f'''<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>{html.escape(full_title)}</title>
<meta name="description" content="{html.escape(desc)}">
<link rel="canonical" href="{canonical}">
<meta name="theme-color" content="#0A0C10">
<meta property="og:type" content="website">
<meta property="og:site_name" content="giong">
<meta property="og:title" content="{html.escape(full_title)}">
<meta property="og:description" content="{html.escape(desc)}">
<meta property="og:url" content="{canonical}">
<meta property="og:image" content="{SITE}assets/social-preview.png">
<meta property="og:image:width" content="1280">
<meta property="og:image:height" content="640">
<meta property="og:image:alt" content="The giong logo: a segmented ember ring">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:site" content="@giongrun">
<meta name="twitter:title" content="{html.escape(full_title)}">
<meta name="twitter:description" content="{html.escape(desc)}">
<meta name="twitter:image" content="{SITE}assets/social-preview.png">
<meta name="author" content="{AUTHOR}">
<link rel="alternate" type="application/atom+xml" title="giong releases" href="{REPO}/releases.atom">
{json_ld(page, full_title, desc, canonical)}
<link rel="icon" href="{asset("assets/favicon.ico")}" sizes="any">
<link rel="icon" href="{asset("assets/giong-icon.svg")}" type="image/svg+xml">
<link rel="apple-touch-icon" href="{asset("assets/giong-icon-180.png")}">
<link rel="preload" href="{asset("fonts/inter-latin.woff2")}" as="font" type="font/woff2" crossorigin>
<link rel="stylesheet" href="{asset("styles.css")}">
<script>(function(d){{d.classList.add("js");try{{var t=localStorage.getItem("giong-theme");if(t==="light"||t==="dark")d.setAttribute("data-theme",t)}}catch(e){{}}}})(document.documentElement)</script>
</head>
<body class="page-{page.replace('/', '-')}">
<a class="skip" href="#main">Skip to content</a>
{header}
<main id="main">
{body}
</main>
{footer}
<script src="{asset("main.js")}" defer></script>
</body>
</html>
'''


def json_ld(page, title, desc, url):
    author = {"@type": "Person", "name": AUTHOR, "url": AUTHOR_URL}
    graph = [{
        "@type": "SoftwareSourceCode", "name": "giong", "description": C.HERO["lede"], "url": SITE,
        "codeRepository": REPO, "programmingLanguage": "Rust", "runtimePlatform": C.RUNTIME_PLATFORM,
        "version": VERSION, "author": author, "maintainer": author,
        "license": ["https://opensource.org/licenses/MIT", "https://www.apache.org/licenses/LICENSE-2.0"],
        "image": SITE + "assets/social-preview.png",
        "keywords": "io_uring, async runtime, thread-per-core, Rust, Linux, AI agents",
    }] if page == "home" else [{"@type": "WebPage", "name": title, "description": desc, "url": url}]
    if page == "docs":
        graph.append({"@type": "FAQPage", "mainEntity": [
            {"@type": "Question", "name": q,
             "acceptedAnswer": {"@type": "Answer", "text": re.sub(r"[`*]", "", a)}} for q, a in C.FAQ]})
    data = {"@context": "https://schema.org", "@graph": graph}
    text = json.dumps(data, ensure_ascii=False, separators=(",", ":")).replace("</", "<\\/")
    return f'<script type="application/ld+json">{text}</script>'


def figures(figs):
    """Big numbers in a row, the first in ember: [(value, what it counts), ...]."""
    return ('<div class="rm-figs">' + "".join(
        f'<div class="rm-fig"><b>{v}</b><span>{ilm(d)}</span></div>' for v, d in figs) + "</div>")


def page_head(eyebrow, h1, lede, meta="", figs=None):
    meta_html = f'<p class="page-meta">{meta}</p>' if meta else ""
    figs_html = figures(figs) if figs else ""
    return (f'<section class="page-head"><div class="wrap"><p class="eyebrow">{eyebrow}</p>'
            f'<h1>{h1}</h1><p class="lede">{ilm(lede)}</p>{figs_html}{meta_html}</div></section>')


def source_note(files):
    links = " · ".join(f'<a href="{BLOB}{f}">{f}</a>' for f in files)
    return f"Generated from {links}"


# ================================================================ pages
def scoreboard():
    """Each giong crate against the Tokio crate it replaces: two bars per row, the
    longer one full width, and the losses underneath."""
    sb = C.SCOREBOARD
    rows = []
    for r in sb["rows"]:
        job, ours, theirs, g, o, gl, ol, verdict, unit, src = r[:10]
        lower = len(r) > 10 and r[10] == "lower"
        top = max(g, o)
        rows.append(
            f'<li class="vs-item{" is-lower" if lower else ""}"><div class="vs-head"><h3>{ilm(job)}</h3>'
            f'<b class="vs-x">{verdict}</b></div>'
            f'<p class="vs-bar is-giong"><span class="vs-who">{ilm(ours)}</span>'
            f'<span class="vs-track"><span class="vs-fill" style="--v:{100 * g / top:.1f}"></span></span>'
            f'<span class="vs-num">{gl}</span></p>'
            f'<p class="vs-bar"><span class="vs-who">{ilm(theirs)}</span>'
            f'<span class="vs-track"><span class="vs-fill" style="--v:{100 * o / top:.1f}"></span></span>'
            f'<span class="vs-num">{ol}</span></p>'
            f'<p class="vs-unit">{ilm(unit)} · <a href="{BLOB}{src}">{src}</a></p></li>')
    losses = "".join(f"<li>{ilm(x)}</li>" for x in sb["losses"])
    return f'''<section class="band" id="versus">
  <div class="wrap">
    {head_block("head to head", sb["h2"], sb["lede"], (link("docs/benchmark-tables") if "benchmark-tables" in VERSION_SLUGS[""] else link("benchmarks"), "Every table"))}
    <ol class="vs-grid" data-reveal>{"".join(rows)}</ol>
    <div class="losses"><h3>Where giong loses</h3><ul>{losses}</ul></div>
    <p class="chart-src">{ilm(sb["foot"])}</p>
  </div>
</section>'''


def build_band():
    """The measured build of a small service, typed out in a terminal, and the table."""
    b = C.BUILD
    lines = []
    for kind, text in b["term"]:
        t = html.escape(text)
        if kind == "cmd":
            lines.append(f'<span class="tl" data-cmd><span class="p">$</span> <span class="tl-t">{t}</span></span>')
        elif kind == "note":
            lines.append(f'<span class="tl c">{t}</span>')
        elif kind == "ok":
            lines.append(f'<span class="tl ok">{t}</span>')
        else:
            lines.append(f'<span class="tl">{t}</span>')
    rows = "".join(
        f'<li><span class="bd-what">{ilm(w)}</span>'
        f'<span class="bd-bars"><span class="bd-bar is-giong" style="--v:{100 * gv / max(gv, av):.1f}"><i>{g}</i></span>'
        f'<span class="bd-bar" style="--v:{100 * av / max(gv, av):.1f}"><i>{a}</i></span></span></li>'
        for w, g, a, gv, av in b["rows"])
    return f'''<section class="band band-alt" id="build">
  <div class="wrap build">
    {head_block("build", b["h2"], b["lede"])}
    <figure class="term term-typed" id="typed">
      <div class="term-bar"><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span>a checkout of giong</span></div>
      <pre><code>{"".join(lines)}</code></pre>
    </figure>
    <div class="bd">
      <p class="bd-key"><span class="is-giong">giong-web</span><span>axum + tokio + tower-http</span><span class="bd-low">lower is better</span></p>
      <ul class="bd-rows" data-reveal>{rows}</ul>
      <p class="chart-src">{ilm(b["foot"])} <a href="{BLOB}{b["src"]}">{b["src"]}</a></p>
    </div>
  </div>
</section>'''


def devkit_band():
    """The giong command on the home page, once a release has it: three steps and a fact."""
    if not any(e[0] == "cli" for e in DOCS):
        return ""
    d = C.DEVKIT
    steps = "".join(
        f'<article class="nx nx-mini dk-step"><header class="nx-top"><span class="nx-badge">{n + 1}</span>'
        f'<b class="nx-ver">{html.escape(title)}</b></header>{copy_cmd(f"dk-{n}", cmd)}'
        f'<p class="nx-line">{ilm(text)}</p></article>'
        for n, (title, cmd, text) in enumerate(d["steps"]))
    facts = "".join(f'<li><span>{ilm(t)}</span></li>' for t in d["facts"])
    return f'''<section class="band" id="devkit">
  <div class="wrap">
    {head_block("dev kit", d["h2"], d["lede"], (link("docs/cli"), "Every command"))}
    <div class="nexts nexts-3">{steps}</div>
    <ul class="nx-points dk-facts">{facts}</ul>
  </div>
</section>'''


def devkit_bench():
    """The benchmarks page's dev kit section, once a release has the giong command."""
    if not any(e[0] == "cli" for e in DOCS):
        return ""
    d = C.DEVKIT_BENCH
    blocks = []
    for t in d["tables"]:
        head = "".join(f"<th>{ilm(h)}</th>" for h in t["head"])
        rows = "".join("<tr>" + "".join(f"<td>{ilm(v)}</td>" for v in r) + "</tr>" for r in t["rows"])
        blocks.append(
            f'<h3>{t["title"]}</h3><p>{ilm(t["note"])}</p>'
            f'<div class="table-scroll" tabindex="0" role="region" aria-label="{html.escape(t["title"])}">'
            f'<table class="cmp"><thead><tr>{head}</tr></thead><tbody>{rows}</tbody></table></div>'
            f'<p class="chart-src">Method and raw data: <a href="{BLOB}{t["src"]}">{t["src"]}</a></p>')
    return f'''<section class="band" id="devkit"><div class="wrap dk-bench">
  {head_block("dev kit", d["h2"], d["lede"])}
  {"".join(blocks)}
</div></section>'''


def next_band():
    """Where the next releases are going: the first milestones that have not shipped."""
    ahead = [m for m in MILESTONES if m["state"] != "done" and m["ver"]][:4]
    if not ahead:
        return ""
    n = C.NEXT
    cards = []
    for m in ahead:
        kind, badge = ms_kind(m)
        pts = "".join(f'<li class="is-{k}">{ICONS["spark"] if k == "section" else ""}<span>{t}</span></li>'
                      for k, t in ms_points(m, 2))
        cards.append(
            f'<a class="nx nx-mini is-{kind}" href="{link("roadmap", m["id"])}">'
            f'<header class="nx-top"><span class="nx-badge">{badge}</span><b class="nx-ver">{ms_label(m)}</b></header>'
            f'<h3>{ms_name(m)}</h3><p class="nx-line">{ms_tagline(m)}</p><ul class="nx-points">{pts}</ul></a>')
    return f'''<section class="band" id="next">
  <div class="wrap">
    {head_block("roadmap", n["h2"], n["lede"], (link("roadmap"), "The whole roadmap"))}
    <div class="nexts nexts-4">{"".join(cards)}</div>
  </div>
</section>'''


def home():
    t = C.HERO
    sys_rows = {r[0]: r for r in next(c for c in C.CHARTS if c["id"] == "syscalls")["rows"] if not isinstance(r, str)}
    marks_e = "".join('<i class="on"></i>' if n < 14 else "<i></i>" for n in range(20))
    marks_g = '<i class="on"></i>' + "<i></i>" * 19
    announce = ""
    if LATEST:
        name = milestone_name(LATEST["version"])
        label = f'giong {LATEST["version"]}' + (f' “{name}”' if name else "") + " is out"
        announce = (f'<a class="announce" href="{link("releases", LATEST["id"])}"><b>New</b>'
                    f'<span>{label}</span><span class="announce-date">{nice_date(LATEST["date"])}</span>'
                    f'<span class="announce-go" aria-hidden="true">→</span></a>')
    stats = [
        (sys_rows["giong"][2], f"system calls per request on `/json`; raw Tokio makes {sys_rows['tokio'][2]}"),
        (str(len(CRATES)), "crates on one version: the runtime and what a service needs on top"),
        (str(ERROR_COUNT), "error codes, each with the cause, the fix and a page of its own"),
        (f"{TOTAL_TESTS:,}", "tests, run on three io_uring setups, on epoll and on real 5.15, 6.0 and 6.1 kernels"),
    ]
    stats_html = "".join(f'<div class="stat"><b>{v}</b><span>{ilm(d)}</span></div>' for v, d in stats)
    drawn = {"One syscall per tick": art.syscalls, "Idle connections hold no buffer": art.buffers,
             "Cancel safe by construction": art.cancel, "Shared nothing": art.cores}
    how = "".join(
        f'<article class="how-card"><div class="ill-wrap">{drawn[h]()}</div>'
        f'<div class="how-text"><p class="tag">{tag}</p><h3>{h}</h3><p>{ilm(p)}</p></div></article>'
        for tag, h, p in C.PILLARS if h in drawn)
    pillars = "".join(
        f'<article class="pillar"><p class="tag">{tag}</p><h3>{h}</h3><p>{ilm(p)}</p></article>'
        for tag, h, p in C.PILLARS if h not in drawn)
    charts = "".join(chart(c, home=True) for c in C.CHARTS if c.get("home"))
    a = C.AGENTS
    points = "".join(f'<li><h3>{h}</h3><p>{ilm(p)}</p></li>' for h, p in a["points"])
    m = C.MCP
    mcp_steps = "\n".join(f'<span class="c"># {i}. {html.escape(h)}</span>\n'
                          + ("" if cmd.startswith("let ") else '<span class="p">$</span> ')
                          + html.escape(cmd) + "\n"
                          for i, (h, cmd) in enumerate(m["setup"], 1))
    mcp_tools = "".join(f'<li><h3><code>{h}</code></h3><p>{ilm(p)}</p></li>' for h, p in m["tools"])
    checked = "".join(f"<li>{ilm(x)}</li>" for x in C.CHECKED)
    not_yet = "".join(f"<li>{ilm(x)}</li>" for x in C.NOT_YET)
    body = f'''<section class="hero">
  <div class="wrap hero-grid">
    <div class="hero-copy">
      {announce}
      <h1>{t["h1"]}</h1>
      <p class="lede">{ilm(t["lede"])}</p>
      <div class="cta">
        <a class="btn btn-main" href="{link("docs")}">Get started</a>
        <a class="btn" href="{link("benchmarks")}">See the benchmarks</a>
      </div>
      {copy_cmd("hero-cmd", C.DEVKIT["hero"] if any(e[0] == "cli" for e in DOCS) else "cargo add giong")}
      <p class="meta"><span>{C.RUNTIME_PLATFORM}</span><span>Rust 1.80+</span><span>MIT or Apache-2.0</span></p>
    </div>
    <figure class="tick" id="tick" data-unit1="syscall this tick" data-unitn="syscalls this tick">
      <div class="tick-ring" role="img" aria-label="Illustration: the operations of one turn of the event loop enter the kernel in one system call.">
        <svg viewBox="0 0 400 400" aria-hidden="true">{ring()}</svg>
        <div class="tick-core" aria-hidden="true">
          <span class="tick-ops"><b data-ops>14</b> operations queued</span>
          <span class="tick-num" data-sys>1</span>
          <span class="tick-unit" data-unit>syscall this tick</span>
        </div>
      </div>
      <div class="vs" aria-hidden="true">
        <div class="vs-row is-giong"><span>giong</span><span class="vs-marks" data-marks-giong>{marks_g}</span><b data-n-giong>1</b></div>
        <div class="vs-row"><span>epoll style</span><span class="vs-marks" data-marks-epoll>{marks_e}</span><b data-n-epoll>14</b></div>
      </div>
      <figcaption>One turn of the event loop, drawn to explain the mechanism.</figcaption>
    </figure>
  </div>
</section>

<section class="stats-band" aria-label="giong in numbers">
  <div class="wrap stats">{stats_html}</div>
</section>

{devkit_band()}

{scoreboard()}

{build_band()}

<section class="band" id="agents">
  <div class="wrap agents">
    {head_block("agent-first", a["h2"], a["lede"])}
    <div class="agents-side">
      <figure class="term">
        <div class="term-bar"><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span>cargo run</span></div>
        <pre><code><span class="p">$</span> cargo run
<span class="c">   Running `target/debug/shop`</span>
Error: <span class="e">{html.escape(a["error"].replace("{version}", VERSION))}</span></code></pre>
      </figure>
      <div class="files">
        <a href="{BLOB}AGENTS.md"><b>AGENTS.md</b><span>how to build, test and change giong</span></a>
        <a href="{asset("llms.txt")}"><b>llms.txt</b><span>where a model finds the docs</span></a>
        <a href="{link("docs", "errors")}"><b>{ERROR_COUNT} error codes</b><span>each with a page: cause and fix</span></a>
      </div>
    </div>
    <ol class="points points-5">{points}</ol>
  </div>
</section>

<section class="band band-alt" id="mcp">
  <div class="wrap agents">
    {head_block("giong-mcp", m["h2"], m["lede"], (f"{BLOB}giong-mcp/README.md", "giong-mcp README"))}
    <div class="agents-side">
      <figure class="term">
        <div class="term-bar"><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span>three steps</span></div>
        <pre><code>{mcp_steps}</code></pre>
      </figure>
      <figure class="term">
        <div class="term-bar"><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span>overview → warnings</span></div>
        <pre><code><span class="e">{html.escape(m["warning"])}</span></code></pre>
      </figure>
    </div>
    <ol class="points points-5">{mcp_tools}</ol>
    <p class="head-lede mcp-foot">{ilm(m["foot"])}</p>
  </div>
</section>

<section class="band" id="why">
  <div class="wrap">
    {head_block("why giong", "Built around the kernel's own queue",
                "giong commits to io_uring and to one thread per core, and to nothing else. These are the six things that buys.")}
    <div class="how">{how}</div>
    <div class="pillars pillars-2">{pillars}</div>
  </div>
</section>

<section class="band band-alt" id="stack">
  <div class="wrap">
    {head_block("the stack", "A whole service, without Tokio underneath",
                f"{len(CRATES)} crates, one version number. The runtime at the bottom, and every layer a service needs on top of it, written for it.",
                (link("crates"), "All crates"))}
    {stack()}
  </div>
</section>

<section class="band" id="code">
  <div class="wrap">
    {head_block("code", "What the code looks like",
                "Plain `async fn`s and a builder. No macros. Every worker binds its own listener on the same port and the kernel spreads connections across cores.",
                (link("docs"), "Get started"))}
    {code_window(C.CODE)}
  </div>
</section>

<section class="band band-alt" id="numbers">
  <div class="wrap">
    {head_block("benchmarks", "Measured, with the script next to every number",
                "One core per server, loopback, medians of five rounds on a 4-vCPU virtual machine. Small responses at 50 connections are a tie with tuned Monoio and raw Tokio; these charts show where the design does pay off.",
                (link("benchmarks"), "All results and caveats"))}
    <div class="charts charts-3">{charts}</div>
  </div>
</section>

<section class="band band-alt" id="status">
  <div class="wrap">
    {head_block("status", "Where giong stands",
                f"Version {MINOR} is complete enough to write real services, and it is checked hard. It is also young, tuned for Linux first, and narrow on purpose.",
                (link("docs", "limits"), "When giong is the wrong choice"))}
    <div class="status">
      <div><h3>Checked</h3><ul class="checks">{checked}</ul></div>
      <div><h3>Not yet</h3><ul class="gaps">{not_yet}</ul></div>
    </div>
  </div>
</section>

{next_band()}

<section class="band start-band">
  <div class="wrap start">
    <div>
      <p class="eyebrow">get started</p>
      <h2>Start with one command</h2>
      <p class="head-lede">The crates are on crates.io. Add the runtime, then the layers you need.</p>
      <div class="start-cmds">{copy_cmd("start-cmd", "cargo add giong giong-web")}</div>
      <div class="cta">
        <a class="btn btn-main" href="{link("docs")}">Read the guide</a>
        <a class="btn" href="{docs_rs_link()}">API reference</a>
        <a class="btn" href="{REPO}">{ICONS["github"]}<span>Star on GitHub</span></a>
      </div>
    </div>
    <p class="legend">Named after <strong>Thánh Gióng</strong>, the child of Vietnamese legend who lay
    silent for three years and then rose a giant. giong costs next to nothing while idle and uses
    every core the moment load arrives.</p>
  </div>
</section>'''
    return frame("home", "giong: the Rust async runtime built for the agent era",
                 C.HERO["lede"], body)


def docs_rs_link(name="giong"):
    return f"https://docs.rs/{name}"


# ================================================================ documentation pages
# One page per crate, made from its README, and the pages in content.py DOC_PAGES. All
# are read from the release tag (released()), so the site documents what `cargo add`
# installs, and a page appears once a release has its file. Every page also exists as
# Markdown (docs/<slug>.md) for agents, and every section has a copy button.
SKILL_MD = released("skills/giong/SKILL.md")


def doc_entries():
    """[(slug, title, path, src_dir, group, lede, crate)] for every page the release has."""
    out = [("get-started", "Get started", None, None, "Start", "", None)]
    for slug, title, path, group, lede in C.DOC_PAGES:
        if group == "Start" and released(path) is not None:
            out.append((slug, title, path, str(Path(path).parent), group, lede, None))
    if SKILL_MD is not None:
        out.append(("agent-skill", "Agent skill", "skills/giong/SKILL.md", "skills/giong", "Start",
                    C.SKILL["lede"], None))
    layer_group = {"runtime": "Runtime", "protocols": "Protocols", "clients": "Clients",
                   "framework": "Framework", "tooling": "Tooling"}
    for c in CRATES:
        path = "README.md" if c["dir"] == "." else f"{c['dir']}/README.md"
        if released(path) is None or released(release.manifest(c["dir"])) is None:
            continue
        if c["name"] == "giong-cli" and cli_commands(released(path)):
            out += cli_entries(path, c)
            continue
        out.append((c["name"], c["name"], path, c["dir"], layer_group.get(c["layer"], "Tooling"),
                    c.get("lede", c["short"]), c))
    for slug, title, path, group, lede in C.DOC_PAGES:
        if group != "Start" and released(path) is not None:
            out.append((slug, title, path, str(Path(path).parent), group, lede, None))
    if any(e[0] == "errors" for e in out):
        out += error_entries()
    return out


CLI_HEADING = re.compile(r"^## (giong ([a-z][a-z0-9-]*)(?: [a-z][a-z0-9 -]*)?)\s*$", re.M)


def cli_commands(readme):
    """[(command, heading)] of every `## giong <command>` section of giong-cli's README."""
    if not readme:
        return []
    found = dict((m.group(2), m.group(1)) for m in CLI_HEADING.finditer(readme))
    order = [c for c in C.CLI_ORDER if c in found] + [c for c in found if c not in C.CLI_ORDER]
    return [(c, found[c]) for c in order]


def cli_entries(path, crate):
    """The Dev kit section: /docs/cli/ (the README without its command sections) and
    /docs/cli/<command>/ for each `## giong <command>` section, so every command has its
    own page and address while giong-cli's README stays the one source."""
    out = [("cli", "Dev kit overview", path, crate["dir"], "Dev kit", C.CLI_LEDE, None)]
    for cmd, heading in cli_commands(released(path)):
        lede = C.CLI_COMMANDS.get(cmd, "")
        out.append((f"cli/{cmd}", heading, f"{path}#{md.slug(heading)}", crate["dir"], "Dev kit", lede, None))
    return out


def cli_source(readme, section):
    """One command's section of giong-cli's README as a page of its own (`### ` become
    `## `), or with section "" the README without its command sections and with a table
    linking every command's page."""
    readme = readme.replace("\r\n", "\n")
    # In-README links to a command section point at that command's page.
    readme = re.sub(r"\]\(#(giong-[a-z0-9-]+)\)", r"](README.md#\1)", readme)
    parts, cur, fence = [], ["", []], None
    for line in readme.split("\n"):
        m = re.match(r"^\s*(```+|~~~+)", line)
        if m:
            fence = None if fence and m.group(1).startswith(fence) else (fence or m.group(1))
        if fence is None and not m and line.startswith("## "):
            parts.append(cur)
            cur = [line[3:].strip(), []]
        cur[1].append(line)
    parts.append(cur)
    if section:
        for heading, lines in parts:
            if heading and md.slug(heading) == section:
                body = "\n".join(lines[1:])
                return re.sub(r"^### ", "## ", body, flags=re.M).strip() + "\n"
        return None
    commands = {md.slug(h) for _, h in cli_commands(readme)}
    keep = [lines for heading, lines in parts if md.slug(heading) not in commands and heading != "License"]
    rows = "".join(f"| [`{h}`](README.md#{md.slug(h)}) | {C.CLI_COMMANDS.get(c, '')} |\n"
                   for c, h in cli_commands(readme))
    table = f"## Commands\n\n| Command | What it does |\n|---|---|\n{rows}\n"
    head, rest = "\n".join(keep[0]), "\n".join("\n".join(x) for x in keep[1:])
    return f"{head.rstrip()}\n\n{table}{rest}\n"


def source(path):
    """A documentation page's Markdown at the release: a file, or `file#section` for one
    command of giong-cli's README."""
    file, _, section = path.partition("#")
    raw = released(file)
    if raw is None or file != "giong-cli/README.md" or not cli_commands(raw):
        return raw
    return cli_source(raw, section)


def error_entries():
    """A page per error code, /docs/errors/E0001/, from docs/errors/E0001.md at the tag.
    They are in no sidebar group: the error index links to every one."""
    out = []
    index = released("docs/errors/README.md") or ""
    for code in dict.fromkeys(re.findall(r"\]\((E\d{4})\.md\)", index)):
        path = f"docs/errors/{code}.md"
        src = released(path)
        if src is None:
            continue
        h = re.search(r"^#\s+(.+)$", src, re.M)
        title = h.group(1).strip() if h else f"GIONG-{code}"
        body = src.replace("\r\n", "\n").split("\n")
        first = next((ln for ln in body if ln.strip() and not ln.startswith("#")), "")
        out.append((f"errors/{code}", title, path, "docs/errors", "Errors", first, None))
    return out


DOCS = doc_entries()
DOC_BY_PATH = {e[2]: e for e in DOCS if e[2]}


# The version whose documentation is being built: "" for the newest release, at /docs/,
# or an older minor version such as "1.2", at /docs/1.2/ (build_version()).
DOC_PREFIX = ""


def doc_page_name(slug, prefix=None):
    base = "docs" + (f"/{DOC_PREFIX if prefix is None else prefix}" if (DOC_PREFIX if prefix is None else prefix) else "")
    return base if slug == "get-started" else f"{base}/{slug}"


def md_file(slug, prefix=None):
    return f"{doc_page_name(slug, prefix)}{'/index' if slug == 'get-started' else ''}.md"


def doc_url(slug, prefix=None):
    return f"{SITE}{doc_page_name(slug, prefix)}/"


def doc_link(url, src_dir):
    """A link inside a documentation page: to another page of the site when the target
    is one, else to the repository at the release tag."""
    if re.match(r"^[a-z]+:", url) or url.startswith("#"):
        return url
    target, _, anchor = url.partition("#")
    path = (Path(src_dir) / target).as_posix() if src_dir not in ("", ".", None) else target
    path = re.sub(r"(^|/)\./", r"\1", path)
    while "/../" in path or path.startswith("../"):
        new = re.sub(r"[^/]+/\.\./", "", path, count=1)
        if new == path:
            break
        path = new
    if path.endswith("/") and path + "README.md" in DOC_BY_PATH:
        path += "README.md"
    if anchor and f"{path}#{anchor}" in DOC_BY_PATH:
        return link(doc_page_name(DOC_BY_PATH[f"{path}#{anchor}"][0]))
    if anchor and path == "giong-cli/README.md":
        # A command this release's README does not have yet: the README at the tag.
        return f"{REL_BLOB}{path}#{anchor}"
    if path in DOC_BY_PATH:
        return link(doc_page_name(DOC_BY_PATH[path][0]), anchor)
    return (REL_TREE if path.endswith("/") else REL_BLOB) + path + (f"#{anchor}" if anchor else "")


def highlight(lang, text):
    if lang.startswith("rust") or lang in ("rs",):
        return rust(text)
    if lang in ("sh", "bash", "shell", "console", "dockerfile", "toml", "yaml", "yml", ""):
        return shell(text) if lang in ("sh", "bash", "shell", "console") else html.escape(text, quote=False)
    return html.escape(text, quote=False)


def clean_md(src):
    """A repository document as a page shows it: no front matter (a skill's), no HTML
    blocks (the README's logo and badges), no `# Title` (the page has its own)."""
    src = src.replace("\r\n", "\n")
    if src.startswith("---\n"):
        end = src.find("\n---\n", 4)
        if end > 0:
            src = src[end + 5:]
    out, fence, skip, titled = [], None, False, False
    for line in src.split("\n"):
        m = re.match(r"^\s*(```+|~~~+)", line)
        if m:
            fence = None if fence and m.group(1).startswith(fence) else (fence or m.group(1))
        elif fence is None:
            if skip:
                skip = line.strip() != ""
                continue
            if re.match(r"^<[A-Za-z!]", line):
                skip = True
                continue
            if not titled and re.match(r"^#\s", line):
                titled = True
                continue
        out.append(line)
    return re.sub(r"\n{3,}", "\n\n", "\n".join(out)).strip() + "\n"


def split_sections(src):
    """(intro, [(heading, markdown)]) split at `## ` headings outside code fences."""
    intro, secs, cur, fence = [], [], None, None
    for line in src.split("\n"):
        m = re.match(r"^\s*(```+|~~~+)", line)
        if m:
            fence = None if fence and m.group(1).startswith(fence) else (fence or m.group(1))
        if fence is None and not m and line.startswith("## "):
            cur = [line[3:].strip(), [line]]
            secs.append(cur)
            continue
        (cur[1] if cur else intro).append(line)
    return "\n".join(intro).strip(), [(h, "\n".join(b).strip()) for h, b in secs]


def md_script(ident, text):
    """Raw Markdown kept in the page for the copy buttons."""
    safe = text.replace("</", "<\\/")
    return f'<script type="text/markdown" id="{ident}">{safe}</script>'


def doc_sidebar(current):
    if current.startswith("errors/"):
        current = "errors"
    groups = {}
    for e in DOCS:
        groups.setdefault(e[4], []).append(e)
    parts = []
    for g in C.DOC_GROUPS:
        items = groups.get(g)
        if not items:
            continue
        links = "".join(
            f'<a href="{link(doc_page_name(e[0]))}"{" aria-current=page" if e[0] == current else ""}>'
            f'{html.escape(e[1])}</a>' for e in items)
        parts.append(f'<div class="side-group"><p>{g}</p>{links}</div>')
    inner = "".join(parts)
    return (f'<nav class="side" aria-label="Documentation"><details class="side-menu" open>'
            f'<summary>Documentation</summary><div class="side-links">{inner}</div></details></nav>')


def doc_actions(slug, title, prompt_id, page_md_id):
    md_href = asset(md_file(slug))
    return (f'<div class="doc-actions">'
            f'<button class="act" type="button" data-md="{page_md_id}" data-done="copied" '
            f'title="Copy this page as Markdown, for a coding agent">{ICONS["copy"]}<span>Copy for agent</span></button>'
            f'<button class="act" type="button" data-md="{prompt_id}" data-done="copied" '
            f'title="Copy a ready prompt with the rules and this page">{ICONS["spark"]}<span>Copy prompt</span></button>'
            f'<a class="act" href="{md_href}" title="This page as plain Markdown">{ICONS["file"]}<span>.md</span></a></div>')


def prompt_for(title, url, body):
    return C.PROMPT.format(version=VERSION, title=title, url=url) + body


def doc_page(entry):
    slug, title, path, src_dir, group, lede, crate = entry
    raw = source(path)
    src = clean_md(raw)
    intro, secs = split_sections(src)
    page_url = doc_url(slug)
    md_url = SITE + md_file(slug)
    render = lambda text: md.render(text, link=lambda u: doc_link(u, src_dir), code=highlight)
    sections, toc, scripts = [], [], []
    for n, (heading, body) in enumerate(secs):
        ident = md.slug(heading)
        sid = f"md-{n}"
        scripts.append(md_script(sid, f"<!-- giong {VERSION}, {page_url}#{ident} -->\n\n{body}"))
        sections.append(
            f'<section class="md-sec"><button class="sec-copy" type="button" data-md="{sid}" data-done="copied" '
            f'title="Copy this section as Markdown">{ICONS["copy"]}<span>Copy</span></button>{render(body)}</section>')
        toc.append(f'<a href="#{ident}">{md.inline(heading)}</a>')
    page_md = f"# {title}\n\n> giong {VERSION} · {page_url} · source: {REL_BLOB}{path}\n\n" + absolute_links(src, src_dir)
    scripts.append(md_script("md-page", page_md))
    scripts.append(md_script("md-prompt", prompt_for(title, md_url, page_md)))
    meta = ""
    if crate:
        meta = (f'<div class="doc-meta">{copy_cmd("add-" + crate["name"], "cargo add " + crate["name"])}'
                f'<p class="crate-links">{crate_links(crate)}</p></div>')
    lede_html = f'<p class="lede">{ilm(lede)}</p>' if lede else ""
    intro_html = f'<div class="md-intro">{render(intro)}</div>' if intro else ""
    toc_html = (f'<nav class="toc doc-toc" aria-label="On this page"><p>On this page</p>'
                f'<div class="toc-links">{"".join(toc)}</div></nav>') if toc else ""
    extra = skill_install() if slug == "agent-skill" else ""
    extra = version_banner(slug) + extra
    body = f'''<div class="wrap docs-layout">
  {doc_sidebar(slug)}
  <article class="doc-page prose">
    <header class="doc-head"><p class="eyebrow">{group}</p><h1>{html.escape(title)}</h1>{lede_html}
      <div class="doc-bar">{version_menu(slug)}{doc_actions(slug, title, "md-prompt", "md-page")}</div>{meta}</header>
    {extra}{intro_html}{"".join(sections)}
    <p class="src-note">giong {VERSION} · from <a href="{REL_BLOB}{path}">{path}</a> at {TAG}</p>
    {"".join(scripts)}
  </article>
  {toc_html}
</div>'''
    desc = re.sub(r"[`*]", "", lede or title)[:300]
    page = frame(doc_page_name(slug), title, desc, body, doc_page_name(slug) + "/")
    if DOC_PREFIX:
        page = page.replace('<link rel="canonical"', '<meta name="robots" content="noindex">\n<link rel="canonical"', 1)
    return page, page_md


def skill_extra():
    """{reference name: (path, dir)}: the giong command's README and the guides, when the
    release has them, so an agent with the skill has the dev kit offline too."""
    out = {}
    if any(e[0] == "cli" for e in DOCS):
        out["giong-cli"] = ("giong-cli/README.md", "giong-cli")
    for e in DOCS:
        if e[4] == "Guides":
            out[e[0].replace("/", "-")] = (e[2], e[3])
    return out


def skill_install():
    s = C.SKILL
    files = (["SKILL.md", "references/best-practices.md", "references/deployment.md",
              "references/errors.md"] + [f"references/{e[0]}.md" for e in DOCS if e[6]]
             + [f"references/{n}.md" for n in skill_extra()])
    tree = "giong/\n" + "\n".join(f"  {f}" for f in files)
    return f'''<section class="skill">
  <div class="skill-cta">
    <a class="btn btn-main" href="{asset("skills/giong.zip")}" download>Download giong.zip</a>
    <button class="btn" type="button" data-md="md-skill" data-done="copied">Copy SKILL.md</button>
    <button class="btn" type="button" data-md="md-skill-prompt" data-done="copied">Copy prompt</button>
  </div>
  <div class="skill-grid">
    <div><h3>Claude Code, for every project</h3>
      <div class="code-copy"><pre class="code" tabindex="0"><code id="skill-user">{shell(s["claude_user"])}</code></pre>
      <button class="copy" type="button" data-copy="skill-user" data-done="copied">copy</button></div></div>
    <div><h3>Claude Code, for one repository</h3>
      <div class="code-copy"><pre class="code" tabindex="0"><code id="skill-project">{shell(s["claude_project"])}</code></pre>
      <button class="copy" type="button" data-copy="skill-project" data-done="copied">copy</button></div>
      <p class="hint">Commit <code>.claude/skills/giong/</code> so every agent on the repository has it.</p></div>
    <div><h3>Other agents</h3><p>{ilm(s["other"])}</p></div>
    <div><h3>What is inside</h3><pre class="code sm"><code>{html.escape(tree)}</code></pre>
      <p class="hint">Built from giong {VERSION}; download it again after an upgrade.</p></div>
  </div>
  {md_script("md-skill", SKILL_MD)}
  {md_script("md-skill-prompt", s["prompt"])}
  <h2 id="the-skill">The skill</h2>
</section>'''


def skill_zip():
    """The skill as a zip: SKILL.md and the release's documentation as references."""
    import io
    import zipfile
    files = {"giong/SKILL.md": SKILL_MD.replace("\n# giong\n", f"\n# giong\n\nBuilt from giong {VERSION} ({TAG}).\n", 1)}
    for slug, path, d in [("best-practices", "docs/BEST_PRACTICES.md", "docs"),
                          ("deployment", "docs/DEPLOYMENT.md", "docs"),
                          ("errors", "docs/errors/README.md", "docs/errors")]:
        text = released(path)
        if text is not None:
            files[f"giong/references/{slug}.md"] = absolute_links(text, d)
    for e in DOCS:
        if e[6]:
            files[f"giong/references/{e[0]}.md"] = absolute_links(released(e[2]), e[3])
    for name, (path, d) in skill_extra().items():
        text = released(path)
        if text is not None:
            files[f"giong/references/{name}.md"] = absolute_links(text, d)
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        for name in sorted(files):
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.external_attr = 0o644 << 16
            z.writestr(info, files[name].encode("utf-8"))
    return buf.getvalue()


# ================================================================ older versions
# The newest release's documentation is at /docs/; each older minor version keeps its own,
# read from its newest patch tag, at /docs/<minor>/ (/docs/1.2/giong-web/), so code pinned
# to an older release, and the agent writing it, read what that release does. Older pages
# say so at the top, link the same page in the newest version, and are kept out of search
# results and the sitemap. A checkout without tags (CI's shallow one) builds none of them.
LATEST_DOCS = DOCS


def old_versions():
    """[(minor, tag)] of every older 1.x minor version, newest first, from the tags."""
    out = git("tag", "-l", "v[0-9]*.[0-9]*.[0-9]*")
    if out is None:
        return []
    best = {}
    for tag in out.decode().split():
        m = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", tag)
        if not m:
            continue                      # release candidates have no docs of their own
        key = (int(m.group(1)), int(m.group(2)))
        if key < (1, 0) or ".".join(map(str, key)) == MINOR:
            continue
        if key not in best or int(m.group(3)) > best[key][0]:
            best[key] = (int(m.group(3)), tag)
    return [(f"{k[0]}.{k[1]}", best[k][1]) for k in sorted(best, reverse=True)
            if k < tuple(int(x) for x in MINOR.split("."))]


OLD = old_versions()
_VERSION_STATE = ("TAG", "VERSION", "REL_BLOB", "REL_TREE", "TAG_HERE", "DOCS", "DOC_BY_PATH",
                  "SKILL_MD", "DOC_PREFIX")


def use_version(minor, tag):
    """Point the documentation builders at `tag`; returns the state to put back."""
    g = globals()
    saved = {k: g[k] for k in _VERSION_STATE}
    g.update(TAG=tag, VERSION=tag[1:], REL_BLOB=f"{REPO}/blob/{tag}/", REL_TREE=f"{REPO}/tree/{tag}/",
             TAG_HERE=True, SKILL_MD=None, DOC_PREFIX=minor)
    g["DOCS"] = [e for e in doc_entries() if e[0] != "agent-skill" and e[4] != "Errors"]
    g["DOC_BY_PATH"] = {e[2]: e for e in DOCS if e[2]}
    return saved


def restore_version(saved):
    globals().update(saved)


# Which pages each version has, for the version menu: {"": newest, "1.2": ...}.
VERSION_SLUGS = {"": {e[0] for e in DOCS}}
for _minor, _tag in OLD:
    _saved = use_version(_minor, _tag)
    VERSION_SLUGS[_minor] = {e[0] for e in DOCS}
    restore_version(_saved)


def version_menu(slug):
    if not OLD:
        return ""
    def target(prefix):
        return link(doc_page_name(slug if slug in VERSION_SLUGS[prefix] else "get-started", prefix))
    current = DOC_PREFIX or MINOR
    rows = [("", f"{MINOR}", "newest")] + [(m, m, "") for m, _ in OLD]
    links = "".join(
        f'<a href="{target(p)}"{" aria-current=true" if (p or MINOR) == current else ""}>'
        f'<b>{label}</b>{f"<span>{note}</span>" if note else ""}</a>' for p, label, note in rows)
    return (f'<details class="ver-menu"><summary title="Documentation version">giong {current}</summary>'
            f'<nav class="ver-list" aria-label="Documentation versions">{links}</nav></details>')


def version_banner(slug):
    if not DOC_PREFIX:
        return ""
    there = slug if slug in VERSION_SLUGS[""] else "get-started"
    return (f'<p class="ver-note">This is the documentation of giong {DOC_PREFIX} ({TAG}). The newest '
            f'release is {MINOR}: <a href="{link(doc_page_name(there, ""))}">read this page for {MINOR}</a>.</p>')


def old_index():
    """/docs/<minor>/: the pages an older version has."""
    groups = {}
    for e in DOCS:
        if e[0] != "get-started":
            groups.setdefault(e[4], []).append(e)
    cards = "".join(
        f'<div class="doc-group"><h3>{g}</h3><div class="doc-cards">'
        + "".join(f'<a class="doc-card" href="{link(doc_page_name(e[0]))}"><b>{html.escape(e[1])}</b>'
                  f'<span>{ilm((e[6] or {}).get("short") or e[5])}</span></a>' for e in groups[g])
        + "</div></div>" for g in C.DOC_GROUPS if g in groups)
    lede = (f"Every crate's README and the guides as they were released in {TAG}, for code that "
            f"depends on giong {DOC_PREFIX}. The API reference is on docs.rs at the same version.")
    page_md = (f"# giong {DOC_PREFIX} documentation\n\n> {TAG} · {doc_url('get-started')}\n\n{lede}\n\n"
               + "".join(f"- [{e[1]}]({SITE}{md_file(e[0])}): {re.sub(r'[`*]', '', e[5])}\n"
                         for e in DOCS if e[0] != "get-started"))
    body = (f'<div class="wrap docs-layout">{doc_sidebar("get-started")}'
            f'<article class="doc-page"><header class="doc-head"><p class="eyebrow">docs · {TAG}</p>'
            f'<h1>giong {DOC_PREFIX} documentation</h1><p class="lede">{ilm(lede)}</p>'
            f'<div class="doc-bar">{version_menu("get-started")}</div></header>'
            f'{version_banner("get-started")}<div class="doc"><section class="md-sec">{cards}</section></div>'
            f'</article></div>')
    page = frame(doc_page_name("get-started"), f"Docs {DOC_PREFIX}",
                 f"The giong {DOC_PREFIX} documentation: every crate and guide as released in {TAG}.",
                 body, doc_page_name("get-started") + "/")
    page = page.replace('<link rel="canonical"', '<meta name="robots" content="noindex">\n<link rel="canonical"', 1)
    return page, page_md


def old_docs():
    """{file: text} for every older version's documentation."""
    files = {}
    for minor, tag in OLD:
        saved = use_version(minor, tag)
        try:
            page, page_md = old_index()
            files[page_file(doc_page_name("get-started"))] = page
            files[md_file("get-started")] = page_md
            for e in DOCS:
                if e[0] != "get-started":
                    page, page_md = doc_page(e)
                    files[page_file(doc_page_name(e[0]))] = page
                    files[md_file(e[0])] = page_md
        finally:
            restore_version(saved)
    return files


def footer_docs():
    names = {e[0]: e[1] for e in LATEST_DOCS}
    return "".join(f'<a href="{link(doc_page_name(s, ""))}">{names[s]}</a>'
                   for s in ("best-practices", "agent-skill") if s in names)


def plain(text):
    """Hand-written content.py text as Markdown with absolute links."""
    return absolute_links(text, "")


def docs():
    """Get started: the first page of the documentation, with the index of every other."""
    reqs = "".join(f"<div><dt>{a}</dt><dd>{ilm(b)}</dd></div>" for a, b in C.REQS)
    http = next(s for s in C.CODE if s["crate"] == "giong-http")
    examples = "".join(
        f'<article class="ex"><h3><a href="{BLOB}examples/{name}.rs">examples/{name}.rs</a></h3>'
        f'<p>{ilm(desc)}</p><pre class="code sm"><code>{html.escape(cmd)}'
        + (f'\n<span class="c">{html.escape(then)}</span>' if then else "")
        + "</code></pre></article>" for name, desc, cmd, then in C.EXAMPLES)
    folder, sdesc, scmd = C.SERVICE
    examples = (f'<article class="ex"><h3><a href="{TREE}examples/{folder}">examples/{folder}/</a></h3>'
                f'<p>{ilm(sdesc)}</p><pre class="code sm"><code>{html.escape(scmd)}</code></pre></article>') + examples
    groups = {}
    for e in DOCS:
        if e[0] != "get-started":
            groups.setdefault(e[4], []).append(e)

    def card_text(e):
        text = (e[6] or {}).get("short") or e[5]
        return ilm(text)

    index = "".join(
        f'<div class="doc-group"><h3>{g}</h3><div class="doc-cards">'
        + "".join(f'<a class="doc-card" href="{link(doc_page_name(e[0]))}"><b>{html.escape(e[1])}</b>'
                  f'<span>{card_text(e)}</span></a>' for e in groups[g])
        + "</div></div>" for g in C.DOC_GROUPS if g in groups)
    api = "".join(
        f'<a class="api" href="https://docs.rs/{c["name"]}/{VERSION}"><b>{c["name"]}</b><span>{ilm(c["short"])}</span></a>'
        for c in CRATES)
    errs = "".join(
        f'<details class="errs"{" open" if n == 0 else ""}><summary><b>{html.escape(g["name"])}</b>'
        f'<span>{len(g["rows"])} code{"" if len(g["rows"]) == 1 else "s"}</span></summary><table><tbody>'
        + "".join(f'<tr><td><a href="{doc_link(r["file"], "docs/errors")}">{r["code"]}</a></td>'
                  f'<td><code>{html.escape(r["variant"])}</code></td><td>{ilm(r["meaning"])}</td></tr>'
                  for r in g["rows"])
        + "</tbody></table></details>" for n, g in enumerate(ERRORS))
    limits = "".join(f'<article class="limit"><h3>{h}</h3><p>{ilm(p)}</p></article>' for h, p in C.LIMITS)
    elsewhere = "".join(f"<p>{ilm(p)}</p>" for p in C.ELSEWHERE)
    faq = "".join(f'<details class="qa"><summary>{q}</summary><p>{ilm(a)}</p></details>' for q, a in C.FAQ)
    install = "cargo add giong\ncargo add giong-web serde --features serde/derive   # routing, middleware, JSON"
    err_sample = C.AGENTS["error"].replace("{version}", VERSION)
    doc_list_md = "".join(
        f"\n### {g}\n\n" + "".join(f"- [{e[1]}]({SITE}{md_file(e[0])}): {re.sub(r'[`*]', '', e[5])}\n"
                                   for e in groups[g])
        for g in C.DOC_GROUPS if g in groups)
    run_sh, src_sh = C.RUN_SH, C.FROM_SOURCE_SH
    # (id, heading, html, markdown): every section is shown and copied the same way.
    secs = [
        ("requirements", "Requirements", f'<dl class="reqs">{reqs}</dl>',
         "".join(f"- **{a}**: {plain(b)}\n" for a, b in C.REQS)),
    ]
    if any(e[0] == "cli" for e in DOCS):
        cli_sh = C.CLI_START
        steps = "".join(f'<li><a href="{link(doc_page_name("cli/" + c))}"><code>giong {c}</code></a> {ilm(t)}</li>'
                        for c, t in C.CLI_START_STEPS if any(e[0] == "cli/" + c for e in DOCS))
        steps_md = "".join(f"- [`giong {c}`]({SITE}{md_file('cli/' + c)}) {plain(t)}\n"
                           for c, t in C.CLI_START_STEPS if any(e[0] == "cli/" + c for e in DOCS))
        secs.insert(0,
            ("quick", "From zero to a running service",
             f"<p>{ilm(C.CLI_START_LEDE)}</p>"
             f'<div class="code-copy"><pre class="code" tabindex="0"><code id="cli-start">{shell(cli_sh)}</code></pre>'
             '<button class="copy" type="button" data-copy="cli-start" data-done="copied">copy</button></div>'
             f'<ul class="steps">{steps}</ul><p>Every command: <a href="{link(doc_page_name("cli"))}">the dev kit</a>.</p>',
             f"{plain(C.CLI_START_LEDE)}\n\n```sh\n{cli_sh}\n```\n\n{steps_md}\n"
             f"Every command: {SITE}{md_file('cli')}\n"))
    secs += [
        ("install", "Install the crates",
         "<p>The crates are on crates.io and share one version number. Add the runtime and the "
         "layers you need; <code>giong-http</code> takes <code>--features tls</code> for HTTPS and "
         "<code>--features client</code> for its client.</p>"
         f'<div class="code-copy"><pre class="code" tabindex="0"><code id="install-cmd">{shell(install)}</code></pre>'
         '<button class="copy" type="button" data-copy="install-cmd" data-done="copied">copy</button></div>',
         f"The crates are on crates.io and share one version number, {VERSION}.\n\n```sh\n{install}\n```\n"),
        ("first", "A first server",
         "<p>Put this in <code>src/main.rs</code>. Every worker binds its own <code>SO_REUSEPORT</code> "
         "listener on the same port, and the kernel spreads connections across cores.</p>"
         f'<pre class="code" tabindex="0"><code>{rust(http["src"])}</code></pre>'
         f'<p>Then run it:</p><pre class="code" tabindex="0"><code>{shell(run_sh)}</code></pre>',
         f"`src/main.rs`:\n\n```rust\n{http['src'].strip()}\n```\n\n```sh\n{run_sh}\n```\n"),
        ("how", "What happens to a request",
         "<p>One connection, from the kernel to your handler and back. Everything stays on the worker "
         "that accepted it, and every trip into the kernel that a turn of its loop needs, for all of "
         "its connections, goes in the same <code>io_uring_enter</code>.</p>"
         f'<figure class="fig ill-wrap">{art.request()}<figcaption>The worker on core 2 owns this '
         "connection for its whole life. Its query to PostgreSQL is one more entry in the same ring, "
         "pipelined with the other tasks' queries.</figcaption></figure>",
         "One connection stays on the worker that accepted it for its whole life. Every trip into "
         "the kernel that a turn of the worker's loop needs, for all of its connections, goes in the "
         "same `io_uring_enter`; a database query is one more entry in that ring.\n"),
        ("browse", "The documentation",
         f"<p>A page for every crate, made from its README at {TAG}, and the guides. Each page has "
         "<b>Copy for agent</b> (the page as Markdown), <b>Copy prompt</b> (the page with the rules "
         "an agent needs) and a <code>.md</code> link; each section has its own copy button.</p>"
         + index,
         f"Every page is also Markdown, for agents:\n{doc_list_md}"),
        ("examples", "Examples",
         f'<p>Each one runs from a clone of the repository. <a href="{TREE}examples">examples/</a></p>'
         f'<div class="examples">{examples}</div>',
         f"- [examples/{folder}]({REL_TREE}examples/{folder}): {plain(sdesc)} `{scmd}`\n"
         + "".join(f"- [examples/{n}.rs]({REL_BLOB}examples/{n}.rs): {plain(d)} `{c}`\n"
                   for n, d, c, _ in C.EXAMPLES)),
        ("api", "API reference",
         f'<p>Every public item is documented on docs.rs, with an example where it helps.</p><div class="apis">{api}</div>',
         "".join(f"- [{c['name']}](https://docs.rs/{c['name']}/{VERSION}): {plain(c['short'])}\n" for c in CRATES)),
        ("errors", "Error codes",
         "<p>Every error giong raises starts with a code, says what went wrong and how to fix it, and "
         f"links to its page. {ERROR_COUNT} codes so far. Match on <code>giong::error::code(&amp;err)</code>, "
         f'never on the message.</p><pre class="code sm"><code>{html.escape(err_sample)}</code></pre>{errs}'
         f'<p class="src-note">{source_note(["docs/errors/README.md"])}</p>',
         "Every error giong raises starts with a code, says what went wrong and how to fix it, and links "
         f"to its page. Match on `giong::error::code(&err)`, never on the message.\n\n```text\n{err_sample}\n```\n\n"
         f"All {ERROR_COUNT} codes: {SITE}docs/errors.md\n"),
        ("limits", "When giong is the wrong choice",
         f'<div class="limits">{limits}</div><div class="elsewhere"><h3>Pick something else when</h3>{elsewhere}</div>',
         "".join(f"- **{h}**: {plain(p)}\n" for h, p in C.LIMITS) + "\n"
         + "".join(f"- {plain(p)}\n" for p in C.ELSEWHERE)),
        ("faq", "Questions people ask", f'<div class="faq">{faq}</div>',
         "".join(f"**{q}**\n\n{plain(a)}\n\n" for q, a in C.FAQ)),
        ("contribute", "Work on giong",
         f'<p>Read <a href="{BLOB}CONTRIBUTING.md">CONTRIBUTING.md</a> first; coding agents have '
         f'<a href="{BLOB}AGENTS.md">AGENTS.md</a>. Open an issue before anything larger than a fix.</p>'
         f'<pre class="code" tabindex="0"><code>{shell(src_sh)}</code></pre>',
         f"Read {BLOB}CONTRIBUTING.md first; coding agents have {BLOB}AGENTS.md.\n\n```sh\n{src_sh}\n```\n"),
    ]
    page_url = SITE + "docs/"
    have = {e[0] for e in DOCS}
    guides = [name for slug, name in (("best-practices", "the best practices"), ("agent-skill", "the agent skill"))
              if slug in have]
    lede = ("Requirements, a first server, and every page of the documentation: a page for each crate"
            + "".join(", " + g for g in guides) + ". The API reference for each crate is on docs.rs.")
    if "cli" in have:
        lede = ("A running service in a few commands, then every page of the documentation: the "
                "`giong` command, a page for each crate" + "".join(", " + g for g in guides)
                + ". The API reference for each crate is on docs.rs.")
    sections, scripts, page_parts = [], [], []
    for n, (ident, heading, inner, smd) in enumerate(secs):
        sid = f"md-{n}"
        text = f"## {heading}\n\n{smd.strip()}\n"
        page_parts.append(text)
        scripts.append(md_script(sid, f"<!-- giong {VERSION}, {page_url}#{ident} -->\n\n{text}"))
        sections.append(
            f'<section class="md-sec" id="{ident}"><button class="sec-copy" type="button" data-md="{sid}" '
            f'data-done="copied" title="Copy this section as Markdown">{ICONS["copy"]}<span>Copy</span></button>'
            f'<h2>{heading}</h2>{inner}</section>')
    page_md = (f"# Get started with giong\n\n> giong {VERSION} · {page_url}\n\n{lede}\n\n"
               + "\n".join(page_parts))
    scripts.append(md_script("md-page", page_md))
    scripts.append(md_script("md-prompt", prompt_for("giong", SITE + md_file("get-started"), page_md)))
    toc_html = "".join(f'<a href="#{i}">{h}</a>' for i, h, _, _ in secs)
    body = (f'<div class="wrap docs-layout">{doc_sidebar("get-started")}'
            f'<article class="doc-page"><header class="doc-head"><p class="eyebrow">docs · {TAG}</p>'
            f'<h1>Get started with giong</h1><p class="lede">{ilm(lede)}</p>'
            f'<div class="doc-bar">{version_menu("get-started")}'
            f'{doc_actions("get-started", "giong", "md-prompt", "md-page")}</div></header>'
            f'<div class="doc">{"".join(sections)}</div>{"".join(scripts)}</article>'
            f'<nav class="toc doc-toc" aria-label="On this page"><p>On this page</p>'
            f'<div class="toc-links">{toc_html}</div></nav></div>')
    return (frame("docs", "Get started", "How to install giong, write a first server, and find the "
                  "documentation of every crate, the best practices and the agent skill.", body, "docs/"),
            page_md)


def crate_versus(name):
    """The head-to-head rows of a crate, from the home page's table."""
    rows = []
    for r in C.SCOREBOARD["rows"]:
        job, ours, theirs, verdict = r[0], r[1], r[2], r[7]
        if name in [o.split("::")[0] for o in ours.split(" + ")]:
            rows.append(f'<li><b>{verdict}</b><span>against {ilm(theirs)} · {ilm(job)}</span></li>')
    return f'<ul class="crate-vs">{"".join(rows)}</ul>' if rows else ""


def crates():
    sections = []
    for c in CRATES:
        items = "".join(f"<li>{ilm(x)}</li>" for x in c["items"])
        facts = [f"<b>{about(c['lines'])}</b> lines"]
        if c.get("tests"):
            facts.append(f"<b>{c['tests']}</b> tests")
        if c.get("fact"):
            facts.append(ilm(c["fact"]))
        sections.append(
            f'<article class="crate" id="{c["name"]}"><div class="crate-side"><h2>{c["name"]}</h2>'
            f'<p class="crate-layer">{dict(C.LAYERS).get(c["layer"], "More")}</p>'
            f'<p class="crate-links">{crate_links(c, guide=True)}</p>'
            f'{copy_cmd("add-" + c["name"], "cargo add " + c["name"])}</div>'
            f'<div class="crate-main"><p class="crate-lede">{ilm(c["lede"])}</p>'
            + (f'<ul>{items}</ul>' if items else "")
            + crate_versus(c["name"])
            + f'<p class="crate-facts">{" · ".join(facts)}</p></div></article>')
    for c in COMING:
        sections.append(
            f'<article class="crate is-coming" id="{c["name"]}"><div class="crate-side"><h2>{c["name"]}</h2>'
            f'<p class="crate-layer">{dict(C.LAYERS).get(c["layer"], "More")} · on the roadmap</p></div>'
            f'<div class="crate-main"><p class="crate-lede">{ilm(c["short"])}</p>'
            f'<p class="crate-facts"><a href="{link("roadmap")}">See the roadmap</a></p></div></article>')
    total_lines = sum(c["lines"] for c in CRATES)
    figs = [(str(len(CRATES)), "crates on one version number"),
            (about(total_lines).replace("about ", "~"), "lines of Rust, counted from the sources"),
            (f"{TOTAL_TESTS:,}", "tests"),
            ("3", "dependencies of the runtime crate at most, on any system")]
    body = f'''{page_head("crates", f"{len(CRATES)} crates, <em>one version</em>",
                          f"Version {VERSION} of each. The runtime at the bottom, and the layers a service needs on top of it, each written for giong rather than wrapped around a Tokio crate, and each measured against the crate it replaces.",
                          "Line counts are read from the sources when the site is built.", figs)}
<section class="band band-tight"><div class="wrap">{stack(detail=True)}</div></section>
<section class="band band-alt"><div class="wrap crate-list">{"".join(sections)}</div></section>'''
    return frame("crates", "Crates", "The giong crates: the runtime, HTTP and TLS, PostgreSQL, MySQL "
                 "and Redis clients, and a web framework.", body, "crates/")


def benchmarks():
    tables = (f'<a href="{link("docs/benchmark-tables")}">benchmark tables</a>'
                    if any(e[0] == "benchmark-tables" for e in DOCS) else f'<a href="{REPO}#benchmarks">README</a>')
    charts = "".join(chart(c) for c in C.CHARTS)
    caveats = "".join(f"<li>{ilm(c)}</li>" for c in C.CAVEATS)
    cmp_head = "".join(f'<th scope="col"{" class=is-giong" if n == 1 else ""}>{h}</th>'
                       for n, h in enumerate(C.COMPARE_HEAD))
    cmp_rows = "".join(
        f'<tr><th scope="row">{row[0]}</th>'
        + "".join(f'<td{" class=is-giong" if n == 0 else ""}>{ilm(v)}</td>' for n, v in enumerate(row[1:]))
        + "</tr>" for row in C.COMPARE)
    scripts = sorted({s for c in C.CHARTS for s in c["src"]} | {"bench/web.sh", "bench/workers.sh"})
    runs = "".join(f'<a href="{BLOB}{s}"><code>{s}</code></a>' for s in scripts)
    body = f'''{page_head("benchmarks", "Measured, <em>losses included</em>", C.BENCH_METHOD,
                          f'Full tables and raw output: {tables} · <a href="{TREE}bench/results">bench/results/</a>', C.BENCH_FIGS)}
{scoreboard()}
<section class="band band-alt"><div class="wrap">
  <div class="bench-intro">
    <div class="head-text">
      <p class="eyebrow">method</p>
      <h2>How each number is measured</h2>
      <p class="head-lede">The server gets one core and the load generator another, so they never
      compete. Servers take turns in interleaved rounds, so a change in the machine's speed hits all
      of them alike. A bar is the median of five rounds; the thin line under it runs from the lowest
      round to the highest.</p>
      <a class="more" href="{BLOB}bench/run.sh">bench/run.sh <span aria-hidden="true">→</span></a>
    </div>
    <figure class="fig ill-wrap">{art.bench_setup()}</figure>
  </div>
  <div class="charts">{charts}</div>
</div></section>
{build_band()}
{devkit_bench()}
<section class="band" id="caveats"><div class="wrap caveats">
  <h2>What these numbers do not show</h2>
  <ul>{caveats}</ul>
</div></section>
<section class="band band-alt" id="compare"><div class="wrap">
  {head_block("compare", "Compared with Monoio and Glommio",
              "The other io_uring runtimes for Rust, as they stood when the benchmarks ran. Monoio and Glommio are mature projects with wider ecosystems; this table is about how each one is built, not a ranking.")}
  <div class="table-scroll" tabindex="0" role="region" aria-label="Comparison with Monoio and Glommio">
    <table class="cmp"><thead><tr>{cmp_head}</tr></thead><tbody>{cmp_rows}</tbody></table>
  </div>
</div></section>
<section class="band" id="reproduce"><div class="wrap">
  {head_block("reproduce", "Run them yourself",
              "Each script builds the servers, runs the load generator and writes its summary to `bench/results/`. Run them on a quiet Linux machine; they take minutes, not seconds.")}
  <div class="runs">{runs}</div>
</div></section>'''
    return frame("benchmarks", "Benchmarks", "giong measured against Tokio, Monoio, Glommio, axum, "
                 "Bun and Node.js, with the method and the caveats next to every number.", body, "benchmarks/")


def short_date(iso):
    try:
        d = datetime.date.fromisoformat(iso)
    except (TypeError, ValueError):
        return iso or ""
    return f"{d.strftime('%b')} {d.day}"


def release_of(m):
    """The first release of a numbered milestone, such as 0.5.0 for 0.5."""
    if not m["ver"] or m["ver"].endswith(".x"):
        return None
    found = [r for r in RELEASES if r["version"].startswith(m["ver"] + ".") and r["date"]]
    return found[-1] if found else None


def ms_label(m):
    # A track that spans several 1.x releases ("... (track R, after 1.0)") is not a crate.
    if not m["ver"] and m["title"].endswith("after 1.0)"):
        return "1.x"
    return m["ver"] or "crates"


def ms_name(m):
    return html.escape(m["name"] if m["ver"] else m["title"].replace(" (done)", ""))


def ms_kind(m):
    """What a milestone that has not shipped is: (css class, badge)."""
    if m["state"] == "releasing":
        return "releasing", "Releasing"
    if m["state"] == "current":
        return "now", "Now"
    if m.get("proposed"):
        return "proposed", "Proposed"
    return "planned", "Planned"


def unlink(src):
    """Markdown links as their text: a card's summary is also shown on other pages."""
    return re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", src)


def ms_tagline(m):
    paras = split_intro(m["body"])[0].split("\n\n")
    if m["state"] != "done" and len(paras) > 1 and paras[0].startswith("Released as"):
        paras = paras[1:]     # not out yet: say what it is, not that it shipped
    intro = re.sub(r"\s+", " ", unlink(paras[0])).strip()
    intro = re.sub(r"^Proposed(?::.*?\.(?=\s+[A-Z])|\.)\s*", "", intro)
    s = re.match(r"^(.+?[.:])(?:\s+[A-Z(`]|$)", intro)
    return ilm(s.group(1).rstrip(":") + ("." if s.group(1).endswith(":") else "")) if s else ilm(intro)


def ms_points(m, n=3):
    """The first few items of a milestone as short phrases, and each of its ### sections."""
    def lead(text):
        text = re.sub(r"\s+", " ", unlink(text)).strip()
        b = re.match(r"^\*\*(.+?)\*\*", text)
        if b:
            return b.group(1).rstrip(":.")
        tick = False
        for i, ch in enumerate(text):
            if ch == "`":
                tick = not tick
            if not tick and i > 12 and (text.startswith((": ", " (", ", ", "; ", " — "), i)):
                return text[:i]
        return text if len(text) < 90 else text[:text.rfind(" ", 0, 85)] + "…"
    top = m["body"].split("\n### ", 1)[0]
    items = re.findall(r"^[-*] \[[ xX]\] (.+(?:\n {2,}.+)*)", top, re.M)
    points = [("item", ilm(lead(t))) for t in items[:n]]
    for h in re.findall(r"^### (.+)$", m["body"], re.M):
        points.append(("section", ilm(h)))
    return points


def roadmap():
    """A track of every numbered milestone, a card for each one that has not shipped, then
    one line per shipped milestone, folded until a reader opens it."""
    shown_shipped = 4

    def md_html(src):
        h = doc_md(src, heading_shift=1, heading_ids=False)
        return h.replace("<p><strong>Exit criteria:</strong>", '<p class="exit"><strong>Done when:</strong>')

    def row(m):
        rel = release_of(m) if m["state"] == "done" else None
        if rel:
            meta = f'<time datetime="{rel["date"]}">{nice_date(rel["date"])}</time>'
        elif m["total"]:
            meta = f'{m["done"]} of {m["total"]}' if m["done"] else f'{m["total"]} items'
        else:
            meta = "details"
        notes = (f'<p class="mr-links"><a href="{link("releases", rel["id"])}">Release notes for '
                 f'{rel["version"]} <span aria-hidden="true">→</span></a></p>') if rel else ""
        line = ms_tagline(m)
        return (f'<details class="mr is-{m["state"]}" id="{m["id"]}"><summary>'
                f'<b class="mr-ver{"" if m["ver"] else " is-word"}">{ms_label(m)}</b>'
                f'<span class="mr-name">{ms_name(m)}</span><span class="mr-meta">{meta}</span>'
                + (f'<span class="mr-line">{line}</span>' if line else "")
                + f'</summary><div class="mr-body prose">{notes}{md_html(m["body"])}</div></details>')

    upcoming = [m for m in MILESTONES if m["state"] != "done"]
    shipped = [m for m in MILESTONES if m["state"] == "done"][::-1]
    lead_m = next((m for m in upcoming if m["state"] == "releasing"), None) or \
        next((m for m in upcoming if m["state"] == "current"), None)

    # The track: every numbered milestone, shipped ones lit.
    nodes = []
    for m in MILESTONES:
        if not m["ver"]:
            continue
        if m["state"] == "done":
            rel = release_of(m)
            cls, when = "done", (f'<time datetime="{rel["date"]}">{short_date(rel["date"])}</time>' if rel else "<i>shipped</i>")
        else:
            cls, badge = ms_kind(m)
            when = f"<i>{badge.lower()}</i>"
        here = ' aria-current="step"' if m is lead_m else ""
        nodes.append(f'<li class="jn is-{cls}"{here}><a href="#{m["id"]}"><span class="jn-dot" aria-hidden="true"></span>'
                     f'<b>{m["ver"]}</b><span class="jn-name">{ms_name(m)}</span>{when}</a></li>')
    journey = (f'<div class="journey-wrap"><ol class="journey" aria-label="Milestones in order">{"".join(nodes)}</ol></div>')

    def card(m):
        kind, badge = ms_kind(m)
        feature = m is lead_m
        pts = "".join(
            f'<li class="is-{k}">{ICONS["spark"] if k == "section" else ""}<span>{t}</span></li>'
            for k, t in ms_points(m, 4 if feature else 3))
        pct = 100 * m["done"] / m["total"] if m["total"] else 0
        progress = (f'<p class="nx-progress"><span class="ms-bar" style="--p:{pct:.0f}%"></span>'
                    f'<span>{m["done"]} of {m["total"]} done</span></p>') if m["total"] and kind != "releasing" else ""
        if kind == "releasing":
            progress = '<p class="nx-progress is-ready"><span class="ms-bar" style="--p:100%"></span><span>done; the release is being cut</span></p>'
        details = '<div class="prose">' + md_html(m["body"]) + '</div>'
        return (f'<article class="nx is-{kind}{" is-feature" if feature else ""}" id="{m["id"]}">'
                f'<header class="nx-top"><span class="nx-badge">{badge}</span><b class="nx-ver">{ms_label(m)}</b></header>'
                f'<h3>{ms_name(m)}</h3><p class="nx-line">{ms_tagline(m)}</p>'
                f'<ul class="nx-points">{pts}</ul>{progress}'
                + more("Every item, and what marks it done", details, "more-box nx-more")
                + '</article>')

    nexts = ""
    if upcoming:
        nexts = f'''<section class="band band-alt" id="next"><div class="wrap">
  {head_block("the road ahead", "Up next",
              "In the order they are planned. Proposed milestones are settled when the one before them ships; nothing here is a promise of a date.")}
  <div class="nexts">{"".join(card(m) for m in upcoming)}</div>
</div></section>'''

    done_rows = "".join(row(m) for m in shipped[:shown_shipped])
    if len(shipped) > shown_shipped:
        older = shipped[shown_shipped:]
        done_rows += more(f"Show {len(older)} earlier milestones",
                          f'<div class="mrs">{"".join(row(m) for m in older)}</div>', "more-box mrs-older")
    done = f'''<section class="band" id="shipped"><div class="wrap rm-col">
  <h2 class="rm-h">Shipped</h2>
  <p class="rm-sub">Newest first. Every change is in the <a href="{link("releases")}">release notes</a>.</p>
  <div class="mrs">{done_rows}</div>
</div></section>'''

    named = ROADMAP_NAMED
    a_intro, a_rest = split_intro(named.get("Agent-first", ""))
    a_count = len(re.findall(r"^\d+\. ", a_rest, re.M))
    a_more = more(f"The {a_count} principles" if a_count else "The principles",
                  f'<div class="prose">{doc_md(a_rest, heading_ids=False)}</div>') if a_rest else ""
    published = [r for r in RELEASES if r["version"] != "Unreleased" and r["date"]]
    first = published[-1] if published else None
    figs = [(str(len(shipped)), "milestones shipped"),
               (str(len(published)), "releases" + (f", the first on {nice_date(first['date'])}" if first else "")),
               (str(len(CRATES)), "crates on one version"),
               (str(len(upcoming)), "milestones ahead")]
    body = f'''<section class="page-head rm-head"><div class="wrap">
  <p class="eyebrow">roadmap</p>
  <h1>The road to the <em>best Rust runtime</em></h1>
  <p class="lede">Every milestone so far and every one planned, in order and with no dates: a milestone is done when its exit criteria are met, and a release ships only what is done.</p>
  {figures(figs)}
  <p class="page-meta">{source_note(["ROADMAP.md"])}</p>
</div>
{journey}
</section>
{nexts}
{done}
<section class="band band-alt"><div class="wrap goal">
  <div class="prose prose-lg"><h2 id="goal">Goal</h2>{doc_md(named.get("Goal", ""), heading_ids=False)}</div>
  <div class="prose"><h2 id="agent-first">Agent-first</h2>{doc_md(a_intro, heading_ids=False)}{a_more}</div>
</div></section>
<section class="band" id="non-goals"><div class="wrap nongoals">
  <h2>Deliberate non-goals</h2>
  <div class="prose">{doc_md(named.get("Non-goals", ""), heading_ids=False)}</div>
</div></section>'''
    return frame("roadmap", "Roadmap", "Where giong is going: every milestone in order, what is "
                 "next, what marks each one done, and the deliberate non-goals.", body, "roadmap/")


def releases():
    items, side = [], []
    for r in RELEASES:
        unreleased = r["version"] == "Unreleased"
        if unreleased and not r["body"].strip():
            continue
        name = "" if unreleased else milestone_name(r["version"])
        title = "Coming next" if unreleased else r["version"]
        meta = []
        if unreleased:
            meta.append("on <code>main</code>, not released yet")
        else:
            meta.append(f'<time datetime="{r["date"]}">{nice_date(r["date"])}</time>')
            meta.append(f'<a href="{REPO}/releases/tag/v{r["version"]}">GitHub release</a>')
        if r["compare"]:
            meta.append(f'<a href="{r["compare"]}">every commit</a>')
        latest = r is LATEST
        badge = '<span class="badge">latest</span>' if latest else ""
        # A release's introduction runs to its first "### " heading, lists included
        # (the breaking changes by crate, say); the entries below it are folded away.
        intro = "" if unreleased else r["intro"]
        rest = r["body"][len(intro):].strip()
        count = len(re.findall(r"^- ", rest, re.M))
        notes = f'<div class="prose">{doc_md(intro, heading_ids=False)}</div>' if intro else ""
        if rest:
            label = (f"Show the {count} entries waiting for the next release" if unreleased
                     else f'Show {"the " + str(count) + " changes" if count > 1 else "the change"} in {r["version"]}')
            notes += more(label, f'<div class="prose">{doc_md(rest, heading_ids=False)}</div>', "more-box next")
        items.append(
            f'<article class="rel{" is-next" if unreleased else ""}" id="{r["id"]}">'
            f'<header class="rel-head"><h2>{title}{badge}</h2>'
            + (f'<p class="rel-name">{html.escape(name)}</p>' if name else "")
            + f'<p class="rel-meta">{" · ".join(meta)}</p></header>{notes}</article>')
        side.append(f'<a href="#{r["id"]}">{title}<span>{nice_date(r["date"]) if r["date"] else "main"}</span></a>')
    published = [r for r in RELEASES if r["version"] != "Unreleased" and r["date"]]
    since_1 = [r for r in published if re.match(r"^1\.\d+\.\d+$", r["version"]) and r["version"] != "1.0.0"]
    figs = [(str(len(published)), "releases" + (f", the first on {nice_date(published[-1]['date'])}" if published else ""))]
    if LATEST:
        name = milestone_name(LATEST["version"])
        figs.append((LATEST["version"], "the newest" + (f", “{html.escape(name)}”" if name else "")))
    figs.append((str(len(since_1)), "releases since 1.0.0, and none needs a code change to upgrade"))
    body = f'''{page_head("releases", "Every release, <em>newest first</em>",
                          "What changed in each version. Every crate shares the version number. Since 1.0 a release only adds: the API is frozen, and `cargo-semver-checks` guards it on every change.",
                          source_note(["CHANGELOG.md"]) + f' · <a href="{REPO}/releases.atom">feed</a>', figs)}
<div class="wrap doc-grid">
  <nav class="toc" aria-label="Versions"><p>Versions</p><div class="toc-links">{"".join(side)}</div></nav>
  <div class="releases">{"".join(items)}</div>
</div>'''
    return frame("releases", "Releases", "Release notes for every version of giong, from CHANGELOG.md.",
                 body, "releases/")


def not_found():
    body = f'''<section class="lost"><div class="wrap">
  <svg class="lost-ring" viewBox="0 0 400 400" aria-hidden="true">{ring(lit=0)}</svg>
  <p class="eyebrow">404</p>
  <h1>Nothing here, <em>not even one syscall.</em></h1>
  <p class="lede">The page you asked for does not exist. Thánh Gióng lay silent for three years before he rose; this page is not coming back.</p>
  <div class="cta"><a class="btn btn-main" href="{link("home")}">Back to giong.run</a><a class="btn" href="{link("docs")}">Docs</a></div>
</div></section>'''
    return frame("404", "Not found", "This page does not exist.", body, "404").replace(
        '<link rel="canonical"', '<meta name="robots" content="noindex">\n<link rel="canonical"')


PAGES = ["home", "docs", "crates", "benchmarks", "roadmap", "releases"]


def sitemap():
    pages = PAGES + [doc_page_name(e[0]) for e in DOCS if e[0] != "get-started"]
    urls = "".join(f"  <url><loc>{SITE}{'' if p == 'home' else p + '/'}</loc></url>\n" for p in pages)
    return ('<?xml version="1.0" encoding="UTF-8"?>\n'
            '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n' + urls + '</urlset>\n')


def released_crates():
    """The crates the release has, in dependency order, with their directory."""
    return [(c, d) for c, d in ((c["name"], CRATE_DIRS[c["name"]]) for c in CRATES)
            if released(release.manifest(d)) is not None]


def llms():
    """llms.txt (https://llmstxt.org): where a language model finds the docs."""
    crates = released_crates()
    names = {name for name, _ in crates}
    def description(d):
        pkg = release.section(released(release.manifest(d)), "package") or ""
        m = re.search(r'^description\s*=\s*"([^"]*)"', pkg, re.M)
        return m.group(1) if m else ""
    api = "".join(f"- [{name} API](https://docs.rs/{name}/{VERSION}): {description(d)}\n"
                  for name, d in crates)
    examples = "".join(f"- [examples/{name}.rs]({REL_BLOB}examples/{name}.rs): {re.sub(r'[`*]', '', desc)}\n"
                       for name, desc, _, _ in C.EXAMPLES if released(f"examples/{name}.rs") is not None)
    folder, desc, _ = C.SERVICE
    if released(f"examples/{folder}/README.md") is not None:
        examples = (f"- [examples/{folder}]({REL_BLOB}examples/{folder}/README.md): "
                    f"{re.sub(r'[`*]', '', desc)}\n") + examples
    mcp = ("- To let an agent look inside a running service (which task blocks a worker, which line\n"
           "  spawned the tasks that leak, how full the pools are), add giong-mcp and keep\n"
           "  `let _mcp = giong_mcp::start_from_env(&handle);` alive (no `?`: it never fails the\n"
           "  service). It serves MCP on the address in `GIONG_MCP`; a TCP address also needs a\n"
           "  token of 16 characters or more in `GIONG_MCP_TOKEN`, which the client sends as\n"
           "  `Authorization: Bearer`, and a Unix socket needs none. Its README says how to\n"
           "  connect a client.\n") if "giong-mcp" in names else ""
    pools = ("- giong-postgres and giong-mysql: `pool.query(..)` and the other methods on a `Pool`\n"
             "  share connections between tasks, so they refuse statements that change the session\n"
             "  (`SET`, `LISTEN`, temporary tables, `USE`). Run those, and transactions, on a\n"
             "  connection from `pool.get().await?`; it comes back with its session reset.\n"
             ) if "giong-postgres" in names else ""
    if released("src/lane.rs") is not None:
        blocking = ("Never block a worker or compute on it for long:\n"
                    "  `giong::spawn_blocking(f).await` runs blocking calls (`std::fs`, C libraries) on\n"
                    "  a blocking lane, `giong::spawn_compute(f).await` runs CPU-heavy work (hashing,\n"
                    "  images, compression) on threads below the workers' priority, and\n"
                    "  `try_spawn_compute` sheds with `GIONG-E0014` when that lane is behind.")
    else:
        blocking = ("There is no `spawn_blocking`: use\n"
                    "  `std::thread::spawn` and a `giong::sync::oneshot` channel, and never block a worker.")
    have = {e[0] for e in DOCS}
    platforms = C.PLATFORM_LINE + (
        " On Linux giong runs on epoll by itself where io_uring is blocked (Docker's default "
        "seccomp profile, gVisor) or the kernel is older." if C.PLATFORM_LINE.startswith("Linux first") else
        " It runs on epoll where io_uring is blocked (Docker's default seccomp profile, gVisor) or "
        "the kernel is older. It does not build on macOS or Windows yet.")
    platforms = textwrap.fill(platforms, 88, subsequent_indent="  ")
    more_crates = ""
    if "giong-grpc" in names:
        more_crates += " gRPC: `giong-grpc` (code from `giong-grpc-build`, no `protoc`), not tonic; Protocol Buffers: `giong-proto`, not prost;"
    if "giong-quic" in names:
        more_crates += " HTTP/3: `giong-http` feature `unstable-http3` (Rust 1.85, may change), not quinn or h3;"
    bench_ref = (f"{SITE}docs/benchmark-tables.md" if "benchmark-tables" in have
                 else "the README's Benchmarks section")
    guides = ""
    if "best-practices" in have:
        guides += f"- [Best practices]({SITE}docs/best-practices.md): how to build, deploy and test a service on giong\n"
    if "agent-skill" in have:
        guides += (f"- [Agent skill]({SITE}skills/giong.zip): a skill for coding agents (SKILL.md and these "
                   f"docs as references); [SKILL.md]({SITE}skills/giong/SKILL.md)\n")
    pages = "".join(f"- [{e[1]}]({SITE}{md_file(e[0])}): {re.sub(r'[`*]', '', e[5])}\n"
                    for e in DOCS if e[0] not in ("get-started", "best-practices", "agent-skill", "errors")
                    and e[4] != "Errors" and not e[0].startswith("cli/"))
    older = ("- Older versions, for code pinned to them: "
             + ", ".join(f"[{m}]({SITE}{md_file('get-started', m)})" for m, _ in OLD) + "\n") if OLD else ""
    cli = ""
    if "cli" in have:
        rows = "".join(f"| {want} | [`{cmd}`]({SITE}{md_file('cli/' + slug)}) |\n"
                       for want, cmd, slug in C.CLI_TASKS if "cli/" + slug in have)
        cli = (f"\n## The giong command\n\n{C.CLI_LLMS}\n\n| You want | Run |\n|---|---|\n{rows}\n"
               f"Every command and its flags: {SITE}{md_file('cli')}\n")
    extra_protocols = ", HTTP/3" if "giong-quic" in names else ""
    grpc = ", gRPC" if "giong-grpc" in names else ""
    return f"""# giong

> An async runtime for Rust services, built for code that AI agents write: a strict API
> meant to stay on major version 1 for many years, errors with a stable code, the cause
> and the fix, and its own crates for a whole service (HTTP{extra_protocols}, TLS{grpc},
> PostgreSQL, MySQL, Redis, DNS) with no Tokio underneath. Thread per core on io_uring:
> every worker owns its ring, tasks and buffers; tasks never move between threads, so
> futures need not be `Send`. Version {VERSION}; every link below is to the documentation
> of that release, which is what `cargo add` installs.

Facts that are easy to get wrong:

- {platforms}
- Buffers are passed by value and handed back, because the kernel uses them after the call:
  `stream.write_all(buf).await` returns `(io::Result<()>, buf)`.
- `recv` is cancel safe: a `timeout` around it loses no data. A dropped `write_all` may
  have sent part of its buffer.
- There are no macros: start a runtime with `giong::block_on` or `giong::Builder`.
- `TcpListener::bind`, `bind_balanced`, `UdpSocket::bind` and `UdpSocket::connect` take IP
  addresses only (`"0.0.0.0:8080"`, `"[::]:8080"`); a host name such as `"localhost:8080"`
  is refused with `GIONG-E0100`. Resolve names with `giong::net::lookup_host` first;
  `TcpStream::connect` resolves them itself. The workers of one process share a port, and
  `bind` refuses a port that another process holds.
- An error giong raises starts with a code, such as `GIONG-E0102`, says how to fix it, and
  links to its page; `giong::error::code(&err)` returns the code to match on.
- giong-web: build the `Router` inside each worker's future (`run(|_| async {{ .. }})`), since
  an `App` is not `Send`. A handler is `async fn(Req<S>) -> Result<Response, Error>`; `?` on
  any other error answers `500` and logs the cause, never sending it.
- Coming from Tokio: `giong::spawn` keeps a task on its worker, so nothing needs `Send` and
  per-worker state is an `Rc<RefCell<..>>`, not an `Arc<Mutex<..>>`. `futures::join!` and
  `futures::select!` take the place of Tokio's macros. {blocking}
  `giong::sync` has `mpsc` and `oneshot`; Tokio's `sync` module (`Semaphore`, `Notify`,
  `watch`, `broadcast`) needs no Tokio runtime and works on giong.
- Child processes: `giong::process::Command` (std's builder; `.output().await`,
  `.spawn()?` then `child.wait().await`); a child's pipes and `giong::process::stdin()` /
  `stdout()` read and write owned buffers. Signals: `giong::signal::listen(SignalKind::HANGUP)?`
  then `.recv().await`. Descriptors pass between processes with
  `UnixStream::send_with_fds` / `recv_with_fds`; systemd's sockets come from
  `giong::net::activated_sockets()`.
- Choosing crates: use the giong crate for the job first. HTTP server: `giong-http`, not
  hyper or axum; routing, JSON and middleware: `giong-web`, not axum; HTTP client:
  `giong-http` with feature `client`, not reqwest; TLS: `giong-tls`; PostgreSQL:
  `giong-postgres`, not sqlx or tokio-postgres; MySQL: `giong-mysql`; Redis: `giong-redis`,
  not redis-rs;{more_crates} files: `giong::fs`; names: `giong::net::lookup_host`; timers: `giong::time`.
  Measured against those Tokio crates on Tokio, one core each, they serve 1.2 to 4.7 times
  the requests, statements or commands, and file reads at an offset 5.5 to 8 times; over one
  PostgreSQL connection shared by every task giong-postgres is behind tokio-postgres,
  so use a `Pool` ({bench_ref} has each table and the exceptions). Next, crates that need no runtime (serde, rustls, Tokio's `sync`) work as
  they are. Only for a protocol giong has no crate for (WebSocket, for one), a crate generic
  over a Tokio or futures-io stream (tokio-tungstenite, h2, tokio-rustls, hyper's `conn`)
  runs through `giong_compat::Compat::new(stream)`, which adds a copy per byte written and
  per byte read with `read`. Crates that need the Tokio runtime (sqlx, reqwest, tonic's
  transport, most cloud SDKs) do not run on giong's workers: they panic with "there is no
  reactor running".
- `giong-http` serves HTTP/2 by itself, through ALPN over TLS or with prior knowledge on the
  same port; handlers do not change. `Server::http2(false)` turns it off. A `serve` call
  keeps at most 10,000 connections open (`Server::max_connections`); past that, clients
  wait in the listen queue.
- On Linux 6.14 to 7.2 each worker's rings count against the locked-memory limit
  (`ulimit -l`, about 150 KiB per worker). Past it giong runs that worker on epoll and says
  why (`GIONG-E0010`); raise the limit as docs/DEPLOYMENT.md shows.
- In a container giong needs no setting: it falls back to epoll where io_uring is blocked,
  starts one worker per whole CPU of the CPU limit (1.5 CPUs: 1 worker), and giong-http
  refuses connections before the memory limit is reached. It raises the soft
  `RLIMIT_NOFILE` to the hard one (containerd starts containers at 1,024 files). Two things
  to do yourself: `Builder::shutdown_on_signals(true)` so that `SIGTERM` stops process 1;
  and in Kubernetes without a CPU limit, set `Builder::workers` from the CPU request.
  examples/rest-api has a Dockerfile and a compose.yaml.
{pools}{mcp}- Install with `cargo add giong` (and `giong-http`, `giong-web`, ...). Libraries need Rust 1.80.
{cli}
## Docs

{api}- [Get started]({SITE}docs/index.md): requirements, a first server, examples, limits
{guides}{pages}- [Error codes]({SITE}docs/errors.md): what each `GIONG-E` code means and how to fix it;
  one code's page is {SITE}docs/errors/E0001.md (any code in place of E0001)
- [CHANGELOG.md]({REL_BLOB}CHANGELOG.md): what changed in each release
- [Everything above in one file]({SITE}llms-full.txt): the READMEs, best practices and error pages of {TAG}
{older}
## Examples

{examples}
## Optional

- [Benchmark results]({REL_TREE}bench/results): raw numbers behind the README
- [ROADMAP.md]({BLOB}ROADMAP.md): milestones and deliberate non-goals
- [AGENTS.md]({BLOB}AGENTS.md) and [CONTRIBUTING.md]({BLOB}CONTRIBUTING.md): how to build,
  test and change giong itself, on its main branch
"""


def absolute_links(text, src_dir):
    """Markdown links and images relative to `src_dir`, made absolute at the tag."""
    def fix(m):
        url = m.group(2)
        if re.match(r"^[a-z]+:", url) or url.startswith("#"):
            return m.group(0)
        path = (Path(src_dir) / url).as_posix() if src_dir not in ("", ".") else url
        path = re.sub(r"(^|/)\./", r"\1", path)
        while "/../" in path or path.startswith("../"):
            new = re.sub(r"[^/]+/\.\./", "", path, count=1)
            if new == path:
                break
            path = new
        # A document that is a page of the site: its Markdown there, one hop for an agent.
        target, _, anchor = path.partition("#")
        page = DOC_BY_PATH.get(path) or (DOC_BY_PATH.get(target) if not anchor else None)
        if page is None and target.endswith("/"):
            page = DOC_BY_PATH.get(target + "README.md") if not anchor else None
        if page is not None:
            return f"{m.group(1)}({SITE}{md_file(page[0])})"
        return f"{m.group(1)}({(REL_TREE if path.endswith('/') else REL_BLOB) + path})"
    return re.sub(r"(\]\s?)\(([^)\s]+)\)", fix, text)


def llms_full():
    """llms-full.txt: every crate's README, the example service's, the deployment notes
    and every error page of the release, in one file, for a model that reads it whole."""
    docs = [("README.md", ".")]
    docs += [(f"{d}/README.md", d) for _, d in released_crates() if d != "."]
    docs.insert(1, ("docs/BEST_PRACTICES.md", "docs"))
    docs += [(f"examples/{C.SERVICE[0]}/README.md", f"examples/{C.SERVICE[0]}")]
    docs += [(e[2], e[3]) for e in DOCS if e[4] == "Guides"]
    docs += [("docs/DEPLOYMENT.md", "docs"), ("docs/errors/README.md", "docs/errors")]
    index = released("docs/errors/README.md") or ""
    docs += [(f"docs/errors/{page}", "docs/errors")
             for page in dict.fromkeys(re.findall(r"\]\((E\d{4}\.md)\)", index))]
    parts = [f"""# giong {VERSION}: the documentation in one file

> The README of every crate and of the example service, the best practices, the guides, the deployment notes and every
> error code page of giong {VERSION}, as released in {TAG}. Links point at the same
> release. The short index is {SITE}llms.txt.
"""]
    for path, d in docs:
        text = released(path)
        if text is None:
            continue
        parts.append(f"\n---\n\n<!-- {REL_BLOB}{path} -->\n\n{absolute_links(text.strip(), d)}\n")
    return "".join(parts)


def check_links(out, files):
    """Every link between the site's own pages, and every anchor in one, must exist."""
    ids = {name: set(re.findall(r'\sid="([^"]+)"', text)) for name, text in files.items()
           if isinstance(text, str)}
    problems = []
    for name, text in files.items():
        if not name.endswith(".html"):
            continue
        for ref in re.findall(r'(?:href|src|srcset)="([^"]+)"', text):
            if re.match(r"^[a-z]+:", ref) or ref.startswith("//"):
                continue
            path, _, anchor = ref.partition("#")
            if PREVIEW:
                target = path or name
            else:
                if path and not path.startswith("/"):
                    problems.append(f"{name}: relative link {ref!r}")
                    continue
                target = path.lstrip("/") if path else name
                if target == "" or target.endswith("/"):
                    target += "index.html"
            if target not in files and not (HERE / "public" / target).is_file() and not (out / target).is_file():
                problems.append(f"{name}: {ref!r} points at nothing")
            elif anchor and target in ids and anchor not in ids[target]:
                problems.append(f"{name}: {ref!r} names an anchor that {target} does not have")
    return problems


def build(out):
    out.mkdir(parents=True, exist_ok=True)
    docs_html, docs_md = docs()
    files = {
        page_file("home"): home(),
        page_file("docs"): docs_html,
        page_file("crates"): crates(),
        page_file("benchmarks"): benchmarks(),
        page_file("roadmap"): roadmap(),
        page_file("releases"): releases(),
        "404.html": not_found(),
        "sitemap.xml": sitemap(),
        "robots.txt": f"User-agent: *\nAllow: /\nSitemap: {SITE}sitemap.xml\n",
        "llms.txt": llms(),
        "llms-full.txt": llms_full(),
        md_file("get-started"): docs_md,
    }
    # A page per crate and guide, each also as Markdown for agents.
    for e in DOCS:
        if e[0] != "get-started":
            page_html, page_md = doc_page(e)
            files[page_file(doc_page_name(e[0]))] = page_html
            files[md_file(e[0])] = page_md
    files.update(old_docs())
    if SKILL_MD is not None:
        files["skills/giong/SKILL.md"] = SKILL_MD
        files["skills/giong.zip"] = skill_zip()
    for name, text in files.items():
        (out / name).parent.mkdir(parents=True, exist_ok=True)
        if isinstance(text, bytes):
            (out / name).write_bytes(text)
            continue
        with open(out / name, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
    missing = [n for n in CRATE_DIRS if n not in C.CRATES]
    if missing:
        print("note: no entry in content.py CRATES for " + ", ".join(missing)
              + "; the site shows their Cargo.toml description", file=sys.stderr)
    print(f"wrote {len(files)} files to {out}")
    if not TAG_HERE:
        print(f"note: {TAG} is not in this checkout, so llms.txt and llms-full.txt were made "
              "from the working tree; the deploy job fetches the tags", file=sys.stderr)
    problems = check_links(out, files)
    if problems:
        sys.exit("broken links on the site:\n  " + "\n  ".join(problems))
    return files


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    build(Path(args[0]) if args else HERE / "public")
