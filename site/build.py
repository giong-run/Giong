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
import sys
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
MINOR = ".".join(VERSION.split(".")[:2])
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
        m = re.match(r"^(\d+(?:\.\d+)*)\s+—\s+(.+?)(?:\s+\((done)\))?$", title)
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
            state = "done"
        elif done:
            state = "current"
        else:
            state = "planned"
        milestones.append(dict(title=title, ver=ver, name=name, state=state, done=done,
                               total=len(boxes), body=body.strip(), id=md.slug(title)))
    if not any(m["state"] == "current" for m in milestones):
        for m in milestones:
            if m["state"] == "planned":
                m["state"] = "current"
                break
    return intro, named, milestones


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
    """The address of one of the site's own pages."""
    if PREVIEW:
        base = "index.html" if page == "home" else f"{page}.html"
    else:
        base = "/" if page == "home" else f"/{page}/"
    return base + (f"#{anchor}" if anchor else "")


def page_file(page):
    """Where a page is written: docs/index.html, so that /docs/ works on any static host."""
    if page == "home":
        return "index.html"
    return f"{page}.html" if PREVIEW else f"{page}/index.html"


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
}

NAV = [("docs", "Docs"), ("crates", "Crates"), ("benchmarks", "Benchmarks"),
       ("roadmap", "Roadmap"), ("releases", "Releases")]


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


def crate_links(c):
    links = [("docs.rs", f"https://docs.rs/{c['name']}"),
             ("crates.io", f"https://crates.io/crates/{c['name']}"),
             ("source", REPO if c["dir"] == "." else TREE + c["dir"])]
    return " · ".join(f'<a href="{u}">{label}</a>' for label, u in links)


# ================================================================ the frame
def frame(page, title, desc, body, path=""):
    canonical = SITE + path
    current = ' aria-current="page"'
    nav = "".join(f'<a href="{link(p)}"{current if p == page else ""}>{label}</a>' for p, label in NAV)
    mark = (f'<picture><source media="(prefers-color-scheme: light)" srcset="{asset("assets/giong-mark-light.svg")}">'
            f'<img src="{asset("assets/giong-mark.svg")}" alt="" width="28" height="28"></picture>')
    release_url = link("releases", LATEST["id"]) if LATEST else link("releases")
    header = f'''<header class="hdr">
  <div class="wrap hdr-in">
    <a class="brand" href="{link("home")}" aria-label="giong, home">{mark}<span>giong</span></a>
    <nav class="nav" aria-label="Site">{nav}</nav>
    <div class="hdr-side">
      <a class="ver" href="{release_url}" title="Release notes for {TAG}">{TAG}</a>
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
        <p>A thread-per-core async runtime for Rust on Linux, built directly on io_uring.</p>
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
<body class="page-{page}">
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
        "codeRepository": REPO, "programmingLanguage": "Rust", "runtimePlatform": "Linux 6.0+",
        "version": VERSION, "author": author, "maintainer": author,
        "license": ["https://opensource.org/licenses/MIT", "https://www.apache.org/licenses/LICENSE-2.0"],
        "image": SITE + "assets/social-preview.png",
        "keywords": "io_uring, async runtime, thread-per-core, Rust, Linux",
    }] if page == "home" else [{"@type": "WebPage", "name": title, "description": desc, "url": url}]
    if page == "docs":
        graph.append({"@type": "FAQPage", "mainEntity": [
            {"@type": "Question", "name": q,
             "acceptedAnswer": {"@type": "Answer", "text": re.sub(r"[`*]", "", a)}} for q, a in C.FAQ]})
    data = {"@context": "https://schema.org", "@graph": graph}
    text = json.dumps(data, ensure_ascii=False, separators=(",", ":")).replace("</", "<\\/")
    return f'<script type="application/ld+json">{text}</script>'


def page_head(eyebrow, h1, lede, meta=""):
    meta_html = f'<p class="page-meta">{meta}</p>' if meta else ""
    return (f'<section class="page-head"><div class="wrap"><p class="eyebrow">{eyebrow}</p>'
            f'<h1>{h1}</h1><p class="lede">{ilm(lede)}</p>{meta_html}</div></section>')


def source_note(files):
    links = " · ".join(f'<a href="{BLOB}{f}">{f}</a>' for f in files)
    return f"Generated from {links}"


# ================================================================ pages
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
        (f"{TOTAL_TESTS:,}", "tests, run on three io_uring setups, on epoll and on real 5.15, 6.0 and 6.1 kernels"),
        (str(len(DEPS)), "dependencies in the runtime: " + ", ".join(f"`{d}`" for d in DEPS)),
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
      {copy_cmd("hero-cmd", "cargo add giong")}
      <p class="meta"><span>Linux 6.0+</span><span>Rust 1.80+</span><span>MIT or Apache-2.0</span></p>
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

<section class="band" id="agents">
  <div class="wrap agents">
    {head_block("agent-first", a["h2"], a["lede"])}
    <div class="agents-side">
      <figure class="term">
        <div class="term-bar"><span class="dots" aria-hidden="true"><i></i><i></i><i></i></span><span>cargo run</span></div>
        <pre><code><span class="p">$</span> cargo run
<span class="c">   Running `target/debug/shop`</span>
Error: <span class="e">{html.escape(a["error"])}</span></code></pre>
      </figure>
      <div class="files">
        <a href="{BLOB}AGENTS.md"><b>AGENTS.md</b><span>how to build, test and change giong</span></a>
        <a href="{asset("llms.txt")}"><b>llms.txt</b><span>where a model finds the docs</span></a>
        <a href="{link("docs", "errors")}"><b>{ERROR_COUNT} error codes</b><span>each with a page: cause and fix</span></a>
      </div>
    </div>
    <ol class="points">{points}</ol>
  </div>
</section>

<section class="band band-alt" id="status">
  <div class="wrap">
    {head_block("status", "Where giong stands",
                f"Version {MINOR} is complete enough to write real services, and it is checked hard. It is also young, Linux only, and narrow on purpose.",
                (link("docs", "limits"), "When giong is the wrong choice"))}
    <div class="status">
      <div><h3>Checked</h3><ul class="checks">{checked}</ul></div>
      <div><h3>Not yet</h3><ul class="gaps">{not_yet}</ul></div>
    </div>
  </div>
</section>

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
    return frame("home", "giong: a thread-per-core async runtime for Rust on io_uring",
                 C.HERO["lede"], body)


def docs_rs_link(name="giong"):
    return f"https://docs.rs/{name}"


def docs():
    reqs = "".join(f"<div><dt>{a}</dt><dd>{ilm(b)}</dd></div>" for a, b in C.REQS)
    http = next(s for s in C.CODE if s["crate"] == "giong-http")
    examples = "".join(
        f'<article class="ex"><h3><a href="{BLOB}examples/{name}.rs">examples/{name}.rs</a></h3>'
        f'<p>{ilm(desc)}</p><pre class="code sm"><code>{html.escape(cmd)}'
        + (f'\n<span class="c">{html.escape(then)}</span>' if then else "")
        + "</code></pre></article>" for name, desc, cmd, then in C.EXAMPLES)
    api = "".join(
        f'<a class="api" href="https://docs.rs/{c["name"]}"><b>{c["name"]}</b><span>{ilm(c["short"])}</span></a>'
        for c in CRATES)
    errs = "".join(
        f'<details class="errs"{" open" if n == 0 else ""}><summary><b>{html.escape(g["name"])}</b>'
        f'<span>{len(g["rows"])} code{"" if len(g["rows"]) == 1 else "s"}</span></summary><table><tbody>'
        + "".join(f'<tr><td><a href="{repo_link(r["file"], "docs/errors")}">{r["code"]}</a></td>'
                  f'<td><code>{html.escape(r["variant"])}</code></td><td>{ilm(r["meaning"])}</td></tr>'
                  for r in g["rows"])
        + "</tbody></table></details>" for n, g in enumerate(ERRORS))
    limits = "".join(f'<article class="limit"><h3>{h}</h3><p>{ilm(p)}</p></article>' for h, p in C.LIMITS)
    elsewhere = "".join(f"<p>{ilm(p)}</p>" for p in C.ELSEWHERE)
    faq = "".join(f'<details class="qa"><summary>{q}</summary><p>{ilm(a)}</p></details>' for q, a in C.FAQ)
    toc = [("requirements", "Requirements"), ("install", "Install"), ("first", "A first server"),
           ("how", "A request's path"), ("examples", "Examples"), ("api", "API reference"), ("errors", "Error codes"),
           ("limits", "Limits"), ("faq", "Questions"), ("contribute", "Work on giong")]
    toc_html = "".join(f'<a href="#{a}">{label}</a>' for a, label in toc)
    body = f'''{page_head("docs", "Get started with giong",
                          "Requirements, a first server, and where everything else is documented. The API reference for each crate is on docs.rs.")}
<div class="wrap doc-grid">
  <nav class="toc" aria-label="On this page"><p>On this page</p><div class="toc-links">{toc_html}</div></nav>
  <div class="doc">
    <section id="requirements"><h2>Requirements</h2><dl class="reqs">{reqs}</dl></section>
    <section id="install"><h2>Install</h2>
      <p>The crates are on crates.io. Add the runtime and the layers you need; <code>giong-http</code>
      takes <code>--features tls</code> for HTTPS and <code>--features client</code> for its client.</p>
      <div class="code-copy"><pre class="code" tabindex="0"><code id="install-cmd">cargo add giong
cargo add giong-http          # or giong-web for routing, middleware and JSON</code></pre>
      <button class="copy" type="button" data-copy="install-cmd" data-done="copied">copy</button></div>
    </section>
    <section id="first"><h2>A first server</h2>
      <p>Put this in <code>src/main.rs</code>. Every worker binds its own <code>SO_REUSEPORT</code>
      listener on the same port, and the kernel spreads connections across cores.</p>
      <pre class="code" tabindex="0"><code>{rust(http["src"])}</code></pre>
      <p>Then run it:</p>
      <pre class="code" tabindex="0"><code>{shell(C.RUN_SH)}</code></pre>
    </section>
    <section id="how"><h2>What happens to a request</h2>
      <p>One connection, from the kernel to your handler and back. Everything stays on the worker
      that accepted it, and every trip into the kernel that a turn of its loop needs, for all of
      its connections, goes in the same <code>io_uring_enter</code>.</p>
      <figure class="fig ill-wrap">{art.request()}
        <figcaption>The worker on core 2 owns this connection for its whole life. Its query to
        PostgreSQL is one more entry in the same ring, pipelined with the other tasks' queries.</figcaption>
      </figure>
    </section>
    <section id="examples"><h2>Examples</h2>
      <p>Each one runs from a clone of the repository; the crates' READMEs have more.
      <a href="{TREE}examples">examples/</a></p>
      <div class="examples">{examples}</div>
    </section>
    <section id="api"><h2>API reference</h2>
      <p>Every public item is documented, with an example where it helps.</p>
      <div class="apis">{api}</div>
    </section>
    <section id="errors"><h2>Error codes</h2>
      <p>Every error giong raises starts with a code, says what went wrong and how to fix it, and
      links to its page. {ERROR_COUNT} codes so far. Match on <code>giong::error::code(&amp;err)</code>,
      never on the message.</p>
      <pre class="code sm"><code>{html.escape(C.AGENTS["error"])}</code></pre>
      {errs}
      <p class="src-note">{source_note(["docs/errors/README.md"])}</p>
    </section>
    <section id="limits"><h2>When giong is the wrong choice</h2>
      <div class="limits">{limits}</div>
      <div class="elsewhere"><h3>Pick something else when</h3>{elsewhere}</div>
    </section>
    <section id="faq"><h2>Questions people ask</h2><div class="faq">{faq}</div></section>
    <section id="contribute"><h2>Work on giong</h2>
      <p>Read <a href="{BLOB}CONTRIBUTING.md">CONTRIBUTING.md</a> first; coding agents have
      <a href="{BLOB}AGENTS.md">AGENTS.md</a>. Open an issue before anything larger than a fix.</p>
      <pre class="code" tabindex="0"><code>{shell(C.FROM_SOURCE_SH)}</code></pre>
    </section>
  </div>
</div>'''
    return frame("docs", "Get started", "How to install giong, write a first server, and find the "
                 "API reference, the error codes and the limits.", body, "docs/")


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
            f'<p class="crate-links">{crate_links(c)}</p>'
            f'{copy_cmd("add-" + c["name"], "cargo add " + c["name"])}</div>'
            f'<div class="crate-main"><p class="crate-lede">{ilm(c["lede"])}</p>'
            + (f'<ul>{items}</ul>' if items else "")
            + f'<p class="crate-facts">{" · ".join(facts)}</p></div></article>')
    for c in COMING:
        sections.append(
            f'<article class="crate is-coming" id="{c["name"]}"><div class="crate-side"><h2>{c["name"]}</h2>'
            f'<p class="crate-layer">{dict(C.LAYERS).get(c["layer"], "More")} · on the roadmap</p></div>'
            f'<div class="crate-main"><p class="crate-lede">{ilm(c["short"])}</p>'
            f'<p class="crate-facts"><a href="{link("roadmap")}">See the roadmap</a></p></div></article>')
    total_lines = sum(c["lines"] for c in CRATES)
    body = f'''{page_head("crates", "The crates",
                          f"{len(CRATES)} crates that share one version number, {VERSION}. The runtime at the bottom, and the layers a service needs on top of it, each written for giong rather than wrapped around a Tokio crate.",
                          f"{about(total_lines)} lines of Rust and {TOTAL_TESTS:,} tests in all · line counts are read from the sources when the site is built")}
<section class="band band-tight"><div class="wrap">{stack(detail=True)}</div></section>
<section class="band band-alt"><div class="wrap crate-list">{"".join(sections)}</div></section>'''
    return frame("crates", "Crates", "The giong crates: the runtime, HTTP and TLS, PostgreSQL, MySQL "
                 "and Redis clients, and a web framework.", body, "crates/")


def benchmarks():
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
    body = f'''{page_head("benchmarks", "Benchmarks", C.BENCH_METHOD,
                          f'Full tables and raw output: <a href="{REPO}#benchmarks">README</a> · <a href="{TREE}bench/results">bench/results/</a>')}
<section class="band band-tight"><div class="wrap">
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
<section class="band band-alt" id="caveats"><div class="wrap caveats">
  <h2>What these numbers do not show</h2>
  <ul>{caveats}</ul>
</div></section>
<section class="band" id="compare"><div class="wrap">
  {head_block("compare", "Compared with Monoio and Glommio",
              "The other io_uring runtimes for Rust, as they stood when the benchmarks ran. Monoio and Glommio are mature projects with wider ecosystems; this table is about how each one is built, not a ranking.")}
  <div class="table-scroll" tabindex="0" role="region" aria-label="Comparison with Monoio and Glommio">
    <table class="cmp"><thead><tr>{cmp_head}</tr></thead><tbody>{cmp_rows}</tbody></table>
  </div>
</div></section>
<section class="band band-alt" id="reproduce"><div class="wrap">
  {head_block("reproduce", "Run them yourself",
              "Each script builds the servers, runs the load generator and writes its summary to `bench/results/`. Run them on a quiet Linux machine; they take minutes, not seconds.")}
  <div class="runs">{runs}</div>
</div></section>'''
    return frame("benchmarks", "Benchmarks", "giong measured against Tokio, Monoio, Glommio, axum, "
                 "Bun and Node.js, with the method and the caveats next to every number.", body, "benchmarks/")


def roadmap():
    def md_html(src):
        h = doc_md(src, heading_shift=1, heading_ids=False)
        return h.replace("<p><strong>Exit criteria:</strong>", '<p class="exit"><strong>Done when:</strong>')

    def body_html(m):
        intro, rest = split_intro(m["body"])
        if m["state"] != "done" or not rest or not intro:
            return md_html(m["body"])
        label = f'Show the {m["total"]} items' if m["total"] else "Show the details"
        return md_html(intro) + more(label, f'<div class="prose">{md_html(rest)}</div>')
    rows = []
    for m in MILESTONES:
        progress = f'<span class="ms-progress">{m["done"]} of {m["total"]}</span>' if m["total"] else ""
        bar = (f'<span class="ms-bar" style="--p:{100 * m["done"] / m["total"]:.0f}%"></span>'
               if m["total"] else "")
        label = m["ver"] or "crates"
        rows.append(
            f'<li class="ms is-{m["state"]}" id="{m["id"]}">'
            f'<div class="ms-side"><b class="ms-ver{"" if m["ver"] else " is-word"}">{label}</b><span class="ms-state">{m["state"]}</span>{progress}{bar}</div>'
            f'<div class="ms-main"><h2>{html.escape(m["name"] if m["ver"] else m["title"])}</h2>'
            f'<div class="prose">{body_html(m)}</div></div></li>')
    named = ROADMAP_NAMED
    body = f'''{page_head("roadmap", "Where giong is going",
                          "Milestones in order, with no dates: a milestone is done when its exit criteria are met.",
                          source_note(["ROADMAP.md"]))}
<section class="band band-tight"><div class="wrap goal">
  <div class="prose prose-lg"><h2 id="goal">Goal</h2>{doc_md(named.get("Goal", ""), heading_ids=False)}</div>
  <div class="prose"><h2 id="agent-first">Agent-first</h2>{doc_md(named.get("Agent-first", ""), heading_ids=False)}</div>
</div></section>
<section class="band band-alt"><div class="wrap"><ol class="timeline">{"".join(rows)}</ol></div></section>
<section class="band" id="non-goals"><div class="wrap nongoals">
  <h2>Deliberate non-goals</h2>
  <div class="prose">{doc_md(named.get("Non-goals", ""), heading_ids=False)}</div>
</div></section>'''
    return frame("roadmap", "Roadmap", "Where giong is going: the milestones in order, their exit "
                 "criteria, and the deliberate non-goals.", body, "roadmap/")


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
        count = len(re.findall(r"^- ", r["body"], re.M))
        intro, rest = split_intro(r["body"])
        if unreleased:
            intro, rest = "", r["body"]
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
    body = f'''{page_head("releases", "Releases",
                          "What changed in each version, newest first. Every crate shares the version number. While it starts with 0, a minor release may break the API.",
                          source_note(["CHANGELOG.md"]) + f' · <a href="{REPO}/releases.atom">feed</a>')}
<div class="wrap doc-grid">
  <nav class="toc" aria-label="Versions"><p>Versions</p><div class="toc-links">{"".join(side)}</div></nav>
  <div class="releases">{"".join(items)}</div>
</div>'''
    return frame("releases", "Releases", "Release notes for every version of giong, from CHANGELOG.md.",
                 body, "releases/")


def not_found():
    body = f'''<section class="lost"><div class="wrap">
  <p class="eyebrow">404</p>
  <h1>Nothing here, <em>not even one syscall.</em></h1>
  <p class="lede">The page you asked for does not exist.</p>
  <div class="cta"><a class="btn btn-main" href="{link("home")}">Back to giong.run</a><a class="btn" href="{link("docs")}">Docs</a></div>
</div></section>'''
    return frame("404", "Not found", "This page does not exist.", body, "404").replace(
        '<link rel="canonical"', '<meta name="robots" content="noindex">\n<link rel="canonical"')


PAGES = ["home", "docs", "crates", "benchmarks", "roadmap", "releases"]


def sitemap():
    urls = "".join(f"  <url><loc>{SITE}{'' if p == 'home' else p + '/'}</loc></url>\n" for p in PAGES)
    return ('<?xml version="1.0" encoding="UTF-8"?>\n'
            '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n' + urls + '</urlset>\n')


def llms():
    """llms.txt (https://llmstxt.org): where a language model finds the docs."""
    api = "".join(f"- [{c['name']} API](https://docs.rs/{c['name']}): {c['description']}\n" for c in CRATES)
    examples = "".join(f"- [examples/{name}.rs]({BLOB}examples/{name}.rs): {re.sub(r'[`*]', '', desc)}\n"
                       for name, desc, _, _ in C.EXAMPLES)
    return f"""# giong

> A thread-per-core async runtime for Rust on Linux, built directly on io_uring. Every
> worker owns its ring, tasks and buffers; tasks never move between threads, so futures
> need not be `Send`. Version {VERSION}.

Facts that are easy to get wrong:

- Linux only: io_uring on 6.0 or newer, and an epoll driver where io_uring is blocked
  (Docker's default seccomp profile, gVisor) or the kernel is older. It does not build on
  macOS or Windows yet.
- Buffers are passed by value and handed back, because the kernel uses them after the call:
  `stream.write_all(buf).await` returns `(io::Result<()>, buf)`.
- `recv` is cancel safe: a `timeout` around it loses no data. A dropped `write_all` may
  have sent part of its buffer.
- There are no macros: start a runtime with `giong::block_on` or `giong::Builder`.
- An error giong raises starts with a code, such as `GIONG-E0102`, says how to fix it, and
  links to its page; `giong::error::code(&err)` returns the code to match on.
- giong-web: build the `Router` inside each worker's future (`run(|_| async {{ .. }})`), since
  an `App` is not `Send`. A handler is `async fn(Req<S>) -> Result<Response, Error>`; `?` on
  any other error answers `500` and logs the cause, never sending it.
- Crates that need the Tokio runtime (sqlx, reqwest, tonic's transport) do not run on giong.
  Crates generic over a Tokio or futures-io stream (h2, tokio-rustls, tokio-tungstenite) do,
  through `giong_compat::Compat::new(stream)`.
- `giong-http` serves HTTP/2 by itself, through ALPN over TLS or with prior knowledge on the
  same port; handlers do not change. `Server::http2(false)` turns it off.
- On Linux 6.14 to 7.2 each worker's rings count against the locked-memory limit
  (`ulimit -l`, about 150 KiB per worker). Past it giong runs that worker on epoll and says
  why (`GIONG-E0010`); raise the limit as docs/DEPLOYMENT.md shows.
- Install with `cargo add giong` (and `giong-http`, `giong-web`, ...). Libraries need Rust 1.80.

## Docs

{api}- [Get started]({SITE}docs/): requirements, a first server, examples, limits
- [README]({BLOB}README.md): design, benchmarks with their method, limitations
- [AGENTS.md]({BLOB}AGENTS.md): how to build, test and change giong, for coding agents
- [Error codes]({BLOB}docs/errors/README.md): what each `GIONG-E` code means and how to fix it
- [CONTRIBUTING.md]({BLOB}CONTRIBUTING.md): the rules every change follows
- [ROADMAP.md]({BLOB}ROADMAP.md): milestones and deliberate non-goals
- [CHANGELOG.md]({BLOB}CHANGELOG.md): what changed in each release

## Examples

{examples}
## Optional

- [Benchmark results]({TREE}bench/results): raw numbers behind the README
- [giong-http README]({BLOB}giong-http/README.md), [giong-web README]({BLOB}giong-web/README.md)
"""


def check_links(out, files):
    """Every link between the site's own pages, and every anchor in one, must exist."""
    ids = {name: set(re.findall(r'\sid="([^"]+)"', text)) for name, text in files.items()}
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
    files = {
        page_file("home"): home(),
        page_file("docs"): docs(),
        page_file("crates"): crates(),
        page_file("benchmarks"): benchmarks(),
        page_file("roadmap"): roadmap(),
        page_file("releases"): releases(),
        "404.html": not_found(),
        "sitemap.xml": sitemap(),
        "robots.txt": f"User-agent: *\nAllow: /\nSitemap: {SITE}sitemap.xml\n",
        "llms.txt": llms(),
    }
    for name, text in files.items():
        (out / name).parent.mkdir(parents=True, exist_ok=True)
        with open(out / name, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
    missing = [n for n in CRATE_DIRS if n not in C.CRATES]
    if missing:
        print("note: no entry in content.py CRATES for " + ", ".join(missing)
              + "; the site shows their Cargo.toml description", file=sys.stderr)
    print(f"wrote {len(files)} files to {out}")
    problems = check_links(out, files)
    if problems:
        sys.exit("broken links on the site:\n  " + "\n  ".join(problems))
    return files


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    build(Path(args[0]) if args else HERE / "public")
