#!/usr/bin/env python3
"""Model-enricher gate on examples/demo.

1. Accuracy: the configured model's disc:category against threads-labels.json.
2. Replay: a second harvest answers from the cache (no new cache lines) and rewrites only the
   harvest-time quad of the discussion graph.
3. Broken model: a copy of the workspace with scripts/eval/bad_model.py as the model. No value
   outside the closed vocabularies, no unknown application and no low-confidence fact may be
   committed, and the full SHACL audit must stay clean.

Usage: llm_gate.py --bin target/release/ograph --demo examples/demo
"""
import argparse, json, pathlib, shutil, subprocess, sys, tempfile

def og(binp, root, *args):
    p = subprocess.run([binp, "--root", str(root), *args], capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr

def rows(binp, root, q):
    rc, out, err = og(binp, root, "--json", "sparql", q)
    if rc not in (0, 1): raise SystemExit(err)
    return json.loads(out)["rows"]

MIN_ACCURACY = 0.85

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
    a = ap.parse_args(); binp = str(pathlib.Path(a.bin).resolve()); demo = pathlib.Path(a.demo).resolve()
    labels = json.loads((demo / "threads-labels.json").read_text())
    cache = demo / ".ograph" / "cache" / "llm.jsonl"
    got = rows(binp, demo, "SELECT ?at ?c WHERE { ?t a disc:Thread ; disc:at ?at ; disc:category ?c }")
    by_sec = {k.split(".")[0]: v for k, v in labels.items()}
    import datetime
    hits = total = 0; misses = []
    for at, cat in got:
        sec = str(int(datetime.datetime.fromisoformat(at.replace("Z", "+00:00")).timestamp()))
        if sec in by_sec:
            total += 1; hits += cat == by_sec[sec]
            if cat != by_sec[sec]: misses.append((by_sec[sec], cat))
    print(f"1. accuracy: {hits}/{len(labels)} threads classified as labeled ({total} classified); misses (gold -> model): {misses}")
    declared = {tuple(r) for r in rows(binp, demo, "SELECT ?a ?b WHERE { GRAPH ?g { ?a ops:dependsOn ?b } FILTER(CONTAINS(STR(?g), \"/catalog/\")) }")}
    model_edges = rows(binp, demo, "SELECT ?a ?b WHERE { GRAPH ?g { ?a ops:dependsOn ?b } FILTER(CONTAINS(STR(?g), \"/discussions/\")) }")
    unconfirmed = rows(binp, demo, "SELECT ?f WHERE { GRAPH ?g { ?q harv:fact ?f ; harv:reason ?r } FILTER(STRSTARTS(?r, \"unconfirmed\")) }")
    print(f"   model-proposed calls committed: {len(model_edges)}, of them declared in the catalog: {sum(tuple(e) in declared for e in model_edges)}; quarantined as unconfirmed: {[u[0] for u in unconfirmed]}")
    before = len(cache.read_text().splitlines()) if cache.exists() else 0
    rc, out, err = og(binp, demo, "harvest")
    after = len(cache.read_text().splitlines()) if cache.exists() else 0
    line = [l for l in out.splitlines() if "discussions" in l]
    print(f"2. replay: cache lines {before} -> {after} (new model calls: {after - before}); {line[0] if line else out[-200:]}")
    replay_ok = after == before and line and "+1 -1 quads" in line[0]
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="ograph-badmodel-"))
    shutil.copytree(demo, tmp / "demo", ignore=shutil.ignore_patterns("store.redb", "cache"))
    cfg = (tmp / "demo" / ".ograph" / "config.toml").read_text()
    head = cfg.split("[llm.extract]")[0]
    bad = pathlib.Path(__file__).with_name("bad_model.py").resolve()
    (tmp / "demo" / ".ograph" / "config.toml").write_text(head + f'[llm.extract]\ncommand = ["python3", "{bad}"]\nmodel = "broken-fixture"\nparallel = 4\n')
    rc, out, err = og(binp, tmp / "demo", "harvest")
    out_h = out
    print("3. broken model harvest:", [l for l in out.splitlines() if "discussions" in l])
    vocab = {"DEPLOYMENT", "CONFIG", "CAPACITY", "DEPENDENCY", "NETWORK", "DATA", "SECURITY", "UNKNOWN"}
    cats = rows(binp, tmp / "demo", "SELECT ?c WHERE { ?t disc:category ?c }")
    sents = rows(binp, tmp / "demo", "SELECT ?s WHERE { ?t disc:sentiment ?s }")
    abouts = rows(binp, tmp / "demo", "SELECT ?a WHERE { ?t a disc:Thread ; disc:about ?a . FILTER(STRSTARTS(STR(?a), \"https://ograph.dev/id/app/\")) }")
    lowconf = rows(binp, tmp / "demo", "SELECT ?f WHERE { ?f a harv:LlmFact ; corr:confidence ?c . FILTER(?c < 0.5) }")
    bad_cat = [c for (c,) in cats if c not in vocab]
    bad_sent = [s for (s,) in sents if s not in {"calm", "concerned", "urgent"}]
    unknown_app = [x for (x,) in abouts if "made-up" in x]
    bad_edges = rows(binp, tmp / "demo", "SELECT ?a ?b WHERE { GRAPH ?g { ?a ops:dependsOn ?b } FILTER(CONTAINS(STR(?g), \"/discussions/\")) }")
    cat_edges = {tuple(r) for r in rows(binp, tmp / "demo", "SELECT ?a ?b WHERE { GRAPH ?g { ?a ops:dependsOn ?b } FILTER(CONTAINS(STR(?g), \"/catalog/\")) }")}
    invented_edges = [e for e in bad_edges if tuple(e) not in cat_edges]
    q = rows(binp, tmp / "demo", "SELECT ?r WHERE { GRAPH ?g { ?q a harv:Quarantined ; harv:reason ?r } FILTER(CONTAINS(STR(?g), \"discussions\")) }")
    rc, out, _ = og(binp, tmp / "demo", "validate")
    print(f"   committed: categories {sorted(set(c for (c,) in cats))}, out-of-vocabulary {bad_cat + bad_sent}, invented apps {unknown_app}, low-confidence model facts {len(lowconf)}, invented calls {invented_edges}")
    print(f"   quarantined {len(q)}: " + "; ".join(sorted({r[0][:70] for r in q})))
    print(f"   audit: {out.strip().splitlines()[-1]}")
    modes = {"not JSON": "not JSON" in out_h, "out of vocabulary": any("closed vocabulary" in r[0] for r in q),
             "invented app": any("catalog" in r[0] for r in q), "low confidence": any("below" in r[0] for r in q),
             "unconfirmed call": any(r[0].startswith("unconfirmed") for r in q)}
    print(f"   failure modes exercised: {modes}")
    ok = hits >= MIN_ACCURACY * len(labels) and all(modes.values()) and replay_ok and not bad_cat and not bad_sent and not unknown_app and not lowconf and not invented_edges and rc == 0
    print("MODEL GATE:", "PASS" if ok else "FAIL")
    shutil.rmtree(tmp)
    sys.exit(0 if ok else 1)

main()
