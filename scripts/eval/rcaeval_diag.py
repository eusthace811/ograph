#!/usr/bin/env python3
"""Tabulate why chain ranking misses on RCAEval cases.

Reads a results file from `rcaeval_score.py --out` and, for each case whose top-1 is wrong,
queries the case's store for every event, then names the cause:

  not_detected   no event on the true service in the window
  no_chain       events on the true service, but no gated chain rooted there
  pre_fault      the winning chain's root event is before the injection (baseline noise wins)
  late           the true service's first post-injection event comes after the winning root
  near_tie       the true service fires first and loses by under 0.01 (scores saturate)
  outranked      the true service fires first and the winner scores clearly higher

Relation says where the winner sits relative to the true service: caller (an upstream
symptom), callee, or unrelated.

Usage: rcaeval_diag.py --bin target/release/ograph --cases cases --results base.json [--mode chain]
"""
import argparse, collections, json, pathlib, re, subprocess, datetime

Q = ('SELECT ?app ?at ?sig ?v ?m WHERE { ?e evt:on ?a . ?a ops:name ?app . ?e evt:at ?at . '
     '?e evt:signal ?sig . OPTIONAL { ?e evt:value ?v } OPTIONAL { ?e evt:metric ?m } }')

def deps(catalog):
    out, name = collections.defaultdict(set), None
    for line in open(catalog):
        if m := re.match(r"\s+name:\s*(\S+)", line): name = m.group(1)
        if m := re.match(r"\s+- component:(\S+)", line): out[name].add(m.group(1))
    return out

def ts(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True)
    ap.add_argument("--results", required=True); ap.add_argument("--mode", default="chain")
    ap.add_argument("--catalog", default="data/fixtures/online-boutique/catalog-info.yaml")
    a = ap.parse_args()
    R = json.loads(pathlib.Path(a.results).read_text())
    res, labels = R["results"], R["labels"]
    D = deps(a.catalog)
    binp = str(pathlib.Path(a.bin).resolve())
    rows, causes = [], collections.Counter()
    by_fault = collections.defaultdict(collections.Counter)
    for cid in sorted(labels):
        lab, ranked = labels[cid], res[cid][a.mode]
        root = lab["root"]
        if ranked[:1] == [root]:
            continue
        d = pathlib.Path(a.cases) / cid
        inc = json.loads((d / "incidents.json").read_text())[0]["opened"]
        out = subprocess.run([binp, "sparql", "--json", Q], cwd=d, capture_output=True, text=True).stdout
        ev = [(r[0], ts(r[1]), r[2], float(r[3]) if r[3] else 0.0, r[4] or "") for r in json.loads(out)["rows"]]
        ev = [e for e in ev if inc - 1800 <= e[1] <= inc]
        inj = lab["inject"]
        def first(app, after=None):
            t = [e[1] for e in ev if e[0] == app and (after is None or e[1] >= after)]
            return min(t) if t else None
        def peak(app):
            x = [e for e in ev if e[0] == app and e[1] >= inj - 30]
            return max(x, key=lambda e: e[3]) if x else None
        ch = json.loads(subprocess.run([binp, "chains", lab["incident"], "--json", "--top", "20", "--rank", a.mode], cwd=d, capture_output=True, text=True).stdout)["apps"]
        score = {x["app"]: x["score"] for x in ch}
        win = ranked[0] if ranked else None
        rpos = ranked.index(root) + 1 if root in ranked else None
        rf = first(root, inj - 30)
        wf = ch[0]["chain"]["root"]["at_ms"] / 1000 if ch and ch[0].get("chain") else None
        if not any(e[0] == root for e in ev):
            cause = "not_detected"
        elif rpos is None:
            cause = "no_chain"
        elif wf is not None and wf < inj - 30:
            cause = "pre_fault"
        elif rf is None or (wf is not None and rf > wf):
            cause = "late"
        elif score[win] - score.get(root, 0) < 0.01:
            cause = "near_tie"
        else:
            cause = "outranked"
        rel = "caller" if win and root in D.get(win, ()) else "callee" if win and win in D.get(root, ()) else "unrelated"
        rp, wp = peak(root), peak(win) if win else None
        causes[cause] += 1; by_fault[lab["fault"]][cause] += 1
        rows.append((cid, lab["fault"], lab["rep"], root, rpos, win, rel, cause,
                     None if rf is None else int(rf - inj), None if wf is None else int(wf - inj),
                     f"{rp[4] or rp[2]}:{rp[3]:.0f}" if rp else "-", f"{wp[4] or wp[2]}:{wp[3]:.0f}" if wp else "-", f"{score[win] - score.get(root, 0):.4f}"))
    print("| case | fault | rep | root | root pos | winner | relation | cause | root first (s after inject) | winning root (s after inject) | root peak | winner peak | score gap |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for r in rows:
        print("| " + " | ".join(str(x) for x in r) + " |")
    print(f"\n{len(rows)} misses: " + ", ".join(f"{k} {v}" for k, v in causes.most_common()))
    for f, c in sorted(by_fault.items()):
        print(f"  {f}: " + ", ".join(f"{k} {v}" for k, v in c.most_common()))
    rel = collections.Counter(r[6] for r in rows)
    print("winner relation: " + ", ".join(f"{k} {v}" for k, v in rel.most_common()))

if __name__ == "__main__":
    main()
