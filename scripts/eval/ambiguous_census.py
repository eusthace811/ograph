#!/usr/bin/env python3
"""Census of ambiguous application-name quarantines, the measurement that decides whether
context disambiguation (break a near-tie between two catalog applications with facts from the
same record) is worth building.

The gateway quarantines a catalog name as ambiguous for three reasons: a tie (two applications
within 0.03 of each other at 0.93 or above), a sibling (`inventory-service-uk` against
`inventory-service-eu`), or a score from 0.80 to 0.93. Context can only settle a tie: a sibling
names an application the catalog does not have, and a low score is a typo guard, not a choice
between two. Each quarantined name is classified by `ograph resolve`'s rationale.

Corpora: the demo (regenerated, harvested with the model command disabled so only cached answers
are used), copies of every existing demo store that holds model answers, the 90 RCAEval cases
(copied, harvested), and the gateway gate's printed reasons. Also prints, per catalog, the
closest pair of distinct applications by Jaro-Winkler: a tie at 0.93 or above needs a mention
within 0.07 of both, so two names far apart cannot tie.

Usage: ambiguous_census.py --bin target/release/ograph --demo examples/demo
       --cases data/rcaeval-cases --stores DIR... --gateway docs/evidence/gateway.txt
       --work target/ambiguous-census [--jobs 4]
"""
import argparse, collections, concurrent.futures as cf, json, pathlib, re, shutil, subprocess

AMBIGUOUS = re.compile(r'^"(.*)" (is close to .* but names a sibling or ties another entity|only fuzzily matches .*); not created$')

def ograph(b, root, *args):
    return subprocess.run([b, "--root", str(root), *args], capture_output=True, text=True)

def reasons(b, root):
    p = ograph(b, root, "--json", "sparql", "SELECT ?r WHERE { GRAPH ?g { ?q harv:reason ?r } }")
    return [r[0] for r in json.loads(p.stdout).get("rows", [])] if p.returncode == 0 else []

def classify(b, root, name):
    p = ograph(b, root, "--json", "resolve", name)
    why = json.loads(p.stdout)["results"][0]["rationale"] if p.stdout.strip() else ""
    return "tie" if "are within" in why else "sibling" if "sibling" in why else "low score" if "only fuzzily" in why else f"other: {why}"

def tally(b, root, rs):
    out = collections.Counter()
    names = []
    for r in rs:
        m = AMBIGUOUS.match(r)
        if m:
            out[classify(b, root, m.group(1))] += 1
            names.append(m.group(1))
    return out, names

def closest_pair(b, root):
    p = ograph(b, root, "--json", "sparql", "SELECT ?a ?n WHERE { ?a a ops:Application ; ops:name ?n }")
    rows = json.loads(p.stdout).get("rows", []) if p.returncode == 0 else []
    names = sorted({re.sub(r"[^0-9a-z]", "", n.lower()): a for a, n in rows}.items())
    best = (0.0, "", "")
    for i, (x, ax) in enumerate(names):
        for y, ay in names[i + 1:]:
            if ax != ay:
                best = max(best, (jaro_winkler(x, y), x, y))
    return len(names), best

def jaro_winkler(a, b):
    if a == b: return 1.0
    la, lb = len(a), len(b)
    if not la or not lb: return 0.0
    w = max(max(la, lb) // 2 - 1, 0)
    ma, mb = [False] * la, [False] * lb
    m = 0
    for i, c in enumerate(a):
        for j in range(max(0, i - w), min(lb, i + w + 1)):
            if not mb[j] and b[j] == c:
                ma[i] = mb[j] = True; m += 1; break
    if not m: return 0.0
    t, k = 0, 0
    for i in range(la):
        if ma[i]:
            while not mb[k]: k += 1
            t += a[i] != b[k]; k += 1
    j = (m / la + m / lb + (m - t / 2) / m) / 3
    p = 0
    while p < min(4, la, lb) and a[p] == b[p]: p += 1
    return j + p * 0.1 * (1 - j)

def harvest_case(b, case, work):
    d = work / case.name
    if d.exists(): shutil.rmtree(d)
    shutil.copytree(case, d, ignore=shutil.ignore_patterns("store.redb", "store.lock"))
    ograph(b, d, "harvest")
    return d, reasons(b, d)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
    ap.add_argument("--cases", required=True); ap.add_argument("--stores", nargs="*", default=[])
    ap.add_argument("--gateway", required=True); ap.add_argument("--work", required=True)
    ap.add_argument("--jobs", type=int, default=4)
    a = ap.parse_args()
    b = str(pathlib.Path(a.bin).resolve()); work = pathlib.Path(a.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    total = collections.Counter()

    def report(label, root, rs):
        t, names = tally(b, root, rs)
        total.update(t)
        print(f"{label}: {len(rs)} quarantined facts, {sum(t.values())} facts quarantined for an ambiguous application name {dict(t)}{' ' + str(sorted(set(names))) if names else ''}")

    demo = work / "demo"
    if demo.exists(): shutil.rmtree(demo)
    shutil.copytree(pathlib.Path(a.demo), demo, ignore=shutil.ignore_patterns("store.redb", "store.lock"))
    subprocess.run(["python3", "generate.py"], cwd=demo, capture_output=True, check=True)
    cfg = demo / ".ograph" / "config.toml"
    cfg.write_text(re.sub(r"(?m)^command = .*$", 'command = ["false"]', cfg.read_text()))
    ograph(b, demo, "harvest")
    rs = reasons(b, demo)
    report(f"demo (deterministic, {sum('model call failed' in r for r in rs)} threads without a cached answer)", demo, rs)
    n, (s, x, y) = closest_pair(b, demo)
    print(f"  demo catalog: {n} application names, closest pair {x} / {y} at {s:.3f}")

    for i, s in enumerate(a.stores):
        d = work / f"store-{i}"
        if d.exists(): shutil.rmtree(d)
        (d / ".ograph").mkdir(parents=True)
        for f in ("store.redb", "config.toml", "ontology.ttl"):
            if (pathlib.Path(s) / f).exists(): shutil.copy(pathlib.Path(s) / f, d / ".ograph" / f)
        rs = reasons(b, d)
        report(f"demo store with model answers #{i} ({sum('model call failed' in r for r in rs)} failed model calls)", d, rs)

    cases = sorted(p for p in pathlib.Path(a.cases).iterdir() if (p / ".ograph" / "config.toml").exists())
    per = collections.Counter(); amb_cases = 0; facts = 0
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for d, rs in ex.map(lambda c: harvest_case(b, c, work / "rcaeval"), cases):
            t, _ = tally(b, d, rs)
            per.update(t); facts += len(rs); amb_cases += bool(t)
    total.update(per)
    print(f"rcaeval: {len(cases)} cases, {facts} quarantined facts, {sum(per.values())} facts quarantined for an ambiguous application name {dict(per)} in {amb_cases} cases")
    n, (s, x, y) = closest_pair(b, work / "rcaeval" / cases[0].name)
    print(f"  rcaeval catalog (online-boutique fixture): {n} application names, closest pair {x} / {y} at {s:.3f}")

    g = collections.Counter()
    for line in pathlib.Path(a.gateway).read_text().splitlines():
        m = re.match(r"\| ([a-z-]+) \| \d+ \| \d+ \| (\d+) \((.*)\) \|$", line)
        if m and AMBIGUOUS.match(m.group(3)):
            g["sibling" if "sibling" in m.group(3) and m.group(1) != "fuzzy-app-name" else "low score" if "fuzzily" in m.group(3) else "?"] += int(m.group(2))
    print(f"gateway gate (synthetic, by construction): {sum(g.values())} facts quarantined for an ambiguous application name {dict(g)}")
    print(f"\nties, the only kind context can settle, on real or realistic data (demo + stores + rcaeval): {total['tie']}")

if __name__ == "__main__":
    main()
