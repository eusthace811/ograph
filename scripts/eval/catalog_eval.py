#!/usr/bin/env python3
"""Does a catalog read from real Kubernetes manifests rank root causes as well as a hand-written one?

RCAEval RE2-OB runs on Online Boutique. The fixture catalog is a Backstage file written by hand
with 15 dependencies. The project's own manifests declare none: a service learns where another
one is from an environment variable, which `ogrep --entities` reports as an inferred `calls`
edge. This scores every case three times: the hand-written catalog, the manifests with declared
dependencies only (`inferred = false`), and the manifests with the inferred calls (the default).

  catalog_eval.py --bin target/release/ograph --cases data/rcaeval-cases --manifests DIR --work DIR

DIR for --manifests is a git repository holding release/kubernetes-manifests.yaml of
GoogleCloudPlatform/microservices-demo. With --edges REPO it also counts, for any repository,
how many inferred calls a file also declares.
"""
import argparse, concurrent.futures as cf, json, pathlib, re, subprocess, collections

def run(cmd, cwd):
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr

def edges(repo):
    rc, out, _ = run(["ogrep", "--entities", ".", "--json"], repo)
    declared, calls = set(), set()
    for line in out.splitlines():
        for e in json.loads(line)["data"].get("edges", []):
            (declared if e["relation"] == "depends_on" else calls if e["relation"] == "calls" else set()).add((e["from"], e["to"]))
    return declared, calls

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True)
    ap.add_argument("--manifests", required=True); ap.add_argument("--work", required=True)
    ap.add_argument("--jobs", type=int, default=6); ap.add_argument("--edges", action="append", default=[])
    a = ap.parse_args()
    binp = str(pathlib.Path(a.bin).resolve()); cases = pathlib.Path(a.cases).resolve(); work = pathlib.Path(a.work).resolve()
    manifests = pathlib.Path(a.manifests).resolve()
    labels = json.loads((cases / "labels.json").read_text())
    print(run(["ogrep", "--version"], ".")[1].strip())
    for repo in [manifests] + [pathlib.Path(r).resolve() for r in a.edges]:
        declared, calls = edges(repo)
        print(f"{repo.name}: {len(declared)} declared dependencies, {len(calls)} inferred calls, {len(calls & declared)} of them also declared, {len(calls - declared)} only inferred")
        for f, t in sorted(calls - declared)[:40]:
            print(f"    only inferred: {f} -> {t}")
    variants = {"hand-written catalog": None, "manifests, declared only": False, "manifests, with inferred calls": True}
    def one(job):
        name, cid = job
        src = cases / cid; d = work / re.sub(r"\W+", "-", name) / cid
        (d / ".ograph").mkdir(parents=True, exist_ok=True)
        for f in src.iterdir():
            if f.name != ".ograph" and not (d / f.name).exists():
                (d / f.name).symlink_to(f)
        cfg = (src / ".ograph" / "config.toml").read_text()
        if variants[name] is not None:
            cfg = re.sub(r'(kind = "catalog"\npath = )"[^"]*"', lambda m: f'{m.group(1)}"{manifests}"\ninferred = {str(variants[name]).lower()}', cfg)
        (d / ".ograph" / "config.toml").write_text(cfg)
        for f in ("ontology.ttl", "shapes.ttl"):
            if (src / ".ograph" / f).exists(): (d / ".ograph" / f).write_text((src / ".ograph" / f).read_text())
        (d / ".ograph" / "store.redb").unlink(missing_ok=True)
        rc, out, err = run([binp, "harvest", "--json"], d)
        if rc != 0: return name, cid, None, err.strip()[-200:], 0
        external = sum("frontend-external" in q["fact"] for o in json.loads(out) for q in o["quarantined"])
        rc, out, err = run([binp, "chains", labels[cid]["incident"], "--json", "--top", "5"], d)
        apps = [x["app"] for x in json.loads(out)["apps"]] if out.strip() else []
        rc, out, err = run([binp, "sparql", "SELECT (COUNT(*) AS ?n) WHERE { ?a ops:dependsOn ?b }", "--json"], d)
        return name, cid, apps, out, external
    jobs = [(n, c) for n in variants for c in sorted(labels)]
    res = collections.defaultdict(dict); deps = {}; held = collections.Counter()
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for name, cid, apps, extra, external in ex.map(one, jobs):
            if apps is None: print("error", name, cid, extra); continue
            res[name][cid] = apps; deps.setdefault(name, extra); held[name] += external
    print(f"\nRCAEval RE2-OB, chain ranking, {len(labels)} cases\n\n| catalog | `ops:dependsOn` in the store of the first case | AC@1 | AC@3 |\n|---|---|---|---|")
    for name in variants:
        pos = [(res[name][c].index(labels[c]["root"]) + 1 if labels[c]["root"] in res[name].get(c, []) else 99) for c in sorted(labels)]
        n = json.loads(deps[name])["rows"][0][0] if deps.get(name) else "?"
        print(f"| {name} | {n} | {sum(p <= 1 for p in pos)/len(pos):.2f} ({sum(p <= 1 for p in pos)}/{len(pos)}) | {sum(p <= 3 for p in pos)/len(pos):.2f} |")
    print("\nThe store also holds the dependencies the trace enricher infers from parent spans, which is why the second row is not 0.")
    events = collections.Counter()
    for cid in sorted(labels):
        d = work / re.sub(r"\W+", "-", "manifests, with inferred calls") / cid
        rows = json.loads(run([binp, "sparql", 'SELECT ?a (COUNT(?e) AS ?n) WHERE { ?e evt:on ?a ; evt:signal ?s . FILTER(CONTAINS(STR(?a), "frontend")) } GROUP BY ?a', "--json"], d)[1] or '{"rows": []}')["rows"]
        for app, n in rows: events[app.rsplit("/", 1)[-1]] += int(n)
    print(f"events on the applications named frontend*, manifests with inferred calls, all cases: {dict(sorted(events.items()))}")
    print(f"\nfacts about `frontend-external`, the name the telemetry of the Kubernetes Service in front of `frontend` carries, held in quarantine over all cases: " + "; ".join(f"{n}: {held[n]}" for n in variants))
    a_, b_ = res["hand-written catalog"], res["manifests, with inferred calls"]
    differ = [c for c in labels if a_.get(c) != b_.get(c)]
    without = lambda apps: [x for x in apps if x != "redis"]
    print(f"\nhand-written catalog against the manifests with inferred calls: first application differs in {sum(a_[c][:1] != b_[c][:1] for c in differ)} cases, the list of five in {len(differ)}")
    print(f"the hand-written catalog names the cache `redis`, as the telemetry does, and the manifests name it `redis-cart`; in {sum(without(a_[c])[:len(without(b_[c]))] == without(b_[c])[:len(without(a_[c]))] for c in differ)} of those {len(differ)} the lists agree once `redis` is taken out")

if __name__ == "__main__":
    main()
