#!/usr/bin/env python3
"""Do declined gaps stay declined across uncached model runs, without hiding other gaps?

Needs the model through pi: 2 x RUNS uncached `ontology coverage` runs on the demo's questions.
Baseline: RUNS runs with no declined.ttl. Treatment: RUNS runs with a declined.ttl that declines
three demo gaps (DECLINED below). The cache is deleted before every run.

Pass, fixed before the first run:
1. In no treatment run does proposed.ttl propose a term for a declined question whose words
   contain the declined term's words or are contained in them, and every time the model finds
   that gap the report says who declined it.
2. Every other question is reported with an undeclined gap in as many treatment runs as baseline
   runs, give or take one, and over all of them the treatment finds at least one fewer at most.
3. Every treatment run exits 1, since undeclined gaps remain.
A run in which any question got no model answer fails the gate: it measured nothing.

--cached keeps the demo's cache and its own ontology.ttl, the one the cache was made with, so
each run replays one recorded set of answers without a model call. It checks the matching on
real model output, not the variation between runs.

Usage: coverage_decline_eval.py --bin target/release/ograph --demo target/demo --work target/decline-eval [--runs 3] [--cached]
"""
import argparse, json, pathlib, re, shutil, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
DECLINED = {
    "ops:KubernetesCluster": ("Which Kubernetes cluster runs payments-api?", "We model services, not the clusters under them."),
    "ops:Revenue": ("How much revenue was lost during INC-4120?", "Finance tracks revenue; it is not an operations fact."),
    "ops:Customers": ("Which customers were affected by INC-5377?", "Customer data stays out of the graph."),
}

def declined_ttl():
    out = "@prefix og: <https://ograph.dev/meta#> .\n@prefix ops: <https://ograph.dev/ops#> .\n\n"
    for term, (q, why) in DECLINED.items():
        out += f'{term} og:declined {json.dumps(why)} ;\n  og:by "eval" ;\n  og:question {json.dumps(q)} .\n\n'
    return out

def words(text):
    return {w.lower() for w in re.findall(r"[A-Z]?[a-z0-9]+|[A-Z]+(?![a-z])", text)}

def run(binary, ws, questions, cached):
    if not cached:
        (ws / ".ograph" / "cache" / "llm.jsonl").unlink(missing_ok=True)
    out = ws / "proposed.ttl"
    p = subprocess.run([binary, "--root", str(ws), "--json", "ontology", "coverage", str(questions), "--out", str(out)], capture_output=True, text=True)
    if p.returncode == 2:
        sys.exit(p.stderr)
    return p.returncode, json.loads(p.stdout), out.read_text()

def proposed_for(ttl, question):
    return [m.group(1) for m in re.finditer(r"^ops:(\w+) a owl:\w+ ;\n  rdfs:comment (\".*\") \.", ttl, re.M) if question in json.loads(m.group(2))]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--demo", type=pathlib.Path, required=True)
    ap.add_argument("--work", type=pathlib.Path, required=True)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--cached", action="store_true")
    args = ap.parse_args()
    binary = str(pathlib.Path(args.bin).resolve())
    ws = args.work / "demo"
    if ws.exists():
        shutil.rmtree(ws)
    (ws / ".ograph").mkdir(parents=True)
    shutil.copytree(args.demo / ".ograph" / "cache", ws / ".ograph" / "cache")
    shutil.copy(args.demo / ".ograph" / "config.toml", ws / ".ograph" / "config.toml")
    shutil.copy(args.demo / ".ograph" / "ontology.ttl" if args.cached else ROOT / "ontology" / "ograph.ttl", ws / ".ograph" / "ontology.ttl")
    shutil.copy(ROOT / "ontology" / "shapes.ttl", ws / ".ograph" / "shapes.ttl")
    for f in args.demo.iterdir():
        if f.name != ".ograph":
            (ws / f.name).symlink_to(f.resolve())
    subprocess.run([binary, "--root", str(ws), "harvest"], check=True, capture_output=True)
    questions = ROOT / "examples" / "demo" / "questions.txt"
    declined_q = {q for q, _ in DECLINED.values()}

    found = {"baseline": [], "treatment": []}
    ok1, ok3 = True, True
    for label in ("baseline", "treatment"):
        (ws / ".ograph" / "declined.ttl").unlink(missing_ok=True)
        if label == "treatment":
            (ws / ".ograph" / "declined.ttl").write_text(declined_ttl())
        for k in range(args.runs):
            rc, report, ttl = run(binary, ws, questions, args.cached)
            failed = [c["question"] for c in report if c.get("failed")]
            if failed:
                print(f"{label} run {k + 1}: {len(failed)} of {len(report)} questions got no answer: {next(c['failed'] for c in report if c.get('failed'))}")
                print("COVERAGE DECLINE GATE: NOT MEASURED")
                sys.exit(2)
            gap_qs = {c["question"] for c in report if c["gaps"]}
            found[label].append(gap_qs)
            print(f"{label} run {k + 1}: exit {rc}, {len(gap_qs)} questions with gaps, {sum(len(c['declined']) for c in report)} declined gaps")
            if label == "baseline":
                for q in sorted(declined_q):
                    print(f"   baseline proposes for {q!r}: {proposed_for(ttl, q)}")
                continue
            ok3 &= rc == 1
            for term, (q, _) in DECLINED.items():
                local = words(term.split(":", 1)[1])
                again = [t for t in proposed_for(ttl, q) if words(t) <= local or local <= words(t)]
                c = next(c for c in report if c["question"] == q)
                said = [f"{d['need']['text']} by {d['by']}" for d in c["declined"]]
                print(f"   {term}: reappeared in proposed.ttl {again or 'no'}; reported declined: {said or 'model did not raise it'}; other proposals {proposed_for(ttl, q)}")
                ok1 &= not again and all(d["by"] == "eval" for d in c["declined"])

    others = sorted({c for runs in found.values() for r in runs for c in r} - declined_q)
    print("\n| question | baseline runs with a gap | treatment runs with a gap |\n|---|---|---|")
    ok2 = True
    for q in others:
        b = sum(q in r for r in found["baseline"])
        t = sum(q in r for r in found["treatment"])
        ok2 &= abs(b - t) <= 1
        print(f"| {q} | {b} | {t} |")
    tb = sum(len(r - declined_q) for r in found["baseline"])
    tt = sum(len(r - declined_q) for r in found["treatment"])
    ok2 &= tt >= tb - 1
    print(f"\nother questions found with a gap, summed over runs: baseline {tb}, treatment {tt}")
    print(f"1. declined gaps never proposed again and reported as declined: {ok1}")
    print(f"2. other gaps found as often as before: {ok2}")
    print(f"3. every treatment run exits 1: {ok3}")
    ok = ok1 and ok2 and ok3
    print("COVERAGE DECLINE GATE:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
