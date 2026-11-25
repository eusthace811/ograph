#!/usr/bin/env python3
"""Agent gate of lane T (docs/evidence/chronic-signals.txt): Parent against Candidate binary,
both run by agent_rca_eval.py into one runs.jsonl with a guidance label each.

  A1 mcp AC@1 on rcaeval-heldout: Candidate > Parent, paired per case (pass share over the
     case's scored runs), exact two-sided sign test p < 0.05.
  A2 skill AC@1 Candidate >= Parent on rcaeval-heldout + rcaeval-miss.
  A3 departures from chains rank 1 that name the label (mcp and skill pooled, held-out + miss):
     Candidate fraction >= Parent fraction.
  A4 misses recovered (rcaeval-miss runs naming the label, mcp and skill): Candidate >= Parent.
Reported only: wrong answers naming a chronic application (one whose every event in
`before --window 30m` is marked chronic), cost, tool calls per run.

Usage: chronic_gate.py --runs OUT/runs.jsonl --parent p --candidate c --reference REF.json
       --cases MIRROR --bin CANDIDATE_BIN [--suites rcaeval-heldout,rcaeval-miss]
"""
import argparse, collections, json, math, pathlib, statistics, subprocess

ap = argparse.ArgumentParser()
ap.add_argument("--runs", required=True); ap.add_argument("--parent", required=True); ap.add_argument("--candidate", required=True)
ap.add_argument("--reference", required=True); ap.add_argument("--cases", required=True); ap.add_argument("--bin", required=True)
ap.add_argument("--suites", default="rcaeval-heldout,rcaeval-miss")
a = ap.parse_args()
suites = a.suites.split(",")
rows = [json.loads(l) for l in open(a.runs)]
ref = json.loads(pathlib.Path(a.reference).read_text())["results"]
labels = json.loads((pathlib.Path(a.cases) / "labels.json").read_text())
SCORED = ("pass", "fail")


def cid(r): return r["case"].split("/")[1]
def suite(r): return r["case"].split("/")[0]
def sel(arm, label, ss):
    return [r for r in rows if r["arm"] == f"{arm}@{label}" and suite(r) in ss]


def sign_p(better, worse):
    n, k = better + worse, min(better, worse)
    if n == 0: return 1.0
    return min(1.0, 2 * sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n)


def per_case(rs):
    d = collections.defaultdict(list)
    for r in rs:
        if r["status"] in SCORED: d[r["case"]].append(r["status"] == "pass")
    return {c: sum(v) / len(v) for c, v in d.items() if v}


def ac(rs):
    s = [r for r in rs if r["status"] in SCORED]
    return sum(r["status"] == "pass" for r in s), len(s)


_chronic = {}
def chronic_apps(c):
    """Applications all of whose events in the 30 minutes before the incident are chronic."""
    if c not in _chronic:
        out = subprocess.run([a.bin, "--json", "before", labels[c]["incident"], "--window", "30m"], cwd=pathlib.Path(a.cases) / c, capture_output=True, text=True)
        ev = json.loads(out.stdout)["events"] if out.returncode in (0, 1) and out.stdout.strip() else []
        by = collections.defaultdict(list)
        for e in ev: by[e["app"]].append(bool(e.get("chronic")))
        _chronic[c] = {app for app, v in by.items() if all(v)}
    return _chronic[c]


ok = {}
print(f"Parent = {a.parent}, Candidate = {a.candidate}; suites {', '.join(suites)}\n")
for arm in ("mcp", "skill"):
    for ss in ([s] for s in suites):
        for lab in (a.parent, a.candidate):
            p, n = ac(sel(arm, lab, ss))
            print(f"  {arm}@{lab:<10} {ss[0]:<16} AC@1 {p}/{n} ({p / max(n, 1):.2f})")
print()

# A1
P, C = per_case(sel("mcp", a.parent, ["rcaeval-heldout"])), per_case(sel("mcp", a.candidate, ["rcaeval-heldout"]))
both = sorted(set(P) & set(C))
better, worse = sum(C[c] > P[c] for c in both), sum(C[c] < P[c] for c in both)
pp, cp = ac(sel("mcp", a.parent, ["rcaeval-heldout"])), ac(sel("mcp", a.candidate, ["rcaeval-heldout"]))
p = sign_p(better, worse)
ok["A1"] = cp[0] / max(cp[1], 1) > pp[0] / max(pp[1], 1) and better > worse and p < 0.05
print(f"A1 mcp held-out: Candidate {cp[0]}/{cp[1]} against Parent {pp[0]}/{pp[1]}; {len(both)} paired cases, better on {better}, worse on {worse}, sign test p = {p:.4f}: {'PASS' if ok['A1'] else 'FAIL'}")

# A2
pp, cp = ac(sel("skill", a.parent, suites)), ac(sel("skill", a.candidate, suites))
ok["A2"] = cp[0] / max(cp[1], 1) >= pp[0] / max(pp[1], 1)
P, C = per_case(sel("skill", a.parent, suites)), per_case(sel("skill", a.candidate, suites))
both = sorted(set(P) & set(C))
b2, w2 = sum(C[c] > P[c] for c in both), sum(C[c] < P[c] for c in both)
print(f"A2 skill held-out + misses: Candidate {cp[0]}/{cp[1]} ({cp[0] / max(cp[1], 1):.3f}) against Parent {pp[0]}/{pp[1]} ({pp[0] / max(pp[1], 1):.3f}); paired better {b2}, worse {w2}, p = {sign_p(b2, w2):.4f}: {'PASS' if ok['A2'] else 'FAIL'}")

# A3
def departures(label):
    rs = [r for r in sel("mcp", label, suites) + sel("skill", label, suites) if r["status"] in SCORED]
    dep = [r for r in rs if r["root"] != (ref[cid(r)]["chain"] or [""])[0]]
    return sum(r["status"] == "pass" for r in dep), len(dep)
dp, dc = departures(a.parent), departures(a.candidate)
ok["A3"] = dc[0] / max(dc[1], 1) >= dp[0] / max(dp[1], 1)
print(f"A3 departures from rank 1 naming the label: Candidate {dc[0]} of {dc[1]} ({dc[0] / max(dc[1], 1):.3f}) against Parent {dp[0]} of {dp[1]} ({dp[0] / max(dp[1], 1):.3f}): {'PASS' if ok['A3'] else 'FAIL'}")

# A4
mp = sum(r["status"] == "pass" for r in sel("mcp", a.parent, ["rcaeval-miss"]) + sel("skill", a.parent, ["rcaeval-miss"]))
mc = sum(r["status"] == "pass" for r in sel("mcp", a.candidate, ["rcaeval-miss"]) + sel("skill", a.candidate, ["rcaeval-miss"]))
ok["A4"] = mc >= mp
print(f"A4 misses recovered (runs naming the label, mcp + skill): Candidate {mc} against Parent {mp}: {'PASS' if ok['A4'] else 'FAIL'}")

print("\nReported, not gated")
for arm in ("mcp", "skill"):
    for lab in (a.parent, a.candidate):
        rs = sel(arm, lab, suites)
        wrong = [r for r in rs if r["status"] == "fail"]
        chron = sum(r["root"] in chronic_apps(cid(r)) for r in wrong)
        names = collections.Counter(r["root"] for r in wrong).most_common(4)
        print(f"  {arm}@{lab:<10} wrong {len(wrong)}, naming a chronic application {chron}; most named {names}; "
              f"tool calls median {statistics.median([r['tool_calls'] for r in rs]) if rs else 0}, cost ${sum(r.get('cost') or 0 for r in rs):.2f}, "
              f"voided/errors {sum(r['status'] not in SCORED for r in rs)} of {len(rs)}")
print("\nAGENT GATE:", "PASS" if all(ok.values()) else "FAIL", {k: "pass" if v else "fail" for k, v in ok.items()})
