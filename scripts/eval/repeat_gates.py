#!/usr/bin/env python3
"""Runs the model gates N times with no model cache and reports variance and per-item errors.

Each run copies examples/demo without its store and model cache, harvests it, then runs
llm_gate.py, alias_eval.py and coverage_eval.py (the main questions and the held-out ones).
Model calls are counted as new lines in the copy's cache; latency is wall time.

Usage: repeat_gates.py --bin target/release/ograph --demo examples/demo --runs 3 [--out DIR]"""
import argparse, datetime, json, pathlib, re, shutil, subprocess, sys, tempfile, time

ap = argparse.ArgumentParser()
ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
ap.add_argument("--runs", type=int, default=3); ap.add_argument("--out")
ap.add_argument("--llm-toml", default="", help="TOML appended to the copy's [llm.extract] section (the last one)")
ap.add_argument("--skip", default="", help="comma list of gates to skip: threads,alias,coverage,heldout,heldoutb")
a = ap.parse_args()
binp = str(pathlib.Path(a.bin).resolve()); demo = pathlib.Path(a.demo).resolve()
ev = pathlib.Path(__file__).resolve().parent
skip = set(filter(None, a.skip.split(",")))
out_dir = pathlib.Path(a.out) if a.out else None
if out_dir: out_dir.mkdir(parents=True, exist_ok=True)
labels = {k.split(".")[0]: v for k, v in json.loads((demo / "threads-labels.json").read_text()).items()}

def lines(p): return len(p.read_text().splitlines()) if p.exists() else 0
def run(*cmd):
    t = time.time(); p = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    return p.returncode, p.stdout + p.stderr, time.time() - t

def coverage(d, cache, q, l):
    if not (demo / q).exists(): return None
    before = lines(cache)
    rc, out, dt = run(sys.executable, ev / "coverage_eval.py", "--bin", binp, "--demo", d, "--questions", q, "--labels", l)
    m = re.search(r"reported covered (\d+)/(\d+); gap questions with the expected gap (\d+)/(\d+)", out)
    if not m:
        return {"covered": "?", "gaps": "?", "errors": [out.strip()[-300:]], "calls": lines(cache) - before, "s": round(dt, 1), "pass": False}
    wrong = [w.strip() for w in out.splitlines() if w.startswith("   Q")]
    return {"covered": f"{m[1]}/{m[2]}", "gaps": f"{m[3]}/{m[4]}", "errors": wrong, "calls": lines(cache) - before, "s": round(dt, 1), "pass": rc == 0}

for i in range(1, a.runs + 1):
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="ograph-repeat-")); d = tmp / "demo"
    shutil.copytree(demo, d, ignore=shutil.ignore_patterns("store.redb", "cache"))
    cache = d / ".ograph" / "cache" / "llm.jsonl"
    if a.llm_toml:
        cfg = d / ".ograph" / "config.toml"; cfg.write_text(cfg.read_text().rstrip("\n") + "\n" + a.llm_toml + "\n")
    r = {"run": i}
    rc, out, dt = run(binp, "--root", d, "harvest")
    r["harvest"] = {"calls": lines(cache), "s": round(dt, 1), "rc": rc}
    if "threads" not in skip:
        rc, out, _ = run(binp, "--root", d, "--json", "sparql", "SELECT ?at ?c WHERE { ?t a disc:Thread ; disc:at ?at ; disc:category ?c }")
        got = {str(int(datetime.datetime.fromisoformat(at.replace("Z", "+00:00")).timestamp())): c for at, c in json.loads(out)["rows"]}
        flips = {s: f"{labels[s]}->{got.get(s)}" for s in labels if got.get(s) != labels[s]}
        rc, out, _ = run(sys.executable, ev / "llm_gate.py", "--bin", binp, "--demo", d)
        r["threads"] = {"right": len(labels) - len(flips), "flips": flips, "gate": "PASS" if rc == 0 else "FAIL",
                        "broken_model": [l.strip() for l in out.splitlines() if l.strip().startswith(("committed:", "failure modes", "audit:"))]}
    if "alias" not in skip:
        rc, out, _ = run(sys.executable, ev / "alias_eval.py", "--bin", binp, "--demo", d)
        r["alias"] = {"line": next((l.strip() for l in out.splitlines() if l.startswith("1.")), out[-200:]), "gate": "PASS" if rc == 0 else "FAIL"}
    if "coverage" not in skip:
        r["coverage"] = coverage(d, cache, "questions.txt", "questions-labels.json")
    if "heldout" not in skip:
        r["heldout"] = coverage(d, cache, "questions-heldout.txt", "questions-heldout-labels.json")
    if "heldoutb" not in skip:
        r["heldoutb"] = coverage(d, cache, "questions-heldout-b.txt", "questions-heldout-b-labels.json")
    if out_dir:
        shutil.copy(cache, out_dir / f"cache-run{i}.jsonl")
    print(json.dumps(r), flush=True)
    shutil.rmtree(tmp)
