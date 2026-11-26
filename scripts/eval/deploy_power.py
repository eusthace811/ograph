#!/usr/bin/env python3
"""chains rank 1 with own_degradation off (Parent) and on (Candidate) over a distractor suite.

Usage: deploy_power.py --bin BIN --suite DIR --work DIR
Each case is staged and harvested as agent_rca_eval does; `ograph chains INC` runs as configured
and with `--set own_degradation=true`. Prints AC@1 per arm and type and the exact sign test."""
import argparse, collections, json, math, pathlib, sys
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import agent_rca_eval as rca  # noqa: E402
from distractor_eval import TYPES  # noqa: E402

ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--suite", required=True); ap.add_argument("--work", required=True)
a = ap.parse_args()
suite = pathlib.Path(a.suite).resolve(); labels = json.loads((suite / "labels.json").read_text())
ctx = rca.Ctx(argparse.Namespace(work=a.work, bin=a.bin, cases=str(suite), guidance=None)); ctx.setup()
rows = {}
for cid, lab in sorted(labels.items()):
    staged = rca.stage(ctx, rca.Case("rcaeval", cid, lab["incident"], lab["root"], suite / cid))
    rca.run([ctx.bin, "harvest", "--json"], staged, env=ctx.env(ctx.bin.parent))
    r = {}
    for arm, extra in (("parent", []), ("candidate", ["--set", "own_degradation=true"])):
        p = rca.run([ctx.bin, "--json", "chains", lab["incident"], "--top", "10", *extra], staged)
        if p.returncode not in (0, 1) or not p.stdout.strip(): raise SystemExit(f"{cid} {arm}: {p.stderr}")
        apps = [x["app"] for x in json.loads(p.stdout)["apps"]]
        r[arm] = apps[:1] == [lab["root"]]
    rows[cid] = {"type": lab["distractor"], **r}
(pathlib.Path(a.work) / "power.json").write_text(json.dumps(rows, indent=1))
print("| type | cases | Parent | Candidate | better | worse |\n|---|---|---|---|---|---|")
for k, name in [*TYPES.items(), ("all", "all")]:
    rs = [r for r in rows.values() if k == "all" or r["type"] == k]
    b = sum(r["candidate"] and not r["parent"] for r in rs); w = sum(r["parent"] and not r["candidate"] for r in rs)
    print(f"| {k} {name} | {len(rs)} | {sum(r['parent'] for r in rs)} | {sum(r['candidate'] for r in rs)} | {b} | {w} |")
n = b + w; p = min(1.0, 2 * sum(math.comb(n, i) for i in range(min(b, w) + 1)) / 2 ** n) if n else 1.0
print(f"sign test over {len(rows)} cases: better {b}, worse {w}, p = {p:.2g}")
