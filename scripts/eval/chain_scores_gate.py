#!/usr/bin/env python3
"""Gate for the per-link score breakdown of `ograph chains`.

For every RCAEval case, both rank modes and two weight settings, every chain `chains --json`
returns must satisfy, within 1e-9:
  link score  = known_pair_prior                     when the link is a known pair
              = w_time * time + w_severity * severity otherwise
  confidence  = geometric mean of the link scores (a score under 1e-9 counts as 1e-9)
  weakest     = the first link with the lowest score
and the `corr:confidence` a harvest stored must be the default-setting confidence rounded to
the four decimals the graph keeps.

RCAEval has no known pair on a best chain, so `--also` adds a harvested workspace that has
them (examples/demo) and checks every incident in it the same way.

Usage: chain_scores_gate.py --bin target/release/ograph --cases CASES [--reuse] [--also WORKSPACE]
"""
import argparse, concurrent.futures as cf, json, math, pathlib, subprocess, sys

TOL = 1e-9
SETTINGS = [
    ("default weights", {"w_time": 0.6, "w_severity": 0.4, "known_pair_prior": 0.9}),
    ("other weights", {"w_time": 0.5, "w_severity": 0.3, "known_pair_prior": 0.8}),
]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True)
    ap.add_argument("--jobs", type=int, default=3); ap.add_argument("--reuse", action="store_true")
    ap.add_argument("--also", default=None)
    a = ap.parse_args()
    binp = str(pathlib.Path(a.bin).resolve()); root = pathlib.Path(a.cases)
    labels = json.loads((root / "labels.json").read_text())

    def og(d, *x):
        p = subprocess.run([binp, *x, "--json"], cwd=d, capture_output=True, text=True)
        if p.returncode not in (0, 1) or not p.stdout.strip():
            sys.exit(f"{d.name}: ograph {' '.join(x[:2])} failed (rc {p.returncode}): {p.stderr.strip()[-300:]}")
        return json.loads(p.stdout)

    def one(cid):
        d = root / cid
        if not (a.reuse and (d / ".ograph" / "store.redb").exists()):
            (d / ".ograph" / "store.redb").unlink(missing_ok=True)
            og(d, "harvest")
        return check(d, cid, [labels[cid]["incident"]])

    def check(d, cid, incidents):
        n = {"chains": 0, "links": 0, "known": 0, "stored": 0, "edges": {}}
        worst = {"link": 0.0, "chain": 0.0, "stored": 0.0}; bad = []
        by_id = {}
        for name, st in SETTINGS:
            sets = [x for k, v in st.items() for x in ("--set", f"{k}={v}")]
            for mode, inc in [(m, i) for m in ("chain", "propagation") for i in incidents]:
                for r in og(d, "chains", inc, "--top", "1000", "--rank", mode, *sets)["apps"]:
                    c = r.get("chain")
                    if not c:
                        continue
                    path = [c["root"]] + c["links"]
                    scores = c["scores"]
                    if len(scores) != len(path):
                        bad.append(f"{cid} {c['id']}: {len(scores)} scores for {len(path)} events"); continue
                    n["chains"] += 1
                    for k, l in enumerate(scores):
                        n["links"] += 1; n["known"] += l["known_pair"]
                        n["edges"][l["edge"]] = n["edges"].get(l["edge"], 0) + 1
                        want = st["known_pair_prior"] if l["known_pair"] else st["w_time"] * l["time"] + st["w_severity"] * l["severity"]
                        worst["link"] = max(worst["link"], abs(want - l["score"]))
                        src = path[k]; to = f"{path[k + 1]['app_name']}:{path[k + 1]['kind']}" if k + 1 < len(path) else "incident"
                        if (l["from"], l["to"]) != (f"{src['app_name']}:{src['kind']}", to) or abs(l["severity"] - min(src["severity"], 1.0)) > TOL or not 0 <= l["time"] <= 1:
                            bad.append(f"{cid} {c['id']} link {k}: {l}")
                    conf = math.exp(sum(math.log(max(l["score"], 1e-9)) for l in scores) / len(scores))
                    worst["chain"] = max(worst["chain"], abs(conf - c["confidence"]))
                    low = min(scores, key=lambda l: l["score"])
                    if c["weakest"] != {"from": low["from"], "to": low["to"], "score": low["score"]}:
                        bad.append(f"{cid} {c['id']}: weakest {c['weakest']} is not {low}")
                    if name == "default weights" and mode == "chain":
                        by_id[c["id"]] = c["confidence"]
        for iri, conf in og(d, "sparql", "SELECT ?c ?v WHERE { ?c a corr:Chain ; corr:confidence ?v }")["rows"]:
            full = iri.replace("id:", "https://ograph.dev/id/") if iri.startswith("id:") else iri.strip("<>")
            if full not in by_id:
                bad.append(f"{cid}: stored chain {iri} is not in the chains answer"); continue
            n["stored"] += 1; worst["stored"] = max(worst["stored"], abs(round(by_id[full], 4) - float(conf)))
        return n, worst, bad

    tot = {"chains": 0, "links": 0, "known": 0, "stored": 0}; edges = {}
    worst = {"link": 0.0, "chain": 0.0, "stored": 0.0}; bad = []
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        runs = list(ex.map(one, sorted(labels)))
    where = f"{len(labels)} cases"
    if a.also:
        d = pathlib.Path(a.also)
        incidents = [r[0] for r in og(d, "sparql", "SELECT ?id WHERE { ?i inc:id ?id } ORDER BY ?id")["rows"]]
        runs.append(check(d, d.name, incidents)); where += f" and {len(incidents)} incidents of {a.also}"
    for n, w, b in runs:
        for k in tot: tot[k] += n[k]
        for k, v in n["edges"].items(): edges[k] = edges.get(k, 0) + v
        for k in worst: worst[k] = max(worst[k], w[k])
        bad += b
    print(f"{where}, 2 rank modes, 2 weight settings: {tot['chains']} chains, {tot['links']} links ({tot['known']} known pairs)")
    print("dependency edge behind each link:", dict(sorted(edges.items())))
    print(f"largest |recomputed - reported| link score: {worst['link']:.3e}")
    print(f"largest |geometric mean of link scores - reported chain confidence|: {worst['chain']:.3e}")
    print(f"stored corr:confidence checked on {tot['stored']} chains; largest |round(confidence, 4) - stored|: {worst['stored']:.3e}")
    print(f"structural failures (labels, severity, weakest, stored ids): {len(bad)}")
    for b in bad[:10]: print("  ", b)
    ok = not bad and worst["link"] <= TOL and worst["chain"] <= TOL and worst["stored"] <= TOL and tot["stored"] > 0
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

main()
