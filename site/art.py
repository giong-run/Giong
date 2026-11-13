"""Explanatory drawings for giong.run, as inline SVG.

Each drawing explains one idea of the design, at a size that stays readable on a phone
(a 360-unit wide canvas, 11-unit text). Colours come from the site's tokens through CSS
classes (see `.ill` in public/styles.css), so the drawings follow the light and dark themes.
Motion is CSS only: it shows the order of events, it stops when the reader asks for reduced
motion, and the still frame is the complete picture.
"""

import html
import math

W = 360


def _a(v):
    return f"{v:g}"


def text(x, y, s, cls="", anchor="start"):
    a = "" if anchor == "start" else f' text-anchor="{anchor}"'
    c = f' class="{cls}"' if cls else ""
    return f'<text x="{_a(x)}" y="{_a(y)}"{a}{c}>{html.escape(s)}</text>'


def rect(x, y, w, h, cls, rx=6, extra=""):
    return f'<rect x="{_a(x)}" y="{_a(y)}" width="{_a(w)}" height="{_a(h)}" rx="{rx}" class="{cls}"{extra}/>'


def line(x1, y1, x2, y2, cls):
    return f'<line x1="{_a(x1)}" y1="{_a(y1)}" x2="{_a(x2)}" y2="{_a(y2)}" class="{cls}"/>'


def arrow(x1, y1, x2, y2, cls="mut", head=6.0, extra=""):
    """A line with a filled head at (x2, y2)."""
    ang = math.atan2(y2 - y1, x2 - x1)
    bx, by = x2 - head * math.cos(ang), y2 - head * math.sin(ang)
    px, py = -math.sin(ang) * head * 0.55, math.cos(ang) * head * 0.55
    pts = f"{_a(round(x2, 2))},{_a(round(y2, 2))} {_a(round(bx + px, 2))},{_a(round(by + py, 2))} {_a(round(bx - px, 2))},{_a(round(by - py, 2))}"
    return (f'<g class="arr {cls}"{extra}><line x1="{_a(x1)}" y1="{_a(y1)}" x2="{_a(round(bx, 2))}" '
            f'y2="{_a(round(by, 2))}"/><polygon points="{pts}"/></g>')


def svg(h, body, label, name):
    return (f'<svg class="ill ill-{name}" viewBox="0 0 {W} {h}" role="img" aria-label="{html.escape(label)}">'
            f'{body}</svg>')


def keyframes(name, points, extra=""):
    """A CSS animation that moves an element through `points` [(percent, x, y), ...]."""
    steps = []
    for pct, x, y, *op in points:
        o = f" opacity: {op[0]};" if op else ""
        steps.append(f"{pct:g}% {{ transform: translate({x:g}px, {y:g}px);{o} }}")
    return f"<style>@keyframes {name} {{ {' '.join(steps)} }}{extra}</style>"


def bezier(p0, p1, p2, p3, n):
    out = []
    for i in range(n + 1):
        t = i / n
        u = 1 - t
        x = u ** 3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t ** 3 * p3[0]
        y = u ** 3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t ** 3 * p3[1]
        out.append((round(x, 1), round(y, 1)))
    return out


def d(n):
    """Inline animation delay."""
    return f' style="--d:{n:g}s"'


# ------------------------------------------------------------------ thread per core
def cores():
    """Shared nothing: one worker per core, the kernel spreads connections."""
    b = []
    # connections arriving
    for i in range(12):
        x = 22 + i * 28.7
        b.append(f'<circle cx="{_a(round(x, 1))}" cy="12" r="3.5" class="dot"/>')
        b.append(line(round(x, 1), 17, round(x, 1), 30, "faint"))
    b.append(rect(0, 30, W, 32, "box2"))
    b.append(text(W / 2, 50, "kernel: SO_REUSEPORT picks a core", "t2", "middle"))
    cw, gap = 82, 10
    for i in range(4):
        x = 1 + i * (cw + gap)
        cx = x + cw / 2
        b.append(arrow(cx, 63, cx, 92, "mut"))
        b.append(f'<circle cx="{_a(cx)}" cy="66" r="3.5" class="tok anim drop"{d(i * 0.55)}/>')
        b.append(rect(x, 94, cw, 156, "hot" if i == 2 else "box"))
        b.append(text(cx, 114, f"core {i}", "t1 tb", "middle"))
        # the worker's own ring
        b.append(f'<g class="ring" transform="translate({_a(cx)} 150)">'
                 f'<circle r="17" class="ring-off"/>'
                 f'<circle r="17" class="ring-on anim spin"{d(-i * 0.7)}/></g>')
        b.append(text(cx, 186, "own ring", "t3", "middle"))
        # tasks that stay where they started
        for k in range(3 if i % 2 == 0 else 4):
            n = 3 if i % 2 == 0 else 4
            tx = cx - (n * 10 - 3) / 2 + k * 10
            b.append(rect(round(tx, 1), 202, 7, 7, "task", 1.5))
        b.append(text(cx, 226, "its tasks", "t3", "middle"))
        b.append(text(cx, 241, "stay here", "t3", "middle"))
    return svg(252, "".join(b),
               "Connections arrive at the kernel, which hands each one to one of four cores. "
               "Every core has its own ring and its own tasks; nothing is shared between cores.",
               "cores")


# ------------------------------------------------------------------ one syscall per tick
OPS = ["accept", "recv", "send", "recv", "timer", "close"]


def syscalls():
    b = []
    lx = 66                        # lanes start here; lane names sit to the left
    # -- epoll style
    b.append(text(0, 12, "epoll style: one system call per operation", "t3"))
    sw, sg = 43, 5.6
    for i, op in enumerate(OPS):
        x = lx + i * (sw + sg)
        b.append(rect(round(x, 1), 24, sw, 18, "seg-box", 3))
        b.append(text(round(x + sw / 2, 1), 37, op, "t2 ts", "middle"))
        b.append(arrow(round(x + sw / 2, 1), 44, round(x + sw / 2, 1), 82, "mut anim flash", 5,
                       d(i * 0.32)))
    b.append(text(0, 37, "process", "t3"))
    b.append(line(lx, 62, W, 62, "dash"))
    b.append(rect(lx, 84, W - lx, 22, "box2", 4))
    b.append(text(lx + (W - lx) / 2, 99, "6 trips into the kernel", "t2", "middle"))
    b.append(text(0, 99, "kernel", "t3"))
    # -- giong
    y0 = 136
    b.append(text(0, y0, "giong: the whole tick in one call", "te"))
    qw, qg = 18, 3
    qx1, qx2 = lx, lx + 166
    b.append(text(qx1, y0 + 18, "submission queue", "t3 ts"))
    b.append(text(qx2, y0 + 18, "completion queue", "t3 ts"))
    for i in range(6):
        b.append(rect(qx1 + i * (qw + qg), y0 + 25, qw, 16, "seg on anim fill-in", 2.5).replace(
            "/>", f' style="--d:{i * 0.22:g}s"/>'))
        b.append(rect(qx2 + i * (qw + qg), y0 + 25, qw, 16, "seg on anim fill-out", 2.5).replace(
            "/>", f' style="--d:{i * 0.12:g}s"/>'))
    b.append(text(0, y0 + 37, "process", "t3"))
    b.append(line(lx, y0 + 62, W, y0 + 62, "dash"))
    mid1 = qx1 + (6 * qw + 5 * qg) / 2
    mid2 = qx2 + (6 * qw + 5 * qg) / 2
    b.append(arrow(mid1, y0 + 45, mid1, y0 + 82, "em anim pulse", 6))
    b.append(arrow(mid2, y0 + 82, mid2, y0 + 45, "em anim pulse-late", 6))
    b.append(rect(lx, y0 + 84, W - lx, 22, "hot", 4))
    b.append(text(lx + (W - lx) / 2, y0 + 99, "1 × io_uring_enter", "te tb", "middle"))
    b.append(text(0, y0 + 99, "kernel", "t3"))
    return svg(y0 + 110, "".join(b),
               "Above: an epoll-style server makes one system call for each of six operations. "
               "Below: giong queues the same six operations and submits them, and collects their "
               "results, in a single io_uring_enter call.",
               "syscalls")


# ------------------------------------------------------------------ provided buffers
def buffers():
    b = []
    lx = 0
    n, step = 8, 45
    # -- a buffer per connection
    b.append(text(0, 12, "a buffer per connection: 8 held, 1 in use", "t3"))
    for i in range(n):
        cx = 22 + i * step
        hot = i == 5
        b.append(f'<circle cx="{cx}" cy="34" r="7" class="{"conn-on" if hot else "conn"}"/>')
        b.append(rect(cx - 13, 48, 26, 34, "buf", 3))
        if hot:
            b.append(rect(cx - 13, 66, 26, 16, "buf-full", 3))
    # -- giong
    y0 = 112
    b.append(text(0, y0, "giong: a shared pool, used when data arrives", "te"))
    for i in range(n):
        cx = 22 + i * step
        b.append(f'<circle cx="{cx}" cy="{y0 + 22}" r="7" class="conn"/>')
    hot = [1, 5, 3]
    # the pool: three buffers centred in their box, the label to its right
    bw, bh, bgap, pad = 26, 28, 12, 12
    box_w = 3 * bw + 2 * bgap + 2 * pad
    box_x, pool_y = 96, y0 + 74
    bufs = [box_x + pad + k * (bw + bgap) for k in range(3)]
    for k, i in enumerate(hot):
        cx = 22 + i * step
        mx = _a(bufs[sorted(hot).index(i)] + bw / 2)     # links do not cross
        b.append(f'<g class="anim take"{d(k * 1.4)}>'
                 f'<circle cx="{cx}" cy="{y0 + 22}" r="7" class="conn-on"/>'
                 f'<path d="M{mx} {pool_y} C {mx} {pool_y - 20}, {cx} {y0 + 52}, {cx} {y0 + 32}" class="link"/>'
                 f'</g>')
    b.append(rect(box_x, pool_y - 8, box_w, bh + 16, "box", 6))
    for x in bufs:
        b.append(rect(x, pool_y, bw, bh, "buf-full", 3))
    b.append(text(box_x + box_w + 10, pool_y + 10, "pool", "t3"))
    b.append(text(box_x + box_w + 10, pool_y + 26, "3 buffers", "t3"))
    return svg(y0 + 122, "".join(b),
               "Above: eight connections each hold a buffer, though only one has data. Below: the "
               "eight connections hold nothing; when data arrives on one, the kernel takes a buffer "
               "from a small shared pool.",
               "buffers")


# ------------------------------------------------------------------ cancel safety
def cancel():
    b = []
    lanes = [("your task", 40), ("driver", 104), ("kernel", 168)]
    cols = [(112, "recv()"), (212, "timeout"), (312, "next recv()")]
    for name, y in lanes:
        b.append(text(0, y + 4, name, "t3"))
        b.append(line(70, y, W, y, "faint"))
    for x, label in cols:
        b.append(text(x, 12, label, "t2 tb", "middle"))
    # 1: the read is submitted, the kernel holds the buffer
    b.append(arrow(112, 50, 112, 156, "mut", 5))
    b.append(rect(96, 160, 32, 16, "buf-full", 3))
    b.append(text(112, 196, "may write", "t3 ts", "middle"))
    b.append(text(112, 208, "into it", "t3 ts", "middle"))
    # 2: the future is dropped; the driver keeps the buffer alive
    b.append(f'<g class="x-mark" transform="translate(212 40)"><line x1="-7" y1="-7" x2="7" y2="7"/>'
             f'<line x1="7" y1="-7" x2="-7" y2="7"/></g>')
    b.append(text(212, 64, "future dropped", "t3 ts", "middle"))
    b.append(rect(186, 92, 52, 24, "hot", 4))
    b.append(text(212, 108, "owns it", "te ts", "middle"))
    b.append(line(212, 117, 212, 158, "em-dash"))
    b.append(rect(196, 160, 32, 16, "buf-full", 3))
    b.append(text(212, 196, "cancel sent", "t3 ts", "middle"))
    # 3: the kernel confirms; the bytes wait for the next read
    b.append(arrow(312, 156, 312, 52, "em", 5))
    b.append(rect(296, 22, 32, 16, "buf-full", 3))
    b.append(text(312, 196, "confirmed:", "t3 ts", "middle"))
    b.append(text(312, 208, "no byte lost", "te ts", "middle"))
    # the moving buffer
    b.append(keyframes("ill-travel", [
        (0, 112, 40, 0), (6, 112, 40, 1), (26, 112, 168, 1), (40, 112, 168, 1), (54, 212, 168, 1),
        (70, 212, 168, 1), (80, 312, 168, 1), (94, 312, 30, 1), (100, 312, 30, 0)]))
    b.append('<g class="tok anim travel"><rect x="-9" y="-6" width="18" height="12" rx="2.5" class="tok-buf"/></g>')
    return svg(216, "".join(b),
               "A read is submitted and the kernel holds its buffer. A timeout drops the future, "
               "but the driver keeps owning the buffer while the kernel may still write into it. "
               "When the kernel confirms the cancel, whatever arrived is returned by the next read.",
               "cancel")


# ------------------------------------------------------------------ the path of a request
def request():
    b = []
    kx, kw = 0, 150                # kernel lane
    wx, ww = 172, 188              # worker lane
    b.append(text(kx, 12, "kernel", "t3 tb"))
    b.append(text(wx, 12, "worker on core 2", "te tb"))
    b.append(line(161, 0, 161, 372, "dash"))
    steps = [
        ("k", "TCP connection", "from a client"),
        ("k", "SO_REUSEPORT", "picks core 2"),
        ("w", "multishot accept", "armed once"),
        ("k", "bytes arrive", "into a pooled buffer"),
        ("w", "your async fn", "parse, route, handle"),
        ("w", "giong-postgres", "query on the same ring"),
        ("k", "send", "the response"),
    ]
    y, gapy, h = 24, 9, 40
    centers = []
    for n, (lane, a, sub) in enumerate(steps):
        x, w = (kx, kw) if lane == "k" else (wx, ww)
        cls = "box2" if lane == "k" else ("hot" if n == 4 else "box")
        b.append(rect(x, y, w, h, cls, 6))
        b.append(text(x + 12, y + 17, a, "t1 tb"))
        b.append(text(x + 12, y + 32, sub, "t3 ts"))
        centers.append((x, w, y))
        y += h + gapy
    for (x1, w1, y1), (x2, w2, y2) in zip(centers, centers[1:]):
        if x1 == x2:
            cx = x1 + w1 / 2
            b.append(arrow(cx, y1 + h + 1, cx, y2 - 1, "mut", 4.5))
        elif x1 < x2:
            b.append(arrow(x1 + w1 + 1, y1 + h - 8, x2 - 1, y2 + 10, "mut", 4.5))
        else:
            b.append(arrow(x1 - 1, y1 + h - 8, x2 + w2 + 1, y2 + 10, "mut", 4.5))
    pts = [(x + w / 2, yy + h / 2) for x, w, yy in centers]
    n = len(pts) - 1
    frames = [(0, *pts[0], 0), (3, *pts[0], 1)]
    for i, (px, py) in enumerate(pts[1:], 1):
        frames.append((round(3 + 90 * i / n - 6, 1), *pts[i - 1], 1))
        frames.append((round(3 + 90 * i / n, 1), px, py, 1))
    frames.append((100, *pts[-1], 0))
    b.append(keyframes("ill-walk", frames))
    b.append('<circle r="5" class="tok anim walk"/>')
    return svg(y + 4, "".join(b),
               "A request's path: the kernel accepts the TCP connection and SO_REUSEPORT picks "
               "core 2; the worker's multishot accept takes it; bytes arrive in a pooled buffer; "
               "your async function runs on that worker, queries PostgreSQL on the same ring, and "
               "the kernel sends the response.",
               "request")


# ------------------------------------------------------------------ how the numbers are measured
def bench_setup():
    b = []
    b.append(text(0, 12, "4-vCPU virtual machine", "t3"))
    b.append(rect(0, 20, W, 124, "box2", 8))
    tw, tg = 80, 8
    tiles = [("cpu 0", "wrk", "the load", "box"), ("cpu 1", "server", "1 worker", "hot"),
             ("cpu 2", "", "", "ghost"), ("cpu 3", "", "", "ghost")]
    for i, (cpu, big, sub, cls) in enumerate(tiles):
        x = 8 + i * (tw + tg)
        b.append(rect(x, 30, tw, 74, cls, 6))
        b.append(text(x + 10, 47, cpu, "t3 ts"))
        if big:
            b.append(text(x + 10, 72, big, ("te" if i == 1 else "t1") + " tl tb"))
            b.append(text(x + 10, 90, sub, "t3 ts"))
        else:
            b.append(text(x + 10, 72, "left to", "t3 ts"))
            b.append(text(x + 10, 86, "the OS", "t3 ts"))
    # loopback between cpu 0 and cpu 1
    b.append('<path d="M48 106 C 48 132, 136 132, 136 106" class="link-em"/>')
    curve = bezier((48, 106), (48, 132), (136, 132), (136, 106), 8)
    there = [(round(5 + 40 * i / 8, 1), x, y, 1) for i, (x, y) in enumerate(curve)]
    back = [(round(55 + 40 * i / 8, 1), x, y, 1) for i, (x, y) in enumerate(reversed(curve))]
    b.append(keyframes("ill-hop", [(0, 48, 106, 0)] + there + back + [(100, 48, 106, 0)]))
    b.append('<circle r="3.5" class="tok anim hop"/>')
    b.append(text(150, 128, "loopback, no network", "t2 ts"))
    # rounds
    y0 = 170
    b.append(text(0, y0, "five rounds, servers interleaved", "t3"))
    x = 0
    kinds = ["on", "bar", "fg3"]
    for r in range(5):
        for k in kinds:
            b.append(rect(x, y0 + 8, 14, 12, f"r-{k}", 2))
            x += 18
        x += 6
    b.append(text(0, y0 + 46, "median", "t2 ts"))
    b.append(rect(52, y0 + 37, 150, 10, "r-on", 2))
    b.append(line(186, y0 + 52, 214, y0 + 52, "whisker"))
    b.append(line(186, y0 + 49, 186, y0 + 55, "whisker"))
    b.append(line(214, y0 + 49, 214, y0 + 55, "whisker"))
    b.append(text(222, y0 + 56, "lowest to highest", "t3 ts"))
    return svg(y0 + 62, "".join(b),
               "The benchmark setup: inside a 4-vCPU virtual machine, the load generator wrk is "
               "pinned to cpu 0 and the server to cpu 1, talking over loopback. Servers are "
               "measured in five interleaved rounds; a chart shows the median, with a thin line "
               "from the lowest round to the highest.",
               "bench")
