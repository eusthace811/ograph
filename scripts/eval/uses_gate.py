#!/usr/bin/env python3
"""Reverse-provenance gate: `ograph uses FILE` predicts what deleting FILE and harvesting again
removes. For each file: predict on a copy of the store, delete the file (and its `[[source]]`
entry when the entry names the file itself), harvest, and compare.

Categories (docs/EVIDENCE.md, Phase 13 lane R): facts that disappear from the union of the input
graphs; chains gone or relinked; current verdicts on those chains; and, reported apart, chains
new after the removal, rank moves and quarantine entries that go. Files of naming sources
(catalog, ownership, docs) are out of scope and reported apart. Triples about an incident that
another file's records also state are left out of both sets and counted.

Usage: uses_gate.py --bin target/release/ograph --demo examples/demo --cases data/rcaeval-cases
       --work target/uses-gate [--model] [--merged target/rcaeval-merged] [--seed 13] [--n-cases 12] [--n-files 16]
"""
import argparse, json, pathlib, random, shutil, subprocess, sys, time

NAMING = {"catalog", "ownership", "docs"}


def og(binp, root, *args, ok=(0,)):
    r = subprocess.run([binp, "--root", str(root), *args], capture_output=True, text=True)
    if r.returncode not in ok:
        sys.exit(f"ograph {' '.join(args[:2])} in {root} failed (rc {r.returncode}): {r.stderr.strip()[-400:]}")
    return r.stdout


def ogj(binp, root, *args, ok=(0,)):
    out = og(binp, root, "--json", *args, ok=ok)
    return json.loads(out) if out.strip() else None


def dump(binp, root):
    """Every quad, and per chain its incident, root, links and rank."""
    rows = ogj(binp, root, "sparql", "SELECT ?s ?p ?o ?g WHERE { GRAPH ?g { ?s ?p ?o } }")["rows"]
    quads = {tuple(r) for r in rows}
    chains = {}
    for s, p, o, g in quads:
        if g.startswith("g:harvest/correlation/") and s != g:
            c = chains.setdefault(s, {"links": set(), "rank": None, "incident": None, "root": None})
            if p == "corr:link":
                c["links"].add(o)
            elif p == "corr:rank":
                c["rank"] = int(o)
            elif p == "corr:leadsTo":
                c["incident"] = o
            elif p == "corr:rootCause":
                c["root"] = o
    return quads, chains


def is_input(g):
    return g.startswith("g:harvest/") and not g.startswith(("g:harvest/correlation/", "g:harvest/verdicts/"))


def current_verdicts(quads):
    """Chain -> current verdict node."""
    on = {s: o for s, p, o, g in quads if p == "rca:onChain"}
    superseded = {s for s, p, o, g in quads if p == "rca:supersededBy"}
    return {c: v for v, c in on.items() if v not in superseded}


def measure(before, after, chains_before, chains_after):
    graph_nodes = {g for *_, g in before}
    facts_b = {(s, p, o) for s, p, o, g in before if is_input(g) and s not in graph_nodes}
    facts_a = {(s, p, o) for s, p, o, g in after if is_input(g) and s not in graph_nodes}
    key = lambda c: (c["incident"], c["root"], frozenset(c["links"]))
    changed = {c for c, v in chains_before.items() if c not in chains_after or key(v) != key(chains_after[c])}
    new = set(chains_after) - set(chains_before)
    moved = {c for c, v in chains_before.items() if c in chains_after and c not in changed and v["rank"] != chains_after[c]["rank"]}
    verdicts = {c for c in current_verdicts(before) if c in changed}
    q_b = {s for s, p, o, g in before if g.startswith("g:quarantine/") and p == "harv:fact"}
    q_a = {s for s, p, o, g in after if g.startswith("g:quarantine/") and p == "harv:fact"}
    return {"facts": facts_b - facts_a, "chains": changed, "verdicts": verdicts, "new": new, "moved": moved, "quarantine": q_b - q_a}


def predict(u):
    chains = lambda *k: {c["chain"] for c in u["chains"] if c["change"] in k}
    return {
        "facts": {(f["subject"], f["predicate"], f["object"]) for f in u["facts"] if not f["also_in"]},
        "chains": chains("gone", "relinked"),
        "verdicts": {v["chain"] for v in u["verdicts"]},
        "new": chains("new"),
        "moved": chains("moved"),
        "quarantine": {q["node"] for q in u["quarantined"]},
    }


def accept_rank1(binp, root):
    rows = ogj(binp, root, "sparql", "SELECT ?c WHERE { ?c corr:rank 1 }", ok=(0, 1))["rows"]
    for (c,) in rows:
        og(binp, root, "verdict", c, "accept", "--evidence", "gate fixture", "--by", "uses-gate")
    return len(rows)


def drop_source(root, f):
    """A source whose path is the file, or a directory the file was the last one in, goes from
    config.toml with the file: ograph keeps the last good facts of a configured path that it
    cannot read or that lists nothing, as it would for an unmounted share."""
    config = root / ".ograph" / "config.toml"
    lines = config.read_text().split("\n")
    paths = [f] + [str(d.relative_to(root)) for d in (root / f).parents if d != root and d.is_relative_to(root) and not any(x.is_file() for x in d.rglob("*"))]
    path = next((x for x in paths if f'path = "{x}"' in lines), None)
    if path is None:
        return
    at = lines.index(f'path = "{path}"')
    start = max(i for i in range(at) if lines[i] == "[[source]]")
    end = next((i for i in range(at + 1, len(lines)) if lines[i].startswith("[")), len(lines))
    config.write_text("\n".join(lines[:start] + lines[end:]))


def trial(binp, base, work, f):
    w = work / "trial"
    shutil.rmtree(w, ignore_errors=True)
    shutil.copytree(base, w, symlinks=True)
    t = time.time()
    u = ogj(binp, w, "uses", str(w / f), ok=(0, 1))
    secs = time.time() - t
    before, cb = dump(binp, w)
    (w / f).unlink()
    drop_source(w, f)
    og(binp, w, "harvest")
    after, ca = dump(binp, w)
    m, p = measure(before, after, cb, ca), predict(u)
    shared = set(u["shared"])
    excluded = {x for x in m["facts"] | p["facts"] if x[0] in shared}
    m["facts"] -= excluded
    p["facts"] -= excluded
    return m, p, secs, u.get("naming"), len(excluded)


def pr(m, p):
    tp = len(m & p)
    return (tp / len(p) if p else 1.0), (tp / len(m) if m else 1.0)


def demo_base(binp, demo, work, model):
    """The demo, harvested. A thread whose model answer is not JSON is not cached and is asked
    again at every harvest, so trials run with a stub model that answers no JSON: no spend, and
    before and after see the same answers. With `model` the base harvest asks the configured
    model once and keeps its answers in WORK/llm.jsonl; later runs read them from there."""
    base = work / "demo"
    shutil.rmtree(base, ignore_errors=True)
    shutil.copytree(demo, base)
    subprocess.run([sys.executable, "generate.py"], cwd=base, check=True, capture_output=True)
    cfg, cache, kept = base / ".ograph" / "config.toml", base / ".ograph" / "cache" / "llm.jsonl", work / "llm.jsonl"
    stub = lambda: cfg.write_text("\n".join('command = ["/bin/echo", "no model in this gate"]' if l.startswith("command = ") else l for l in cfg.read_text().split("\n")))
    if not model:
        stub()
        if kept.exists():
            cache.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(kept, cache)
    (base / ".ograph" / "store.redb").unlink(missing_ok=True)
    og(binp, base, "harvest")
    if model:
        shutil.copy(cache, kept)
        stub()
    files = ["alerts.json", "catalog/catalog-info.yaml", "chat-actions/actions.json", "config.jsonl", "deployments.jsonl", "discussions/ops.json",
             "incidents.json", "logs.jsonl", "metrics.csv", "news.jsonl", "ownership.jsonl", "runbooks/payments.md", "runbooks/platform.md"]
    return base, files


def rca_base(binp, cases, work, rng, n_cases, n_files):
    labels = json.loads((cases / "labels.json").read_text())
    picked = sorted(rng.sample(sorted(labels), n_cases))
    base = work / "rca"
    shutil.rmtree(base, ignore_errors=True)
    (base / ".ograph").mkdir(parents=True)
    cfg = (cases / picked[0] / ".ograph" / "config.toml").read_text().split("[[source]]")
    src = "[[source]]" + [s for s in cfg if 'kind = "catalog"' in s][0]
    files = []
    for k, cid in enumerate(picked):
        (base / cid).mkdir()
        names = [("incidents", "incidents.json"), ("metrics", "metrics.csv"), ("logs", "logs.jsonl"), ("traces", "traces.jsonl")] + ([("ownership", "ownership.jsonl")] if k == 0 else [])
        for kind, name in names:
            (base / cid / name).symlink_to(cases / cid / name)
            src += f'[[source]]\nkind = "{kind}"\npath = "{cid}/{name}"\n\n'
            files.append(f"{cid}/{name}")
    (base / ".ograph" / "config.toml").write_text(src + '[correlation]\nwindow_s = 1800\nrank = "chain"\n')
    og(binp, base, "harvest")
    return base, picked, sorted(rng.sample(files, n_files))


def time_merged(binp, merged, rng, n):
    """`uses` time on the 90-case merged store that similar_eval.py builds, for n of its files."""
    paths = [l.split('"')[1] for l in (merged / ".ograph" / "config.toml").read_text().split("\n") if l.startswith("path = ")]
    secs = []
    for f in sorted(rng.sample([x for x in paths if x.endswith((".json", ".csv", ".jsonl"))], n)):
        t = time.time()
        u = ogj(binp, merged, "uses", f, ok=(0, 1))
        secs.append(time.time() - t)
        changes = sum(c["change"] in ("gone", "relinked") for c in u["chains"])
        print(f"  {secs[-1]:.2f} s  {pathlib.Path(f).parent.name}/{pathlib.Path(f).name}: {len(u['facts'])} facts, {len(u['events'])} events, {changes} chains gone or relinked, {len(u['verdicts'])} verdicts")
    secs.sort()
    print(f"merged store, {n} files: median {secs[len(secs) // 2]:.2f} s, max {secs[-1]:.2f} s")


def main():
    ap = argparse.ArgumentParser()
    for a in ("--bin", "--demo", "--cases", "--work"):
        ap.add_argument(a, required=True)
    ap.add_argument("--model", action="store_true", help="harvest the demo base once with its configured model (costs money)")
    ap.add_argument("--merged", help="the merged store of similar_eval.py --out, to time uses on")
    ap.add_argument("--seed", type=int, default=13); ap.add_argument("--n-cases", type=int, default=12); ap.add_argument("--n-files", type=int, default=16)
    a = ap.parse_args()
    binp, work = str(pathlib.Path(a.bin).resolve()), pathlib.Path(a.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    rng = random.Random(a.seed)
    demo, demo_files = demo_base(binp, pathlib.Path(a.demo).resolve(), work, a.model)
    rca, picked, rca_files = rca_base(binp, pathlib.Path(a.cases).resolve(), work, rng, a.n_cases, a.n_files)
    print(f"seed {a.seed}; RCAEval cases: {' '.join(picked)}")
    for base in (demo, rca):
        print(f"{base.name}: accepted the rank-1 chain of {accept_rank1(binp, base)} incidents")
    cats = ["facts", "chains", "verdicts", "new", "moved", "quarantine"]
    totals = {scope: {c: [set(), set()] for c in cats} for scope in ("in scope", "naming sources")}
    for base, files in ((demo, demo_files), (rca, rca_files)):
        for f in files:
            m, p, secs, naming, excluded = trial(binp, base, work, f)
            scope = "naming sources" if naming in NAMING else "in scope"
            for c in cats:
                totals[scope][c][0] |= {(base.name, f, x) for x in m[c]}
                totals[scope][c][1] |= {(base.name, f, x) for x in p[c]}
            cells = "  ".join(f"{c} {len(m[c] & p[c])}/{len(m[c])}m/{len(p[c])}p" for c in cats)
            ok = all(m[c] == p[c] for c in ("facts", "chains", "verdicts"))
            print(f"{base.name}/{f} [{naming or 'in scope'}] uses {secs * 1000:.0f} ms  {cells}  shared-incident triples left out {excluded}  {'EXACT' if ok else 'MISMATCH'}")
            for c in ("facts", "chains", "verdicts"):
                for x in sorted(m[c] - p[c])[:3]:
                    print(f"    missed {c}: {x}")
                for x in sorted(p[c] - m[c])[:3]:
                    print(f"    extra {c}: {x}")
    passed = True
    for scope, t in totals.items():
        print(f"\n{scope}: precision / recall per category (correct / measured / predicted)")
        for c in cats:
            m, p = t[c]
            prec, rec = pr(m, p)
            print(f"  {c:<11} precision {prec:.3f}  recall {rec:.3f}  ({len(m & p)} / {len(m)} / {len(p)})")
            if scope == "in scope" and c in ("facts", "chains", "verdicts"):
                passed &= prec == 1.0 and rec == 1.0
    print(f"\ngate (in scope; facts, chains, verdicts at 1.000): {'PASS' if passed else 'FAIL'}")
    if a.merged:
        print()
        time_merged(binp, pathlib.Path(a.merged).resolve(), rng, 12)
    sys.exit(0 if passed else 1)


if __name__ == "__main__":
    main()
