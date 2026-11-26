#!/usr/bin/env python3
"""rank1.py BIN STAGE_ROOT LABELS [--prefix rcaeval-] [--set k=v ...]: chains rank 1 per case on harvested stores."""
import json, subprocess, sys, pathlib, concurrent.futures as cf
b, root, labels = sys.argv[1], pathlib.Path(sys.argv[2]), json.load(open(sys.argv[3]))
args = sys.argv[4:]; prefix = ""
if args[:1] == ["--prefix"]: prefix = args[1]; args = args[2:]
sets = [x for a in args for x in ("--set", a.split("--set=")[-1])] if args else []
sets = []
it = iter(args)
for a in it:
    if a == "--set": sets += ["--set", next(it)]
def one(cid):
    lab = labels[cid]
    p = subprocess.run([b, "--json", "chains", lab["incident"], "--top", "10", *sets], cwd=root / (prefix + cid), capture_output=True, text=True)
    apps = [x["app"] for x in json.loads(p.stdout)["apps"]] if p.stdout.strip() else []
    return cid, apps
with cf.ThreadPoolExecutor(8) as ex: res = dict(ex.map(one, sorted(labels)))
out = {c: {"rank1": a[0] if a else None, "right": bool(a) and a[0] == labels[c]["root"], "apps": a[:5], "type": labels[c].get("distractor")} for c, a in res.items()}
json.dump(out, sys.stdout, indent=1)
