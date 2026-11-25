#!/usr/bin/env python3
"""Mirrors a directory of RCAEval cases as symlinks, so an eval harvests into the mirror and
leaves the cases untouched: every input file is a link, each case's config and labels.json are
copies.

Usage: link_cases.py SRC_CASES DEST
"""
import pathlib, shutil, sys

src, dest = pathlib.Path(sys.argv[1]).resolve(), pathlib.Path(sys.argv[2]).resolve()
dest.mkdir(parents=True, exist_ok=True)
shutil.copy(src / "labels.json", dest / "labels.json")
n = 0
for case in sorted(p for p in src.iterdir() if (p / ".ograph" / "config.toml").exists()):
    out = dest / case.name
    (out / ".ograph").mkdir(parents=True, exist_ok=True)
    shutil.copy(case / ".ograph" / "config.toml", out / ".ograph" / "config.toml")
    for f in case.iterdir():
        if f.is_file() and not (out / f.name).exists():
            (out / f.name).symlink_to(f)
    n += 1
print(f"{n} cases linked under {dest}")
