#!/usr/bin/env python3
"""Which crates' fuzz targets go through the crates a change touches.

Usage: fuzz-reach.py '<JSON list of changed crates>'
Prints the JSON list of crates whose fuzz targets should run: each changed
crate, and each crate whose fuzz package depends on a changed one, directly
or through other path dependencies (giong-http's targets go through
giong-quic, giong-web's through giong-http...). A crate is named by its top
directory, `giong` for the repository root, as the fuzz workflow names them.

A change to the core crate (`giong`) does not spread: every fuzz package
depends on it, and a push to main runs every target anyway.
"""

import json
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def owner(path: Path) -> str:
    """The crate a directory belongs to: its top directory, or `giong`."""
    rel = path.resolve().relative_to(ROOT)
    return rel.parts[0] if rel.parts and rel.parts[0].startswith("giong-") else "giong"


def path_deps(manifest_dir: Path) -> list[Path]:
    """The directories of the path dependencies in `manifest_dir/Cargo.toml`."""
    data = tomllib.loads((manifest_dir / "Cargo.toml").read_text())
    # Build dependencies count: giong-grpc-build generates code others build.
    kinds = ("dependencies", "build-dependencies")
    tables = [data.get(k, {}) for k in kinds]
    tables += [t.get(k, {}) for t in data.get("target", {}).values() for k in kinds]
    found = []
    for table in tables:
        for spec in table.values():
            if isinstance(spec, dict) and "path" in spec:
                found.append((manifest_dir / spec["path"]).resolve())
    return found


def reach(manifest_dir: Path) -> set[str]:
    """Every crate `manifest_dir` depends on through path dependencies."""
    seen, todo, crates = set(), [manifest_dir.resolve()], set()
    while todo:
        d = todo.pop()
        if d in seen:
            continue
        seen.add(d)
        crates.add(owner(d))
        todo.extend(path_deps(d))
    return crates


def main() -> None:
    changed = set(json.loads(sys.argv[1]))
    spreading = changed - {"giong"}
    run = set(changed)
    for fuzz in [ROOT / "fuzz", *sorted(ROOT.glob("giong-*/fuzz"))]:
        if not (fuzz / "Cargo.toml").exists():
            continue
        crate = owner(fuzz)
        if reach(fuzz) & spreading:
            run.add(crate)
    print(json.dumps(sorted(run)))


if __name__ == "__main__":
    main()
