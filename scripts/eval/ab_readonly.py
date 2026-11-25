#!/usr/bin/env python3
"""ab_compare.sh without writing to the eval data: the RCAEval cases are mirrored under --work
with their sources linked, and the demo is harvested from a copy.

Compares an old and a new binary on the demo's harvest and eight queries (byte for byte), the
per-case RCAEval rankings of rcaeval_score.py, and the output of similar_eval.py.

Usage: ab_readonly.py OLD_BIN NEW_BIN --cases data/rcaeval-cases --demo target/demo --work target/ab
"""
import argparse, json, pathlib, re, shutil, subprocess, sys

HERE = pathlib.Path(__file__).resolve().parent
QUERIES = ["before INC-5377", "chains INC-5377", "chains INC-5377 --rank propagation", "similar INC-5377", "blast ledger-db", "news", "validate", "why INC-5377"]

def mirror(cases, root):
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    (root / "labels.json").symlink_to(cases / "labels.json")
    for c in sorted(cases.iterdir()):
        if (c / ".ograph" / "config.toml").exists():
            (root / c.name / ".ograph").mkdir(parents=True)
            shutil.copy(c / ".ograph" / "config.toml", root / c.name / ".ograph" / "config.toml")
            for f in c.iterdir():
                if f.name != ".ograph":
                    (root / c.name / f.name).symlink_to(f)

def demo(binary, src, d):
    if d.exists():
        shutil.rmtree(d)
    shutil.copytree(src, d, symlinks=True)
    (d / ".ograph" / "store.redb").unlink(missing_ok=True)
    out = {"harvest": re.sub(r" \([0-9.]+ s\)|in [0-9.]+ s", "", subprocess.run([binary, "harvest"], cwd=d, capture_output=True, text=True).stdout)}
    for q in QUERIES:
        out[q] = re.sub(r'"harvested_at":"[^"]*"', "", subprocess.run([binary, *q.split(), "--json"], cwd=d, capture_output=True, text=True).stdout)
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("old"); ap.add_argument("new")
    ap.add_argument("--cases", type=pathlib.Path, required=True)
    ap.add_argument("--demo", type=pathlib.Path, required=True)
    ap.add_argument("--work", type=pathlib.Path, required=True)
    a = ap.parse_args()
    bins = {"old": str(pathlib.Path(a.old).resolve()), "new": str(pathlib.Path(a.new).resolve())}
    res = {}
    for k, b in bins.items():
        mirror(a.cases.resolve(), a.work / f"cases-{k}")
        subprocess.run([sys.executable, str(HERE / "rcaeval_score.py"), "--bin", b, "--cases", str(a.work / f"cases-{k}"), "--jobs", "3", "--out", str(a.work / f"rcaeval-{k}.json")], check=True, capture_output=True)
        rank = {c: {m: r for m, r in v.items() if m != "harvest_s"} for c, v in json.loads((a.work / f"rcaeval-{k}.json").read_text())["results"].items()}
        sim = subprocess.run([sys.executable, str(HERE / "similar_eval.py"), "--bin", b, "--cases", str(a.cases.resolve()), "--out", str(a.work / f"similar-{k}")], capture_output=True, text=True).stdout
        res[k] = {"rankings": rank, "similar": [l for l in sim.splitlines() if "merged harvest" not in l], **demo(b, a.demo, a.work / f"demo-{k}")}
    same = 0
    for key in res["old"]:
        ok = res["old"][key] == res["new"][key]
        same += ok
        print(("identical  " if ok else "DIFFERENT  ") + key)
    print(f"{same} of {len(res['old'])} outputs identical")
    print("\n".join(l for l in res["new"]["similar"] if "top-1" in l))
    score = subprocess.run([sys.executable, str(HERE / "rcaeval_score.py"), "--bin", bins["new"], "--cases", str(a.work / "cases-new"), "--reuse"], capture_output=True, text=True).stdout
    print("new binary, RCAEval AC@1 AC@3 Avg@5:", next(l for l in score.splitlines() if l.startswith("| chain")))
    sys.exit(0 if same == len(res["old"]) else 1)


if __name__ == "__main__":
    main()
