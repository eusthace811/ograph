#!/usr/bin/env python3
"""Does `ograph ontology diff` predict what a re-harvest under the edited ontology quarantines?

For each mutation of ograph.ttl or shapes.ttl: harvest the demo under the shipped ontology, run
`ontology diff` on the mutated files against the stored data, then write the mutated files into
the workspace and harvest again. Passes when, for every mutation, the predicted number of facts
quarantined per harvest graph equals the facts the second harvest quarantines that the first did
not, and no change to a term the store never uses is marked BREAKING.

Usage: ontology_diff_eval.py --bin target/release/ograph --demo target/demo --work target/diff-eval
"""
import argparse, collections, json, pathlib, re, shutil, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
TTL = (ROOT / "ontology" / "ograph.ttl").read_text()
SHAPES = (ROOT / "ontology" / "shapes.ttl").read_text()

def line_sub(text, start, pattern, repl):
    lines = text.split("\n")
    hits = [i for i, l in enumerate(lines) if l.startswith(start)]
    assert len(hits) == 1, (start, hits)
    new = re.sub(pattern, repl, lines[hits[0]], count=1)
    assert new != lines[hits[0]], (start, pattern)
    lines[hits[0]] = new
    return "\n".join(lines)

def drop_line(text, start):
    lines = text.split("\n")
    kept = [l for l in lines if not l.startswith(start)]
    assert len(kept) == len(lines) - 1, start
    return "\n".join(kept)

# name -> (ograph.ttl, shapes.ttl, the term a change to which must not be BREAKING, or None)
MUTATIONS = {
    "remove used class evt:ConfigChange": (drop_line(TTL, "evt:ConfigChange a "), SHAPES, None),
    "remove unused class evt:TraceSlow": (drop_line(TTL, "evt:TraceSlow a "), SHAPES, "evt:TraceSlow"),
    "remove used property evt:template": (drop_line(TTL, "evt:template a "), SHAPES, None),
    "remove unused property evt:codeLocation": (drop_line(TTL, "evt:codeLocation a "), SHAPES, "evt:codeLocation"),
    "narrow range disc:about to inc:Incident": (line_sub(TTL, "disc:about a ", r"rdfs:range og:Thing", "rdfs:range inc:Incident"), SHAPES, None),
    "widen range evt:on to og:Thing": (line_sub(TTL, "evt:on a ", r"rdfs:range ops:Application", "rdfs:range og:Thing"), SHAPES, None),
    "narrow domain evt:summary to evt:Alert": (line_sub(TTL, "evt:summary a ", r"rdfs:domain evt:Event", "rdfs:domain evt:Alert"), SHAPES, None),
    "drop subclass edge evt:Deployment < evt:Event": (line_sub(TTL, "evt:Deployment a ", r"rdfs:subClassOf evt:Event", "rdfs:subClassOf og:Thing"), SHAPES, None),
    "drop subclass edge of unused evt:Commit": (line_sub(TTL, "evt:Commit a ", r"rdfs:subClassOf evt:Event", "rdfs:subClassOf og:Thing"), SHAPES, "evt:Commit"),
    "close vocabulary evt:metric to one value": (TTL + '\nevt:metric og:vocabulary [ og:value "connections" ] .\n', SHAPES, None),
    "close vocabulary of unused evt:codeLocation": (TTL + '\nevt:codeLocation og:vocabulary [ og:value "a.rs:1" ] .\n', SHAPES, "evt:codeLocation"),
    "remove vocabulary value evt:severity SEV4": (line_sub(TTL, '  [ og:value "SEV4"', r'.*', '') .replace('"SEV3" ; rdfs:comment "Minor: limited impact." ],', '"SEV3" ; rdfs:comment "Minor: limited impact." ] .'), SHAPES, None),
    "make disc:about functional": (line_sub(TTL, "disc:about a ", r"owl:ObjectProperty", "owl:ObjectProperty, owl:FunctionalProperty"), SHAPES, None),
    "require inc:mitigation on incidents": (TTL, SHAPES + "\nshape:Mitigated a sh:NodeShape ; sh:targetClass inc:Incident ; sh:property [ sh:path inc:mitigation ; sh:minCount 1 ] .\n", None),
}

def workspace(src, dst):
    if dst.exists():
        shutil.rmtree(dst)
    (dst / ".ograph").mkdir(parents=True)
    for f in src.iterdir():
        if f.name != ".ograph":
            (dst / f.name).symlink_to(f.resolve())
    shutil.copy(src / ".ograph" / "config.toml", dst / ".ograph" / "config.toml")
    shutil.copytree(src / ".ograph" / "cache", dst / ".ograph" / "cache")
    (dst / ".ograph" / "ontology.ttl").write_text(TTL)
    (dst / ".ograph" / "shapes.ttl").write_text(SHAPES)

def ograph(binary, root, *args):
    p = subprocess.run([binary, "--root", str(root), "--json", *args], capture_output=True, text=True)
    if p.returncode == 2:
        sys.exit(f"{root}: ograph {' '.join(args)}: {p.stderr}")
    return json.loads(p.stdout)

def graph(g):
    return g.split("harvest/", 1)[1]

def quarantined(outcomes):
    return {graph(o["graph"]): {q["fact"] for q in o["quarantined"]} for o in outcomes}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--demo", type=pathlib.Path, required=True)
    ap.add_argument("--work", type=pathlib.Path, required=True)
    args = ap.parse_args()
    binary = str(pathlib.Path(args.bin).resolve())
    failures = 0
    print("| mutation | BREAKING lines | predicted facts quarantined (per graph) | re-harvest quarantined | match |\n|---|---|---|---|---|")
    for name, (ttl, shapes, unused) in MUTATIONS.items():
        ws = args.work / re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")
        workspace(args.demo, ws)
        before = quarantined(ograph(binary, ws, "harvest"))
        (ws / "mutated.ttl").write_text(ttl)
        (ws / "mutated-shapes.ttl").write_text(shapes)
        d = ograph(binary, ws, "ontology", "diff", str(ws / "mutated.ttl"), "--shapes", str(ws / "mutated-shapes.ttl"))
        predicted = {graph(q["graph"]): q["facts"] for q in d["quarantine"]}
        (ws / ".ograph" / "ontology.ttl").write_text(ttl)
        (ws / ".ograph" / "shapes.ttl").write_text(shapes)
        after = quarantined(ograph(binary, ws, "harvest"))
        actual = {g: len(fs - before.get(g, set())) for g, fs in after.items() if fs - before.get(g, set())}
        breaking = [c for c in d["changes"] if c.startswith("BREAKING")]
        false_breaking = unused is not None and any(unused in c for c in breaking)
        ok = predicted == actual and not false_breaking
        failures += not ok
        fmt = lambda m: ", ".join(f"{g} {n}" for g, n in sorted(m.items())) or "none"
        changes = "; ".join(c for c in d["changes"] if not c.startswith("added")) or "none"
        print(f"| {name} | {changes} | {fmt(predicted)} | {fmt(actual)} | {'yes' if ok else 'NO'} |")
    print(f"\nmutations: {len(MUTATIONS)}, mismatched or falsely BREAKING: {failures}")
    print("ONTOLOGY DIFF GATE:", "PASS" if failures == 0 else "FAIL")
    sys.exit(1 if failures else 0)

if __name__ == "__main__":
    main()
