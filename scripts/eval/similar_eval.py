#!/usr/bin/env python3
"""Investigation-memory eval: all RCAEval cases in one store, each incident's true chain
accepted as a verdict, then for every incident `ograph similar` over the incidents before it.
Scores whether the most similar past incident's accepted cause is this incident's root cause.
Chance is about 1/5 (five root-cause services, evenly split).

Usage: similar_eval.py --bin target/release/ograph --cases data/rcaeval-cases --out data/rcaeval-merged
"""
import argparse, json, pathlib, subprocess, collections, time, sys
from datetime import datetime

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True); ap.add_argument("--out", required=True)
    a = ap.parse_args(); binp = str(pathlib.Path(a.bin).resolve()); cases = pathlib.Path(a.cases).resolve(); out = pathlib.Path(a.out).resolve()
    labels = json.loads((cases / "labels.json").read_text())
    (out / ".ograph").mkdir(parents=True, exist_ok=True); (out / ".ograph" / "store.redb").unlink(missing_ok=True)
    first = cases / sorted(labels)[0]
    cfg = (first / ".ograph" / "config.toml").read_text().split("[[source]]")
    catalog = [s for s in cfg if 'kind = "catalog"' in s][0]
    src = "[[source]]" + catalog + f'[[source]]\nkind = "ownership"\npath = "{first}/ownership.jsonl"\n\n'
    for cid in sorted(labels):
        for kind, f in [("incidents", "incidents.json"), ("metrics", "metrics.csv"), ("logs", "logs.jsonl")]:
            src += f'[[source]]\nkind = "{kind}"\npath = "{cases / cid / f}"\n\n'
    (out / ".ograph" / "config.toml").write_text(src + "[correlation]\nwindow_s = 1800\nrank = \"chain\"\n")
    t = time.time()
    p = subprocess.run([binp, "--root", str(out), "harvest"], capture_output=True, text=True)
    print(f"merged harvest: {time.time() - t:.1f} s, rc {p.returncode}; {p.stdout.strip().splitlines()[-1] if p.stdout.strip() else p.stderr[-300:]}")
    if p.returncode:
        sys.exit("merged harvest failed")

    def og(*x, ok=(0,)):
        r = subprocess.run([binp, "--root", str(out), "--json", *x], capture_output=True, text=True)
        if r.returncode not in ok:
            sys.exit(f"ograph {' '.join(x[:2])} failed (rc {r.returncode}): {r.stderr.strip()[-300:]}")
        return json.loads(r.stdout) if r.stdout.strip() else None

    opened = {i: datetime.fromisoformat(at) for i, at in og("sparql", "SELECT ?id ?at WHERE { ?i inc:id ?id ; evt:at ?at }")["rows"]}
    accepted = []
    for cid, l in labels.items():
        rows = og("sparql", f'SELECT ?c WHERE {{ ?c corr:leadsTo ?i ; corr:candidateApp id:app\\/{l["root"]} . ?i inc:id "{l["incident"]}" }} ORDER BY ?c LIMIT 1', ok=(0, 1))["rows"]
        if rows:
            og("verdict", rows[0][0], "accept", "--evidence", "RCAEval label", "--by", "eval"); accepted.append(l["incident"])
    print(f"accepted a true chain for {len(accepted)}/{len(labels)} incidents (the others had no chain rooted at the true service)")
    # Every incident opened after some accepted one is scored; a query that finds no past cause is a miss.
    eligible = {cid: l for cid, l in labels.items() if any(opened[a] < opened[l["incident"]] for a in accepted)}
    hit1 = hit3 = 0; n = len(eligible); per = collections.Counter(); split = collections.defaultdict(lambda: [0, 0, 0])
    for cid, l in eligible.items():
        sims = [s for s in og("similar", l["incident"], "--top", "1000", ok=(0, 1)) if s["cause_apps"]][:3]
        top = sims[0]["cause_apps"] if sims else []
        top3 = {x for s in sims for x in s["cause_apps"]}
        hit1 += l["root"] in top; hit3 += l["root"] in top3; per[l["fault"]] += l["root"] in top
        sp = split["tune (rep 1)" if l["rep"] == 1 else "held out (reps 2-3)"]; sp[0] += l["root"] in top; sp[1] += l["root"] in top3; sp[2] += 1
    print(f"incidents with an earlier accepted incident to compare: {n}")
    print(f"top-1 similar past incident shares the root cause: {hit1}/{n} = {hit1 / max(n, 1):.2f}; any of top 3: {hit3 / max(n, 1):.2f}; chance about 0.20")
    print("per fault (top-1 hits):", dict(per))
    for k, (h1, h3, m) in sorted(split.items(), reverse=True):
        print(f"{k}: top-1 {h1}/{m} = {h1 / m:.2f}; top 3 {h3 / m:.2f}")
    if not n:
        sys.exit("no incident had an earlier accepted incident to compare")

main()
