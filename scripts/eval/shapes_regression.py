#!/usr/bin/env python3
"""Harvest the demo and every RCAEval case twice, once under a baseline shapes.ttl and once under
the candidate, and compare what the gateway quarantines. Passes when the candidate quarantines
exactly what the baseline does on every workspace.

Each workspace is rebuilt under --work with its sources linked, not copied, so the eval data is
only read.

Usage: shapes_regression.py --bin target/release/ograph --base OLD.ttl --new ontology/shapes.ttl
       --demo target/demo --cases data/rcaeval-cases --work target/shapes-regression [--jobs 3]
"""
import argparse, collections, concurrent.futures as cf, json, pathlib, shutil, subprocess

def workspace(src, dst, shapes):
    if dst.exists():
        shutil.rmtree(dst)
    (dst / ".ograph").mkdir(parents=True)
    for f in src.iterdir():
        if f.name != ".ograph":
            (dst / f.name).symlink_to(f.resolve())
    for name in ("config.toml", "ontology.ttl"):
        if (src / ".ograph" / name).exists():
            shutil.copy(src / ".ograph" / name, dst / ".ograph" / name)
    if (src / ".ograph" / "cache").exists():
        shutil.copytree(src / ".ograph" / "cache", dst / ".ograph" / "cache")
    shutil.copy(shapes, dst / ".ograph" / "shapes.ttl")

def quarantined(binary, root):
    p = subprocess.run([binary, "--root", str(root), "--json", "harvest"], capture_output=True, text=True)
    if p.returncode != 0:
        raise SystemExit(f"{root}: harvest failed: {p.stderr}")
    return collections.Counter((o["graph"], q["reason"]) for o in json.loads(p.stdout) for q in o["quarantined"])

def one(args, name, src):
    out = {}
    for label, shapes in (("base", args.base), ("new", args.new)):
        ws = args.work / label / name
        workspace(src, ws, shapes)
        out[label] = quarantined(args.bin, ws)
    return name, out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--base", type=pathlib.Path, required=True)
    ap.add_argument("--new", type=pathlib.Path, required=True)
    ap.add_argument("--demo", type=pathlib.Path, required=True)
    ap.add_argument("--cases", type=pathlib.Path, required=True)
    ap.add_argument("--work", type=pathlib.Path, required=True)
    ap.add_argument("--jobs", type=int, default=3)
    args = ap.parse_args()
    args.bin = str(pathlib.Path(args.bin).resolve())
    spaces = [("demo", args.demo)] + [(c.name, c) for c in sorted(args.cases.iterdir()) if (c / ".ograph" / "config.toml").exists()]
    changed, total = [], {"base": 0, "new": 0}
    with cf.ThreadPoolExecutor(args.jobs) as ex:
        for name, out in ex.map(lambda s: one(args, *s), spaces):
            for k in total:
                total[k] += sum(out[k].values())
            if out["base"] != out["new"]:
                changed.append((name, out["new"] - out["base"], out["base"] - out["new"]))
    print(f"workspaces: {len(spaces)} (demo + {len(spaces) - 1} RCAEval cases)")
    print(f"facts quarantined: baseline {total['base']}, candidate {total['new']}")
    for name, added, gone in changed:
        for (g, r), n in sorted(added.items()):
            print(f"  {name}: +{n} {g}: {r}")
        for (g, r), n in sorted(gone.items()):
            print(f"  {name}: -{n} {g}: {r}")
    print(f"workspaces whose quarantine changed: {len(changed)}")
    print("SHAPES REGRESSION:", "PASS" if not changed else "FAIL")
    raise SystemExit(0 if not changed else 1)

if __name__ == "__main__":
    main()
