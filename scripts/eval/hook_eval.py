#!/usr/bin/env python3
"""Gate for the session-start hook (`./x install-hook`), run as the hook runs it: the command
from scripts/agent/install_hook.py through `sh -c`, in the workspace directory.

States, on a copy of the demo and on a copy of one RCAEval case (the larger store):
- no .ograph/ anywhere above: silent;
- fresh harvest, incidents with unjudged chains: one line naming them;
- every incident judged: silent;
- a source file edited after the harvest: one line naming it;
- harvested again: silent.
Timing: 20 runs per state, wall clock including process start. The gate: each state prints
what it should, and the slowest run is under 200 ms.

Usage: hook_eval.py --bin target/release/ograph --demo examples/demo --case DATA/rcaeval-cases/c-... --work target/hook-eval
"""
import argparse, json, os, pathlib, re, shutil, statistics, subprocess, time

here = pathlib.Path(__file__).resolve().parents[2]
COMMAND = re.search(r'COMMAND = "(.*)"', (here / "scripts/agent/install_hook.py").read_text()).group(1)

def main():
    ap = argparse.ArgumentParser()
    for k in ("bin", "demo", "case", "work"): ap.add_argument(f"--{k}", required=True)
    a = ap.parse_args()
    b = pathlib.Path(a.bin).resolve(); work = pathlib.Path(a.work).resolve()
    if work.exists(): shutil.rmtree(work)
    env = dict(os.environ, PATH=f"{b.parent}:{os.environ['PATH']}")
    hook = lambda cwd: subprocess.run(["sh", "-c", COMMAND], cwd=cwd, env=env, capture_output=True, text=True)
    og = lambda root, *x: subprocess.run([str(b), "--root", str(root), *x], capture_output=True, text=True, env=env)
    def timed(cwd, n=20):
        ms, out = [], None
        for _ in range(n):
            t = time.perf_counter(); p = hook(cwd); ms.append((time.perf_counter() - t) * 1000)
            assert p.returncode == 0, p.stderr
            out = p.stdout
        return out, statistics.median(ms), max(ms)
    ok, slowest = True, 0.0
    def state(label, cwd, want):
        nonlocal ok, slowest
        out, med, mx = timed(cwd)
        good = (out == "") if want is None else bool(re.fullmatch(want, out.strip())) and out.count("\n") == 1
        ok &= good; slowest = max(slowest, mx)
        print(f"  {label:<42} median {med:6.1f} ms  max {mx:6.1f} ms  {'ok' if good else 'WRONG'}  {out.strip() or '(silent)'}")

    empty = work / "no-workspace"; empty.mkdir(parents=True)
    print("## no .ograph/")
    state("no workspace", empty, None)
    for label, src in (("demo", a.demo), ("rcaeval case", a.case)):
        root = work / label.replace(" ", "-")
        shutil.copytree(src, root, ignore=shutil.ignore_patterns("store.redb", "store.lock", "harvested"))
        t = time.perf_counter(); assert og(root, "harvest").returncode == 0
        print(f"## {label} (harvest {time.perf_counter() - t:.1f} s, store {(root / '.ograph/store.redb').stat().st_size / 1e6:.1f} MB)")
        incidents = json.loads(og(root, "--json", "sparql", "SELECT DISTINCT ?i WHERE { ?c corr:rank ?r ; corr:leadsTo ?x . ?x inc:id ?i }").stdout)["rows"]
        state("harvested, chains unjudged", root, rf"ograph: {len(incidents)} incidents have chains but no verdict \(.*\)\.")
        for (i,) in incidents:
            assert og(root, "verdict", f"id:chain/{i.lower()}-1", "reject", "--evidence", "hook-eval", "--by", "hook-eval").returncode == 0
        state("every incident judged", root, None)
        config = (root / ".ograph" / "config.toml").read_text()
        src_file = next(p for p in sorted(root.iterdir()) if p.suffix in (".json", ".jsonl") and p.name != "incidents.json" and f'path = "{p.name}"' in config)
        time.sleep(0.05); src_file.write_text(src_file.read_text())
        state(f"{src_file.name} edited after the harvest", root, rf"ograph: the graph is stale: 1 source files changed since the harvest at .* \({src_file.name}\); run `ograph harvest`\.")
        assert og(root, "harvest").returncode == 0
        state("harvested again", root, None)
    print(f"\nslowest run {slowest:.1f} ms (gate: under 200 ms)")
    print("HOOK GATE:", "PASS" if ok and slowest < 200 else "FAIL")

main()
