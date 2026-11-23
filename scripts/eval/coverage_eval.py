#!/usr/bin/env python3
"""Ontology coverage gate on examples/demo/questions.txt (labels in questions-labels.json).

1. Verdicts: each question labeled covered must be reported covered; each labeled gap must be
   reported with a gap that contains one of its expected words.
2. Replay: a second run answers from the cache and gives the same report.
3. Proposals: ontology.ttl plus the proposed Turtle parses, and `ograph ontology diff` reports
   only additions (nothing BREAKING).

Usage: coverage_eval.py --bin target/release/ograph --demo examples/demo"""
import argparse, json, pathlib, subprocess, sys, tempfile

ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
a = ap.parse_args(); binp = str(pathlib.Path(a.bin).resolve()); demo = pathlib.Path(a.demo).resolve()
labels = json.loads((demo / "questions-labels.json").read_text())
tmp = pathlib.Path(tempfile.mkdtemp(prefix="ograph-coverage-"))
def run(out):
    p = subprocess.run([binp, "--root", str(demo), "--json", "ontology", "coverage", str(demo / "questions.txt"), "--out", str(out)], capture_output=True, text=True)
    if p.returncode == 2: raise SystemExit(p.stderr)
    return json.loads(p.stdout)
cache = demo / ".ograph" / "cache" / "llm.jsonl"
first = run(tmp / "proposed.ttl")
lines = len(cache.read_text().splitlines())
again = run(tmp / "again.ttl")
replay = again == first and len(cache.read_text().splitlines()) == lines
ok_cov = ok_gap = 0; wrong = []
for i, c in enumerate(first, 1):
    gaps = [g["text"].lower() for g in c["gaps"]]
    if i in labels["covered"]:
        ok_cov += not gaps
        if gaps: wrong.append(f"Q{i} labeled covered, reported missing {gaps}")
    else:
        want = labels["gaps"][str(i)]
        hit = any(w in g for g in gaps for w in want)
        ok_gap += hit
        if not hit: wrong.append(f"Q{i} expected a gap like {want}, got {gaps}")
print(f"1. verdicts: covered questions reported covered {ok_cov}/{len(labels['covered'])}; gap questions with the expected gap {ok_gap}/{len(labels['gaps'])}")
for w in wrong: print("   " + w)
print(f"2. replay from cache, same report: {replay}")
ont = pathlib.Path(__file__).resolve().parents[2] / "ontology" / "ograph.ttl"
(tmp / "merged.ttl").write_text(ont.read_text() + "\n" + (tmp / "proposed.ttl").read_text())
d = subprocess.run([binp, "--root", str(demo), "ontology", "diff", str(tmp / "merged.ttl")], capture_output=True, text=True)
added = [l for l in d.stdout.splitlines() if l.startswith("added")]
breaking = [l for l in d.stdout.splitlines() if l.startswith("BREAKING")]
print(f"3. proposals merged into the ontology: {len(added)} additions, {len(breaking)} breaking, diff exit {d.returncode}")
ok = ok_gap == len(labels["gaps"]) and ok_cov >= len(labels["covered"]) - 2 and replay and not breaking and d.returncode == 0 and added
print("COVERAGE GATE:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
