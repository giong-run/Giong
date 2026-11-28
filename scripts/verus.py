#!/usr/bin/env python3
"""The Verus proofs in verify/ (docs/verus.md).

    scripts/verus.py drift      check that every mirrored source file is still
                                what its proof verifies, and that every pinned
                                function a model describes is unchanged (no
                                Verus needed: the lint job runs it on every
                                pull request)
    scripts/verus.py prove      run Verus on every proof
    scripts/verus.py planted    plant each known bug in a copy of its proof and
                                check that Verus rejects it (a proof that
                                accepts a planted bug proves nothing)
    scripts/verus.py install    download the pinned Verus release into
                                target/verus and the Rust toolchain it needs
    scripts/verus.py all        drift, prove, planted
    scripts/verus.py pins       print the pins as they are now (to update
                                verify/pins.txt after reviewing a model)

Verus is found as $VERUS, else target/verus/verus-x86-linux/verus, else
`verus` on PATH; its version must be the pinned one.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent.parent
VERIFY = ROOT / "verify"

# The pinned release: Verus builds against one Rust toolchain, which must be
# installed with rustup (it is CI's RUST_STABLE as well).
VERUS_VERSION = "0.2026.11.22.1ce6a59"
VERUS_TOOLCHAIN = "1.100.0"
# The sha256 of the release archive below. `install` refuses any other archive, so a
# replaced release asset never runs here or in CI. To move to a newer Verus, download
# the archive, check it, and put its sha256 here with the new version.
VERUS_SHA256 = "c955d5958877f4f907cce80aa996d4239a481984829d82e325938c7bf242b0db"
VERUS_URL = (
    "https://github.com/verus-lang/verus/releases/download/"
    f"release/{VERUS_VERSION}/verus-{VERUS_VERSION}-x86-linux.zip"
)

# Each proof is a crate root Verus checks on its own.
PROOFS = ["ipc_ring.rs", "wakeups.rs"]

# Proof file -> the source file its `verus-mirror` section must equal.
MIRRORS = {"ipc_ring.rs": "src/ipc/ring.rs"}

# Where a proof may trust code instead of proving it. Anything else marked
# `external_body` or `external`, and any `assume(`/`admit(`, fails `drift`.
TRUSTED = {
    "ipc_ring.rs": {
        # A const Verus cannot evaluate (u64::from_le_bytes); not used by the
        # code proven.
        "MAGIC",
        # The constructor: raw pointer casts and `addr_of!`, under its
        # `# Safety` contract, which its `ensures` restates.
        "new",
        # Dereferences the shared `Dir`; its fields are atomics, whose loads
        # vstd specifies as returning anything.
        "dir",
        # `const _: () = assert!(size_of::<Control>() <= DATA_OFFSET)`: rustc
        # checks it when it compiles the crate.
        "_",
    },
}

# Planted bugs: (proof, what it breaks, text to replace, replacement, an error
# Verus must report). Each must make the proof fail.
PLANTED = [
    (
        "wakeups.rs",
        "run_batch polls a task without RunFlag::begin_poll (the flag stays set)",
        "tasks: s.tasks.insert(u, Task { queued: false, ..s.tasks[u] }),",
        "tasks: s.tasks,",
        "assertion failed",
    ),
    (
        "wakeups.rs",
        "prepare_park does not look at pending after announcing the sleep",
        "        Pc::ParkRead => if s.pending || choice {",
        "        Pc::ParkRead => if choice {",
        "postcondition not satisfied",
    ),
    (
        "wakeups.rs",
        "Doorbell::ring takes the sleeping flag but writes no eventfd",
        "State { sleeping: false, wakers: s.wakers.insert(w, WPc::Write), ..s }",
        "State { sleeping: false, wakers: s.wakers.insert(w, WPc::Idle), ..s }",
        "postcondition not satisfied",
    ),
    (
        "wakeups.rs",
        "the worker sleeps with tasks still in its run queue",
        "        Pc::AfterBatch => if s.q.len() > 0 || choice {",
        "        Pc::AfterBatch => if choice {",
        "postcondition not satisfied",
    ),
    (
        "ipc_ring.rs",
        "checked_head accepts a head more than a ring behind the tail",
        "if head > self.pos || used > self.capacity || used % 8 != 0 {",
        "if head > self.pos || used % 8 != 0 {",
        "postcondition not satisfied",
    ),
    (
        "ipc_ring.rs",
        "peek follows a wrap marker at offset 0 (the bug the proof found)",
        """                if off == 0 {
                    return Err(Corrupt("a wrap marker at the start of the data area"));
                }
""",
        "",
        "decreases not satisfied",
    ),
    (
        "ipc_ring.rs",
        "peek returns a record that runs past the end of the data area",
        "if len > max_message(self.capacity as usize) || total > avail || total > contiguous {",
        "if len > max_message(self.capacity as usize) || total > avail {",
        "postcondition not satisfied",
    ),
    (
        "ipc_ring.rs",
        "push forgets the room a wrap marker skips",
        """        let need = if total <= contiguous {
            total
        } else {
            contiguous + total
        };
        if need > self.capacity - (self.pos - self.seen) {
            self.seen = self.checked_head()?;""",
        """        let need = total;
        if need > self.capacity - (self.pos - self.seen) {
            self.seen = self.checked_head()?;""",
        "postcondition not satisfied",
    ),
]

SPEC_CLAUSES = {
    "requires",
    "ensures",
    "recommends",
    "decreases",
    "invariant",
    "invariant_except_break",
    "opens_invariants",
    "returns",
}


def fail(msg):
    print(f"verus.py: {msg}", file=sys.stderr)
    sys.exit(1)


# --- Tokens -----------------------------------------------------------------

TOKEN = re.compile(
    r"""
    (?P<ws>\s+)
  | (?P<line>//[^\n]*)
  | (?P<block>/\*)
  | (?P<str>b?"(?:\\.|[^"\\])*")
  | (?P<char>b?'(?:\\.|[^'\\])')
  | (?P<life>'[A-Za-z_]\w*)
  | (?P<num>\d[\w.]*)
  | (?P<id>[A-Za-z_]\w*)
  | (?P<p>.)
    """,
    re.X | re.S,
)


def tokens(text):
    """Rust tokens without whitespace and comments (doc comments included);
    punctuation one character at a time, which is enough to compare."""
    out = []
    i = 0
    while i < len(text):
        m = TOKEN.match(text, i)
        kind = m.lastgroup
        if kind == "block":
            depth, j = 1, m.end()
            while depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                elif j >= len(text):
                    fail("unterminated block comment")
                else:
                    j += 1
            i = j
            continue
        if kind not in ("ws", "line"):
            out.append(m.group())
        i = m.end()
    # Visibility is not compared: Verus wants `pub` on consts that the
    # source declares `pub(crate)`.
    norm = []
    k = 0
    while k < len(out):
        if out[k : k + 4] == ["pub", "(", "crate", ")"]:
            norm.append("pub")
            k += 4
        else:
            norm.append(out[k])
            k += 1
    return norm


def drop_item(toks, head):
    """`toks` without each item that starts with the token list `head` and
    ends with its first balanced `{ ... }`."""
    out = []
    i = 0
    while i < len(toks):
        if toks[i : i + len(head)] == head:
            j = i + len(head)
            while toks[j] != "{":
                j += 1
            depth = 0
            while True:
                depth += {"{": 1, "}": -1}.get(toks[j], 0)
                j += 1
                if depth == 0:
                    break
            i = j
        else:
            out.append(toks[i])
            i += 1
    return out


# --- The mirror -------------------------------------------------------------


def strip_verus(text):
    """The executable code of a mirrored section: without spec clauses,
    proof blocks, verifier attributes, and named return values."""
    lines = text.split("\n")
    out = []
    i = 0
    indent = lambda s: len(s) - len(s.lstrip(" "))
    while i < len(lines):
        line = lines[i]
        s = line.strip()
        if s.startswith("#[verifier::"):
            i += 1
            continue
        if s in SPEC_CLAUSES:
            # Up to the line that opens the body, one level out.
            level = indent(line) - 4
            i += 1
            while i < len(lines) and not (
                lines[i].strip() and indent(lines[i]) <= level
            ):
                i += 1
            continue
        if s == "proof {":
            level = indent(line)
            i += 1
            while not (lines[i].strip() == "}" and indent(lines[i]) == level):
                i += 1
            i += 1
            continue
        if re.match(r"\s*(assert|assume|admit)\b", line):
            fail(f"a proof statement outside a `proof {{ }}` block: {s}")
        line = re.sub(r"->\s*\(\w+:\s*(.*)\)\s*$", r"-> \1", line)
        out.append(line)
        i += 1
    return "\n".join(out)


def mirror_section(proof):
    text = (VERIFY / proof).read_text()
    begin, end = "// verus-mirror: begin\n", "// verus-mirror: end\n"
    if text.count(begin) != 1 or text.count(end) != 1:
        fail(f"verify/{proof}: needs one `{begin.strip()}` and one `{end.strip()}`")
    return text[text.index(begin) + len(begin) : text.index(end)]


def check_mirror(proof, source):
    proven = tokens(strip_verus(mirror_section(proof)))
    src = drop_item(tokens((ROOT / source).read_text()), ["#", "[", "cfg", "(", "test", ")", "]", "mod"])
    if proven == src:
        return True
    k = next((k for k, (a, b) in enumerate(zip(proven, src)) if a != b), min(len(proven), len(src)))
    ctx = lambda t: " ".join(t[max(0, k - 12) : k + 12])
    print(f"drift: {source} is not what verify/{proof} proves.", file=sys.stderr)
    print(f"  source: ... {ctx(src)} ...", file=sys.stderr)
    print(f"  proof:  ... {ctx(proven)} ...", file=sys.stderr)
    print(
        f"  Copy the change into the `verus-mirror` section of verify/{proof}, then run"
        " `scripts/verus.py prove` (docs/verus.md).",
        file=sys.stderr,
    )
    return False


def check_trusted(proof):
    text = (VERIFY / proof).read_text()
    ok = True
    for m in re.finditer(r"#\[verifier::(external_body|external)\]\s*\n(.*)", text):
        name = re.search(r"\b(?:fn|const|struct|enum)\s+(\w+)", m.group(2))
        if not name or name.group(1) not in TRUSTED.get(proof, set()):
            print(f"drift: verify/{proof} trusts `{m.group(2).strip()}` without proof;"
                  " add it to TRUSTED in scripts/verus.py with the reason, or prove it",
                  file=sys.stderr)
            ok = False
    for m in re.finditer(r"\b(assume|admit)\s*\(", text):
        line = text.count("\n", 0, m.start()) + 1
        print(f"drift: verify/{proof}:{line}: `{m.group(1)}` is not allowed", file=sys.stderr)
        ok = False
    return ok


# --- Pins (models of code the proof does not include) ------------------------


def item_tokens(path, name):
    """Tokens of the fn `name` ("Type::fn" or "fn") in `path`."""
    toks = tokens((ROOT / path).read_text())
    owner, _, fn = name.rpartition("::")
    start = 0
    if owner:
        for k in range(len(toks)):
            if toks[k] == "impl" and owner in toks[k : k + 12] and "{" in toks[k : k + 14]:
                cut = toks[k : k + 14]
                if cut[cut.index("{") - 1] == owner or (
                    "for" in cut and cut[cut.index("for") + 1] == owner
                ):
                    start = k
                    break
        else:
            fail(f"{path}: no impl of {owner}")
    for k in range(start, len(toks)):
        if toks[k] == "fn" and toks[k + 1] == fn:
            j = k
            while toks[j] != "{":
                j += 1
            depth = 0
            while True:
                depth += {"{": 1, "}": -1}.get(toks[j], 0)
                j += 1
                if depth == 0:
                    return toks[k:j]
    fail(f"{path}: no fn {name}")


def pins_now():
    out = []
    pins = VERIFY / "pins.txt"
    if not pins.exists():
        return out
    for line in pins.read_text().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        _, path, name = line.split()
        digest = hashlib.sha256(" ".join(item_tokens(path, name)).encode()).hexdigest()[:16]
        out.append((digest, path, name))
    return out


def check_pins():
    pins = VERIFY / "pins.txt"
    if not pins.exists():
        return True
    want = [l.split() for l in pins.read_text().splitlines() if l.strip() and not l.startswith("#")]
    ok = True
    for (digest, path, name), (old, _, _) in zip(pins_now(), want):
        if digest != old:
            print(f"drift: {path} {name} changed since verify/ modelled it (pin {old}, now {digest}).",
                  file=sys.stderr)
            print("  Check the model in verify/ against the change (docs/verus.md says which model"
                  " covers what), then update the line in verify/pins.txt from `scripts/verus.py pins`.",
                  file=sys.stderr)
            ok = False
    return ok


# --- Running Verus ------------------------------------------------------------


def verus_bin():
    for cand in (os.environ.get("VERUS"), ROOT / "target/verus/verus-x86-linux/verus", shutil.which("verus")):
        if cand and Path(cand).exists():
            return str(cand)
    fail("Verus not found: run `scripts/verus.py install` or set VERUS")


def check_version(verus):
    v = json.loads((Path(verus).parent / "version.json").read_text())["verus"]["version"]
    if v != VERUS_VERSION:
        fail(f"Verus {v} found, {VERUS_VERSION} pinned (scripts/verus.py install)")


def run_verus(verus, root):
    t = time.monotonic()
    r = subprocess.run([verus, str(root), "--crate-type=lib"], capture_output=True, text=True)
    return r.returncode, r.stdout + r.stderr, time.monotonic() - t


def prove():
    verus = verus_bin()
    check_version(verus)
    ok = True
    for proof in PROOFS:
        code, out, secs = run_verus(verus, VERIFY / proof)
        result = re.findall(r"verification results:: .*", out)
        print(f"verify/{proof}: {result[-1] if result else 'no result'} ({secs:.1f} s)")
        if code != 0:
            print(out, file=sys.stderr)
            ok = False
    return ok


def planted():
    verus = verus_bin()
    check_version(verus)
    ok = True
    with tempfile.TemporaryDirectory() as tmp:
        for proof, what, old, new, expect in PLANTED:
            text = (VERIFY / proof).read_text()
            if text.count(old) != 1:
                print(f"planted: verify/{proof}: the text for '{what}' is not there once;"
                      " update PLANTED in scripts/verus.py", file=sys.stderr)
                ok = False
                continue
            root = Path(tmp) / proof
            root.write_text(text.replace(old, new))
            code, out, secs = run_verus(verus, root)
            caught = code != 0 and expect in out
            print(f"planted: {'caught' if caught else 'MISSED'}: {what} ({secs:.1f} s)")
            if not caught:
                print(out, file=sys.stderr)
                ok = False
    return ok


def install():
    dest = ROOT / "target/verus"
    binary = dest / "verus-x86-linux/verus"
    if binary.exists():
        try:
            check_version(binary)
            print(f"Verus {VERUS_VERSION} already in {dest}")
            return True
        except SystemExit:
            shutil.rmtree(dest / "verus-x86-linux")
    dest.mkdir(parents=True, exist_ok=True)
    archive = dest / "verus.zip"
    digest = hashlib.sha256()
    with urllib.request.urlopen(VERUS_URL, timeout=300) as r, open(archive, "wb") as f:
        while chunk := r.read(1 << 20):
            digest.update(chunk)
            f.write(chunk)
    if digest.hexdigest() != VERUS_SHA256:
        archive.unlink()
        fail(
            f"the Verus archive from {VERUS_URL} has sha256 {digest.hexdigest()}, "
            f"{VERUS_SHA256} pinned (VERUS_SHA256 in scripts/verus.py): not installing it"
        )
    with zipfile.ZipFile(archive) as z:
        for name in z.namelist():
            parts = PurePosixPath(name).parts
            if name.startswith(("/", "\\")) or ".." in parts or not parts or parts[0] != "verus-x86-linux":
                archive.unlink()
                fail(f"the Verus archive holds {name!r}, outside verus-x86-linux/: not installing it")
        z.extractall(dest)
    archive.unlink()
    for f in (dest / "verus-x86-linux").iterdir():
        f.chmod(0o755)
    subprocess.run(["rustup", "toolchain", "install", "--profile", "minimal", VERUS_TOOLCHAIN], check=True)
    print(f"Verus {VERUS_VERSION} installed in {dest}")
    return True


def drift():
    ok = all([check_mirror(p, s) for p, s in MIRRORS.items()])
    ok = all([check_trusted(p) for p in PROOFS]) and ok
    ok = check_pins() and ok
    if ok:
        print(f"drift: none ({', '.join(MIRRORS.values())} mirrored; {len(pins_now())} pins)")
    return ok


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "all"
    if cmd == "pins":
        for digest, path, name in pins_now():
            print(digest, path, name)
        return
    steps = {
        "drift": [drift],
        "prove": [prove],
        "planted": [planted],
        "install": [install],
        "all": [drift, prove, planted],
    }.get(cmd)
    if steps is None:
        fail(f"unknown command {cmd!r}\n{__doc__}")
    ok = True
    for step in steps:
        ok = step() and ok
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
