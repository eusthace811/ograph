#!/usr/bin/env python3
"""Gate for incident field conflicts.

1. Fixture: INC-1 on checkout opened at T with SEV1 in incidents/a.json; incidents/b.json, a
   later export, says T+40m and SEV3. The cause is an alert on ledger-db 10 minutes before T; a
   decoy alert on payments-api falls inside the window of the later time only. Under the default
   policy ("last") `ograph why INC-1` must list both open times and both severities with their
   records, and the top chain is the decoy. Under `at = "earliest", severity = "max"` the top
   chain is ledger-db.
2. Zero conflict entries ("disagrees with") on the demo and on every RCAEval case store.

Usage: incident_conflicts_eval.py --bin target/release/ograph --demo examples/demo
       --cases target/rcaeval-cases --work target/incident-conflicts
"""
import argparse, json, os, pathlib, shutil, subprocess, sys

T = 1795386400

def og(binp, root, *args, env=None):
    p = subprocess.run([binp, "--root", str(root), *args], capture_output=True, text=True, env=env)
    return p.returncode, p.stdout, p.stderr

def conflicts(binp, root):
    rc, out, err = og(binp, root, "--json", "sparql", 'SELECT ?f ?r WHERE { GRAPH ?g { ?q a harv:Quarantined ; harv:fact ?f ; harv:reason ?r } FILTER(CONTAINS(?r, "disagrees with")) }')
    if rc not in (0, 1): raise SystemExit(err)
    return json.loads(out)["rows"]

def fake_ogrep(work):
    line = lambda i, to: json.dumps({"data": {"edges": [{"evidence": {"file": "c.yaml", "line": 1}, "from": i, "provenance": "declared", "relation": "depends_on", "to": to}] if to else [], "entity": {"id": i, "kind": "service", "locations": [{"file": "c.yaml", "line": 1}]}}})
    body = "\n".join([line("checkout", "payments-api"), line("checkout", "ledger-db"), line("payments-api", ""), line("ledger-db", "")])
    p = work / "fake-ogrep"
    p.write_text(f"#!/bin/sh\ncat <<'X'\n{body}\nX\n"); p.chmod(0o755)
    return p

def fixture(binp, work, name, merge, env):
    root = work / name
    shutil.rmtree(root, ignore_errors=True); (root / "incidents").mkdir(parents=True)
    og(binp, root, "init")
    alert = lambda app, n, at: {"labels": {"alertname": n, "service": app, "severity": "critical"}, "startsAt": at}
    (root / "alerts.json").write_text(json.dumps([alert("ledger-db", "PoolSaturated", T - 600), alert("payments-api", "ErrorRate", T + 1500)]))
    (root / "incidents/a.json").write_text(json.dumps([{"id": "INC-1", "title": "Checkout down", "service": "checkout", "opened": T, "severity": "SEV1", "status": "open"}]))
    (root / "incidents/b.json").write_text(json.dumps([{"id": "INC-1", "service": "checkout", "opened": T + 2400, "severity": "SEV3", "status": "open"}]))
    cfg = "".join(f'[[source]]\nkind = "{k}"\npath = "{p}"\n' for k, p in [("catalog", "."), ("alerts", "alerts.json"), ("incidents", "incidents")])
    (root / ".ograph/config.toml").write_text(cfg + "[correlation]\nwindow_s = 1800\n" + merge)
    rc, out, err = og(binp, root, "harvest", env=env)
    assert rc == 0, err
    _, why, _ = og(binp, root, "why", "INC-1")
    _, ch, _ = og(binp, root, "--json", "chains", "INC-1", "--top", "1")
    apps = [a["app"] for a in json.loads(ch)["apps"]]
    return [l for l in why.splitlines() if "evt:at" in l or "evt:severity" in l], apps[:1]

def main():
    ap = argparse.ArgumentParser()
    for k in ("bin", "demo", "cases", "work"): ap.add_argument("--" + k, required=True)
    a = ap.parse_args(); binp = str(pathlib.Path(a.bin).resolve())
    work = pathlib.Path(a.work).resolve(); work.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "OGRAPH_OGREP": str(fake_ogrep(work))}
    ok = True
    results = {}
    for name, merge in [("last", ""), ("earliest", '[incidents]\nmerge = { at = "earliest", severity = "max" }\n')]:
        why, top = fixture(binp, work, name, merge, env)
        results[name] = top
        print(f"1. policy {name}: top chain {top}; ograph why INC-1:")
        for l in why: print("   " + l)
        listed = sum("disagrees with" in l for l in why) == 2 and sum("QUARANTINED" not in l for l in why) == 2
        ok &= listed
    print(f"   both values listed with their records under each policy: {ok}")
    recovered = results["earliest"] == ["ledger-db"] and results["last"] != ["ledger-db"]
    print(f"   true cause ledger-db: last -> {results['last']}, earliest -> {results['earliest']}; recovered by earliest only: {recovered}")
    demo = work / "demo"
    shutil.rmtree(demo, ignore_errors=True)
    shutil.copytree(a.demo, demo, ignore=shutil.ignore_patterns("store.redb", "store.lock"))
    rc, _, err = og(binp, demo, "harvest", "--only", "incidents")
    assert rc == 0, err
    demo_n = len(conflicts(binp, demo))
    cases = sorted(p for p in pathlib.Path(a.cases).iterdir() if (p / ".ograph" / "store.redb").exists())
    case_n = sum(len(conflicts(binp, c)) for c in cases)
    print(f"2. conflict entries: demo {demo_n}; RCAEval {case_n} over {len(cases)} case stores")
    ok = ok and recovered and demo_n == 0 and case_n == 0 and len(cases) == 90
    print("INCIDENT CONFLICT GATE:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

main()
