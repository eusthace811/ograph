#!/usr/bin/env python3
"""Exact-quote alias gate on examples/demo. Harvests two scratch copies of the demo, one without
the runbooks source and one with it, and checks:

1. The aliases committed from runbooks are exactly the ones runbooks-labels.json lists; no decoy
   becomes an alias.
2. Alerts that name a service by an alias are quarantined without the runbooks and resolve to the
   catalog application with them.
3. Root-cause chains for the open incident are the same in both copies.

Usage: alias_eval.py --bin target/release/ograph --demo examples/demo"""
import argparse, json, pathlib, re, shutil, subprocess, sys, tempfile

def og(binp, root, *args):
    p = subprocess.run([binp, "--root", str(root), *args], capture_output=True, text=True)
    if p.returncode == 2: raise SystemExit(p.stderr)
    return p.stdout

def rows(binp, root, q):
    return json.loads(og(binp, root, "--json", "sparql", q))["rows"]

ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
a = ap.parse_args(); binp = str(pathlib.Path(a.bin).resolve()); demo = pathlib.Path(a.demo).resolve()
labels = json.loads((demo / "runbooks-labels.json").read_text())
want = {(app, x) for app, xs in labels["aliases"].items() for x in xs}
tmp = pathlib.Path(tempfile.mkdtemp(prefix="ograph-alias-"))
runs = {}
for name in ["without runbooks", "with runbooks"]:
    d = tmp / name.replace(" ", "-")
    shutil.copytree(demo, d, ignore=shutil.ignore_patterns("store.redb"))
    if name == "without runbooks":
        cfg = d / ".ograph" / "config.toml"
        cfg.write_text(re.sub(r'\[\[source\]\]\nkind = "docs"\npath = "runbooks"\n', "", cfg.read_text()))
    og(binp, d, "harvest")
    got = {(x.split("/")[-1], v) for x, v in rows(binp, d, 'SELECT ?a ?v WHERE { GRAPH ?g { ?a ops:alias ?v } FILTER(CONTAINS(STR(?g), "/docs/")) }')}
    quarantined = [r for (r,) in rows(binp, d, 'SELECT ?f WHERE { GRAPH ?g { ?q harv:fact ?f } FILTER(CONTAINS(STR(?g), "/alerts/")) }') if "pay-gw" in r or "finder" in r]
    resolved = rows(binp, d, 'SELECT ?app WHERE { ?e a evt:Alert ; evt:on ?app ; evt:summary ?s . FILTER(?s IN ("PayGwQueueDepth", "FinderSlowQueries")) }')
    docs_q = rows(binp, d, 'SELECT ?f ?r WHERE { GRAPH ?g { ?q harv:fact ?f ; harv:reason ?r } FILTER(CONTAINS(STR(?g), "/docs/")) }')
    chains = json.loads(og(binp, d, "--json", "chains", "INC-5377"))
    runs[name] = dict(got=got, quarantined=quarantined, resolved=sorted(x for (x,) in resolved), docs_q=docs_q, top=[(r["app"], round(r["score"], 4)) for r in chains["apps"][:3]])
    print(f"{name}: aliases from runbooks {sorted(got)}; alias-named alert facts quarantined {len(quarantined)}; alias-named alerts resolved to {runs[name]['resolved']}; top causes {runs[name]['top']}")
    for f, r in docs_q:
        print(f"   runbook proposal quarantined: {f} -- {r}")
w = runs["with runbooks"]
tp, fp, fn = len(w["got"] & want), sorted(w["got"] - want), sorted(want - w["got"])
decoys = sorted(v for (_, v) in w["got"] if v in labels["decoys"])
print(f"\n1. aliases: {tp}/{len(want)} expected committed, unexpected {fp}, missed {fn}, decoys committed {decoys}")
print(f"2. alias-named alerts: quarantined {len(runs['without runbooks']['quarantined'])} facts without runbooks, {len(w['quarantined'])} with; resolved to {w['resolved']}")
same = runs["with runbooks"]["top"] == runs["without runbooks"]["top"]
print(f"3. INC-5377 top causes unchanged: {same}")
ok = tp == len(want) and not fp and not decoys and len(runs["without runbooks"]["quarantined"]) > 0 and not w["quarantined"] and w["resolved"] == ["id:app/payments-api", "id:app/search-api"] and same
print("ALIAS GATE:", "PASS" if ok else "FAIL")
shutil.rmtree(tmp)
sys.exit(0 if ok else 1)
