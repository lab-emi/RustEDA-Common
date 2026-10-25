#!/usr/bin/env python3
"""Maintain a bounded integrity manifest of the canonical shared source and fixtures."""
import argparse
import hashlib
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--max-files", type=int, default=10000)
    parser.add_argument("--max-bytes", type=int, default=64 * 1024 * 1024)
    args = parser.parse_args()
    if args.max_files <= 0 or args.max_bytes <= 0:
        parser.error("budgets must be positive")
    data = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--",
         "crates", "docs/specs", "verification/fixtures", "verification/pdks.toml"],
        cwd=ROOT, timeout=30,
    )
    paths = sorted(set(p.decode() for p in data.split(b"\0") if p))
    if len(paths) > args.max_files:
        raise SystemExit("MANIFEST_BUDGET: too many files")
    total = 0
    lines = []
    for name in paths:
        file = ROOT / name
        if not file.exists():
            continue
        if file.is_symlink() or not file.resolve().is_relative_to(ROOT):
            raise SystemExit("UNSAFE_PATH: " + name)
        digest = hashlib.sha256()
        with file.open("rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                total += len(chunk)
                if total > args.max_bytes:
                    raise SystemExit("MANIFEST_BUDGET: too many bytes")
                digest.update(chunk)
        lines.append(digest.hexdigest() + "  " + name + "\n")
    expected = "".join(lines)
    manifest = ROOT / "verification/shared.sha256"
    if args.check:
        if not manifest.is_file() or manifest.read_text() != expected:
            raise SystemExit("SHARED_MANIFEST_MISMATCH: run scripts/shared_manifest.py")
    else:
        manifest.write_text(expected)
    print(f"Canonical shared files verified: {len(lines)} files, {total} bytes")


if __name__ == "__main__":
    main()
