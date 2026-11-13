"""A small Markdown renderer for the repository's own documents.

It covers what CHANGELOG.md, ROADMAP.md and docs/ use: headings, paragraphs, nested
lists (with task boxes), code fences, block quotes, simple tables, `code`, **bold**,
*emphasis* and [links](url). It is not a general Markdown implementation, and it does
not need to be: the build fails loudly (see `strict`) rather than render something
it does not understand.
"""
import html
import re

LIST_ITEM = re.compile(r"^(\s*)([-*+]|\d+[.)])\s+(.*)$")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
FENCE = re.compile(r"^(\s*)(```+|~~~+)\s*([\w+-]*)")
REF_DEF = re.compile(r"^\[[^\]]+\]:\s+\S+")


def slug(text):
    """GitHub-style anchor: lower case, punctuation dropped, spaces to hyphens."""
    text = re.sub(r"<[^>]+>", "", text).lower().strip()
    text = re.sub(r"[^\w\- ]", "", text)
    return re.sub(r"\s", "-", text)


def inline(text, link=lambda url: url):
    """Inline Markdown to HTML. `link` rewrites every link target."""
    codes = []

    def keep_code(m):
        codes.append(f"<code>{html.escape(m.group(2), quote=False)}</code>")
        return f"\x00{len(codes) - 1}\x00"

    text = re.sub(r"(`+)(.+?)\1", keep_code, text)
    text = html.escape(text, quote=False)

    def a(m):
        label, url = m.group(1), html.unescape(m.group(2))
        return f'<a href="{html.escape(link(url))}">{label}</a>'

    text = re.sub(r"\[([^\]]+)\]\(([^)\s]+)\)", a, text)
    text = re.sub(r"\*\*(.+?)\*\*", r"<strong>\1</strong>", text)
    text = re.sub(r"(?<![\w*])\*(?!\s)(.+?)(?<!\s)\*(?![\w*])", r"<em>\1</em>", text)
    return re.sub(r"\x00(\d+)\x00", lambda m: codes[int(m.group(1))], text)


def render(src, link=lambda url: url, heading_shift=0, heading_ids=True):
    """Markdown to HTML. Headings move down by `heading_shift` levels."""
    lines = src.replace("\r\n", "\n").split("\n")
    out = []
    i = 0
    n = len(lines)

    def para(buf):
        if buf:
            out.append(f"<p>{inline(' '.join(s.strip() for s in buf), link)}</p>")

    buf = []
    while i < n:
        line = lines[i]
        stripped = line.strip()
        if stripped.startswith("<!--"):
            para(buf); buf = []
            while i < n and "-->" not in lines[i]:
                i += 1
            i += 1
            continue
        if not stripped:
            para(buf); buf = []
            i += 1
            continue
        if REF_DEF.match(stripped):
            para(buf); buf = []
            i += 1
            continue
        m = FENCE.match(line)
        if m:
            para(buf); buf = []
            fence, lang = m.group(2), m.group(3)
            body = []
            i += 1
            while i < n and not lines[i].strip().startswith(fence):
                body.append(lines[i])
                i += 1
            i += 1
            cls = f' class="lang-{lang}"' if lang else ""
            out.append(f'<pre class="code"><code{cls}>{html.escape(chr(10).join(body), quote=False)}</code></pre>')
            continue
        m = HEADING.match(line)
        if m:
            para(buf); buf = []
            level = min(6, len(m.group(1)) + heading_shift)
            text = inline(m.group(2), link)
            ident = f' id="{slug(m.group(2))}"' if heading_ids else ""
            out.append(f"<h{level}{ident}>{text}</h{level}>")
            i += 1
            continue
        if stripped.startswith(">"):
            para(buf); buf = []
            quote = []
            while i < n and lines[i].strip().startswith(">"):
                quote.append(re.sub(r"^\s*>\s?", "", lines[i]))
                i += 1
            out.append(f"<blockquote>{render(chr(10).join(quote), link, heading_shift, heading_ids)}</blockquote>")
            continue
        if stripped.startswith("|") and i + 1 < n and re.match(r"^\s*\|[\s:|-]+\|\s*$", lines[i + 1]):
            para(buf); buf = []
            def cells(row):
                return [c.strip() for c in row.strip().strip("|").split("|")]
            head = cells(line)
            i += 2
            rows = []
            while i < n and lines[i].strip().startswith("|"):
                rows.append(cells(lines[i]))
                i += 1
            th = "".join(f"<th>{inline(c, link)}</th>" for c in head)
            tb = "".join("<tr>" + "".join(f"<td>{inline(c, link)}</td>" for c in r) + "</tr>" for r in rows)
            out.append(f'<div class="table-scroll"><table><thead><tr>{th}</tr></thead><tbody>{tb}</tbody></table></div>')
            continue
        if LIST_ITEM.match(line) and not buf:
            i = _list(lines, i, out, link)
            continue
        buf.append(line)
        i += 1
    para(buf)
    return "\n".join(out)


def _list(lines, i, out, link):
    """Parse a (nested) list starting at line i; return the index after it."""
    items = []          # (indent, ordered, text)
    n = len(lines)
    while i < n:
        line = lines[i]
        m = LIST_ITEM.match(line)
        if m:
            items.append([len(m.group(1).expandtabs(4)), m.group(2)[0].isdigit(), m.group(3)])
            i += 1
            continue
        if not line.strip():
            # A blank line ends the list unless the next text belongs to it.
            j = i + 1
            while j < n and not lines[j].strip():
                j += 1
            if j < n and (LIST_ITEM.match(lines[j]) and len(LIST_ITEM.match(lines[j]).group(1)) > 0
                          or (lines[j].startswith((" ", "\t")) and items)):
                i = j
                continue
            if j < n and LIST_ITEM.match(lines[j]) and items and items[0][0] == 0:
                i = j
                continue
            break
        if line.startswith((" ", "\t")) and items:
            items[-1][2] += " " + line.strip()
            i += 1
            continue
        break

    html_out = []
    stack = []          # (indent, tag)

    def li(text):
        cls = ""
        box = re.match(r"^\[([ xX])\]\s+(.*)$", text)
        if box:
            done = box.group(1) != " "
            cls = ' class="task is-done"' if done else ' class="task"'
            text = box.group(2)
        return f"<li{cls}>{inline(text, link)}"

    for indent, ordered, text in items:
        tag = "ol" if ordered else "ul"
        if not stack or indent > stack[-1][0]:
            html_out.append(f"<{tag}>")
            stack.append((indent, tag))
        else:
            while stack and indent < stack[-1][0]:
                html_out.append(f"</li></{stack.pop()[1]}>")
            html_out.append("</li>")
        html_out.append(li(text))
    while stack:
        html_out.append(f"</li></{stack.pop()[1]}>")
    out.append("".join(html_out))
    return i
