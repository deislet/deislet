#!/usr/bin/env python3
"""Collect the source trees deis-runtime still opens at compile-time paths.

deno_core records extension JS as LoadedFromFsDuringSnapshot, so a binary
copied to another machine looks for the original absolute paths. This copies
those trees into a staging directory that remote-install overlays onto `/`.
"""

from __future__ import annotations

import argparse
import re
import shutil
import sys
from pathlib import Path

CRATE_DIR_RE = re.compile(
    rb"((?:/home|/ext|/usr|/opt|/var)/[A-Za-z0-9._+/-]*?"
    rb"/index\.crates\.io-[0-9a-f]+/deno_[A-Za-z0-9_.-]+)"
)
RUNTIME_DIR_RE = re.compile(
    rb"((?:/home|/ext|/usr|/opt|/var)/[A-Za-z0-9._+/-]*?/crates/deis-runtime)"
)


def unique_existing_dirs(data: bytes, pattern: re.Pattern[bytes]) -> list[Path]:
    found: list[Path] = []
    seen: set[Path] = set()
    for match in pattern.finditer(data):
        raw = match.group(1).decode("ascii", errors="ignore")
        # Concatenated junk after the directory name (`<snapshot>`, `ext:`)
        raw = raw.split("<")[0]
        path = Path(raw)
        if path in seen:
            continue
        seen.add(path)
        if path.is_dir():
            found.append(path)
    return found


def copy_tree(src: Path, dest_root: Path) -> Path:
    dest = dest_root / src.relative_to("/")
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(src, dest, dirs_exist_ok=True)
    return dest


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="path to deis-runtime")
    parser.add_argument(
        "dest",
        type=Path,
        help="staging directory; contents are relative to /",
    )
    args = parser.parse_args()

    if not args.binary.is_file():
        print(f"error: missing binary {args.binary}", file=sys.stderr)
        return 1

    data = args.binary.read_bytes()
    runtime_dirs = unique_existing_dirs(data, RUNTIME_DIR_RE)
    crate_dirs = unique_existing_dirs(data, CRATE_DIR_RE)
    roots: list[Path] = []
    for runtime_dir in runtime_dirs:
        src = runtime_dir / "src"
        roots.append(src if src.is_dir() else runtime_dir)
    roots.extend(crate_dirs)
    # Unique, keep order
    ordered: list[Path] = []
    seen: set[Path] = set()
    for root in roots:
        if root in seen:
            continue
        seen.add(root)
        ordered.append(root)

    if not ordered:
        print(
            f"error: {args.binary} has no compile-time source directories "
            "that exist on this machine; build and ship from the same host",
            file=sys.stderr,
        )
        return 1

    args.dest.mkdir(parents=True, exist_ok=True)
    for root in ordered:
        copied = copy_tree(root, args.dest)
        print(f"runtime-fs {root} -> {copied}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
