#!/usr/bin/env python3
"""Do rejections on similar past incidents improve root-cause ranking? The experiment that
decided against `ruled_out_weight` and against a "ruled out" mark in `ograph chains`. Both
were removed after it, so this script needs ograph built from commit 5f3ea74, titled
"Experiment: past verdicts as a ranking input in chains"; with any other build it says so and
exits 0. docs/evidence/ruled-out.txt is the record of the run.

All 90 RCAEval cases in one store. Wherever the rank-1 chain is wrong it is rejected. Each
incident is then ranked with `ruled_out_weight` from a grid, seeing earlier incidents only (the
way production does) and then leave-one-out. The weight is chosen on rep 1 and reported on
reps 2-3. The mark is scored by where it lands: on a root that is not the true one (right) or
on the true root (misleading), per similarity floor.

Keep rule, set before the run: as for precedent_eval.py, held-out AC@1 on later incidents
must beat the weight-0 baseline with no case made wrong.

Usage: ruled_out_eval.py --bin target/release/ograph --cases CASES --out WORKDIR [--jobs 3]
"""
import argparse, subprocess, sys, tempfile
from precedent_eval import Bench, store

def has_setting(binary, name):
    with tempfile.TemporaryDirectory() as root:
        subprocess.run([binary, "--root", root, "init"], capture_output=True)
        p = subprocess.run([binary, "--root", root, "replay", "--set", f"{name}=0"], capture_output=True, text=True)
        return f"no setting {name}" not in p.stderr

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True); ap.add_argument("--out", required=True)
    ap.add_argument("--jobs", type=int, default=3)
    a = ap.parse_args()
    if not has_setting(a.bin, "ruled_out_weight"):
        print("ruled_out_eval.py needs ograph built from commit 5f3ea74 (\"Experiment: past verdicts as a ranking input in chains\"),\n"
              f"which has the ruled_out_weight setting; {a.bin} does not. The result of that run is docs/evidence/ruled-out.txt.")
        sys.exit(0)
    labels, og = store(a, "rejections")
    b = Bench(labels, og, a.jobs)
    base = b.ranked({})
    print(f"no verdicts: AC@1 all {b.ac1(base, b.cids):.3f}, rep 1 {b.ac1(base, b.tune):.3f}, reps 2-3 {b.ac1(base, b.held):.3f}")
    wrong = {c: base[c][0]["chain"]["id"] for c in b.cids if base[c] and not b.hit(base, c)}
    for c, chain in wrong.items():
        og("verdict", chain, "reject", "--evidence", f"RCAEval label: the root is {labels[c]['root']}", "--by", "eval")
    print(f"rejected the wrong rank-1 chain on {len(wrong)} incidents")
    for name, any_time in [("earlier incidents only", "false"), ("leave-one-out (all other incidents)", "true")]:
        runs, _ = b.grid(f"ruled_out_weight, {name}: AC@1 per weight", "ruled_out_weight", {"past_any_time": any_time}, base, [])
        print("  'ruled out' marks at weight 0, per similarity floor (on the top 5 roots; on the rank-1 root):")
        for split, of in [("rep 1", b.tune), ("reps 2-3", b.held)]:
            for floor in [0.0, 0.7, 0.8, 0.85, 0.9, 0.95]:
                marked = [(c, k, r) for c in of for k, r in enumerate(runs[0.0][c]) if r.get("ruled_out", {}).get("similarity", -1) >= floor]
                true = lambda rows: sum(labels[c]["root"] == r["app"] for c, _, r in rows)
                first = [m for m in marked if m[1] == 0]
                print(f"    {split:<8} floor {floor:<4g}: {len(marked):>2} marks, {true(marked)} on the true root; rank 1: {len(first)} marks, {true(first)} on the true root")

main()
