#!/usr/bin/env python3
"""Bound the sstate cache between builds, keeping the most recently used artifacts."""

import argparse
from pathlib import Path


def prune(directory, max_bytes):
    if max_bytes == 0 or not directory.exists():
        return 0
    groups = {}
    # BitBake touches archives/signatures on cache hits. Treat an archive and
    # its signatures as one entry so pruning never leaves stale sidecars.
    for path in directory.rglob("*"):
        if path.is_symlink() or not path.is_file():
            continue
        name = path.name
        if name.endswith(".tar.zst"):
            archive = path
        elif name.endswith((".tar.zst.siginfo", ".tar.zst.sig")):
            archive = path.with_suffix("")
        else:
            continue
        groups.setdefault(archive, []).append(path)
    entries = []
    for paths in groups.values():
        stats = [path.stat() for path in paths]
        entries.append((max(stat.st_mtime for stat in stats), sum(stat.st_size for stat in stats), paths))
    total = sum(size for _, size, _ in entries)
    removed = 0
    for _, size, paths in sorted(entries, key=lambda entry: entry[0]):
        if total <= max_bytes:
            break
        for path in paths:
            path.unlink()
        total -= size
        removed += size
    return removed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--max-gb", type=int, default=64, help="GiB limit; 0 disables pruning")
    args = parser.parse_args()
    if args.max_gb < 0:
        parser.error("max-gb must be nonnegative")
    removed = prune(args.directory, args.max_gb * 1024**3)
    print(f"Sstate cleanup: removed {removed / 1024**3:.2f} GiB; limit {args.max_gb} GiB (0 = unlimited)")


if __name__ == "__main__":
    main()
