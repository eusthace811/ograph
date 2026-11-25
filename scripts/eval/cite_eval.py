#!/usr/bin/env python3
"""Gate for `ograph cite-check`: it flags fabricated and stale citations and nothing real.

On a copy of the demo workspace:
1. The real agent answer docs/evidence/agent-run.txt: every citation it holds is real, so every
   one must come back present.
2. A planted answer: real citations (control) plus fabricated ones, each with the status the
   rule fixed before any run gives it:
   - unknown: incident ids and IRIs no harvest wrote, chain ids with random digests, and
     application near-twins (`-canary`, `-v2`, `legacy-`) of catalog names;
   - stale: a real chain cited at a rank it does not have, and the chains a narrower
     correlation window (window_s 1800 -> 120) stops producing while their events stay;
   - present: the chains at their own rank (`N. app: id:chain/x (rank N)`; the list number
     alone is no rank), the events `before` lists, the catalog names.
Flagged = stale or unknown. Precision = right flags / flags; recall = planted fabrications and
stale citations flagged / planted. Status accuracy counts stale and unknown apart.
The gate: no real citation flagged on the agent answer, precision 1.0 and recall 1.0 on the
planted one.

Usage: cite_eval.py --bin target/release/ograph --demo examples/demo --work target/cite-eval [--set first|fresh]
"""
import argparse, json, pathlib, random, shutil, subprocess

# "first" is the set the gate was written with. Its run missed `legacy-` twins, which led to
# detecting names that extend a catalog name; "fresh" was written after that change and before
# its first run, with other twin forms and applications, so the change is checked on data it
# was not shaped on.
TWIN_APPS = {"first": ["ledger-db", "payments-api", "checkout", "orders-api"],
             "fresh": ["inventory", "web-frontend", "auth-service", "notifications", "search-api"]}
TWIN_FORMS = {"first": ["{}-canary", "{}-v2", "legacy-{}"],
              "fresh": ["{}-staging", "{}-blue", "old-{}", "next-{}", "{}-replica"]}
SEEDS = {"first": 7, "fresh": 11}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True); ap.add_argument("--work", required=True)
    ap.add_argument("--answer", default="docs/evidence/agent-run.txt")
    ap.add_argument("--set", choices=["first", "fresh"], default="first")
    a = ap.parse_args()
    b = str(pathlib.Path(a.bin).resolve()); work = pathlib.Path(a.work).resolve()
    root = work / "demo"
    if root.exists(): shutil.rmtree(root)
    shutil.copytree(a.demo, root, ignore=shutil.ignore_patterns("store.redb", "store.lock", "harvested"))
    run = lambda *args, text=None: subprocess.run([b, "--root", str(root), *args], capture_output=True, text=True, input=text)
    def check(text):
        p = run("--json", "cite-check", "-", text=text)
        assert p.returncode in (0, 1), p.stderr
        return json.loads(p.stdout)
    assert run("harvest").returncode == 0

    real = check(pathlib.Path(a.answer).read_text())
    flagged_real = [c for c in real if c["status"] != "present"]
    print(f"## agent answer ({a.answer}): {len(real)} citations, {len(flagged_real)} flagged")
    for c in real: print(f"  {c['status']:<8} {c['kind']:<9} {c['cited']}  ({c['detail']})")

    incidents = [i["id"] for i in json.loads(run("--json", "sparql", "SELECT ?i WHERE { ?x inc:id ?i }").stdout)["rows"] for i in [{"id": i[0]}]]
    chains = {}
    for inc in incidents:
        out = run("--json", "chains", inc, "--top", "10")
        for k, r in enumerate(json.loads(out.stdout).get("apps", [])):
            if r.get("chain"): chains[r["chain"]["id"].replace("https://ograph.dev/id/", "id:")] = (k + 1, r["app"], inc)
    events = [e["event"] for e in json.loads(run("--json", "before", "INC-5377", "--window", "30m").stdout)["events"]]
    apps = [r[0] for r in json.loads(run("--json", "sparql", "SELECT ?n WHERE { ?a a ops:Application ; ops:name ?n }").stdout)["rows"]]

    rng = random.Random(SEEDS[a.set])
    hexid = lambda: "".join(rng.choice("0123456789abcdef") for _ in range(16))
    planted = []  # (line, cited, expected status)
    for c, (rank, app, inc) in chains.items():
        planted.append((f"{rank}. {app}: {c} (rank {rank})", c, "present"))
    for e in events[:6]: planted.append((f"See {e} in the timeline.", e, "present"))
    for n in apps: planted.append((f"The service {n} is involved.", n, "present"))
    for inc in incidents: planted.append((f"Compare with {inc}.", inc, "present"))
    top = next(c for c, v in chains.items() if v[0] == 1 and v[2] == "INC-5377")
    planted.append((f"Rank 3 is {top}.", top, "stale"))
    for fake in ["INC-9999", "INC-5378", "INC-1"]: planted.append((f"This matches {fake}.", fake, "unknown"))
    for fake in ["id:inc/inc-1234", "id:ev/alert-ledger-db-made-up-1795385980000", "id:app/ledger-service", "id:vendor/nobody"]:
        planted.append((f"Evidence: {fake}.", fake, "unknown"))
    for inc in ["inc-5377", "inc-5377", "inc-4120", "inc-9999"]:
        c = f"id:chain/{inc}-{hexid()}"; planted.append((f"The chain {c} explains it.", c, "unknown"))
    for n in TWIN_APPS[a.set]:
        for form in TWIN_FORMS[a.set]:
            twin = form.format(n)
            planted.append((f"The service {twin} failed first.", twin, "unknown"))

    config = root / ".ograph" / "config.toml"
    config.write_text(config.read_text().replace("window_s = 1800", "window_s = 120"))
    assert run("harvest").returncode == 0
    now = {}
    for inc in incidents:
        for k, r in enumerate(json.loads(run("--json", "chains", inc, "--top", "10").stdout).get("apps", [])):
            if r.get("chain"): now[r["chain"]["id"].replace("https://ograph.dev/id/", "id:")] = k + 1
    labels = []
    for line, cited, want in planted:
        if cited in chains and want == "present" and now.get(cited) != chains[cited][0]:
            want = "stale"
        labels.append((line, cited, want))
    got = {}
    for c in check("\n".join(l for l, _, _ in labels)):
        got.setdefault(c["cited"], c["status"])
    right_flags = sum(1 for _, c, w in labels if w != "present" and got.get(c, "missing") != "present" and c in got)
    flags = sum(1 for _, c, _ in labels if got.get(c) not in (None, "present"))
    should = sum(1 for _, _, w in labels if w != "present")
    exact = sum(1 for _, c, w in labels if got.get(c) == w)
    print(f"\n## planted answer (set {a.set}): {len(labels)} citations; {should} should be flagged "
          f"({sum(1 for *_, w in labels if w == 'stale')} stale, {sum(1 for *_, w in labels if w == 'unknown')} unknown), "
          f"{len(labels) - should} present")
    print(f"extracted {sum(1 for _, c, _ in labels if c in got)} of {len(labels)}; status exactly right for {exact}")
    for _, c, w in labels:
        if got.get(c) != w: print(f"  MISMATCH {c}: want {w}, got {got.get(c, 'not extracted')}")
    precision = right_flags / flags if flags else 1.0
    recall = right_flags / should if should else 1.0
    print(f"flags {flags}, right {right_flags}; precision {precision:.3f}  recall {recall:.3f}")
    ok = not flagged_real and precision == 1.0 and recall == 1.0
    print("CITE-CHECK GATE:", "PASS" if ok else "FAIL")

main()
