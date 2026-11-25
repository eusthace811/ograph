#!/usr/bin/env python3
"""Mirror the RCAEval cases into a directory of their own: each case's input files as symlinks
(read only) and its .ograph/config.toml as a copy, so evals can write stores there and never
into the source tree.

Usage: mirror_cases.py --cases data/rcaeval-cases --out target/rcaeval-cases
"""
import argparse, pathlib, shutil

ap = argparse.ArgumentParser(); ap.add_argument("--cases", required=True); ap.add_argument("--out", required=True)
a = ap.parse_args()
src, out = pathlib.Path(a.cases).resolve(), pathlib.Path(a.out).resolve()
out.mkdir(parents=True, exist_ok=True)
shutil.copy(src / "labels.json", out / "labels.json")
n = 0
for d in sorted(src.glob("c-*")):
    m = out / d.name
    (m / ".ograph").mkdir(parents=True, exist_ok=True)
    for f in d.iterdir():
        if f.is_file():
            link = m / f.name
            if not link.is_symlink():
                link.symlink_to(f)
    shutil.copy(d / ".ograph" / "config.toml", m / ".ograph" / "config.toml")
    n += 1
print(f"mirrored {n} cases into {out}")
