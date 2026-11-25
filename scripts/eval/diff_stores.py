#!/usr/bin/env python3
"""Run `ograph ontology diff NEW.ttl --shapes NEW-shapes.ttl` against every harvested workspace
under --stores and summarize: the changes per kind, and the stored facts each workspace would
quarantine. Passes when nothing is BREAKING and nothing would be quarantined anywhere.

Usage: diff_stores.py --bin target/release/ograph --ttl ontology/ograph.ttl
       --shapes ontology/shapes.ttl --stores target/shapes-regression/base
"""
import argparse, collections, json, pathlib, subprocess, sys

ap = argparse.ArgumentParser()
ap.add_argument("--bin", required=True)
ap.add_argument("--ttl", type=pathlib.Path, required=True)
ap.add_argument("--shapes", type=pathlib.Path, required=True)
ap.add_argument("--stores", type=pathlib.Path, required=True)
a = ap.parse_args()
changes, facts, breaking = collections.Counter(), 0, 0
stores = sorted(d for d in a.stores.iterdir() if (d / ".ograph" / "store.redb").exists())
for d in stores:
    p = subprocess.run([a.bin, "--root", str(d), "--json", "ontology", "diff", str(a.ttl.resolve()), "--shapes", str(a.shapes.resolve())], capture_output=True, text=True)
    if p.returncode == 2:
        sys.exit(f"{d}: {p.stderr}")
    out = json.loads(p.stdout)
    changes.update(out["changes"])
    facts += sum(q["facts"] for q in out["quarantine"])
    breaking += any(c.startswith("BREAKING") for c in out["changes"]) or bool(out["quarantine"])
print(f"workspaces: {len(stores)}")
for c, n in sorted(changes.items()):
    print(f"  {n:>3}  {c}")
print(f"workspaces with a BREAKING change or a predicted quarantine: {breaking}; stored facts that would be quarantined: {facts}")
print("DIFF STORES:", "PASS" if breaking == 0 else "FAIL")
sys.exit(1 if breaking else 0)
