#!/usr/bin/env python3
"""Do `ograph ontology check` and `ograph validate` flag exactly what is unfinished?

1. On the shipped ontology, the warnings for event classes and signal values that no code
   writes, and for event classes no shape requires a time of, must equal what this script finds
   by reading the sources itself (the enrichers' code, ograph.ttl, shapes.ttl).
2. After merging a sample proposed.ttl, in the format `ontology coverage` writes, every proposed
   term must be flagged as still proposed.
3. A misspelled sh:targetClass in shapes.ttl must be a `check` problem (exit 1), and `validate`
   on the harvested demo must list that shape as matching nothing.

4. For information, not gated: which shapes match nothing on each harvested store under --stores
   (the workspaces shapes_regression.py leaves under its --work/new).

Usage: ontology_lint_eval.py --bin target/release/ograph --demo target/demo --work target/lint-eval
       [--stores target/shapes-regression/new]
"""
import collections
import argparse, json, pathlib, re, shutil, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
TTL = (ROOT / "ontology" / "ograph.ttl").read_text()
SHAPES = (ROOT / "ontology" / "shapes.ttl").read_text()

SAMPLE_PROPOSALS = '''
ops:Cluster a owl:Class ;
  rdfs:comment "Proposed: needed by \\"Which cluster runs checkout?\\"" .

ops:runsOnCluster a owl:DatatypeProperty ;
  rdfs:comment "Proposed: needed by \\"Which cluster runs checkout?\\"" .

ops:region a owl:DatatypeProperty ;
  rdfs:comment "Proposed: needed by \\"Which region is payments-api in?\\"" .
'''

def expected_from_sources():
    parent = dict(re.findall(r"^(\w+:\w+) a owl:Class ; rdfs:subClassOf (\w+:\w+)", TTL, re.M))
    def ancestors(c):
        out = [c]
        while out[-1] in parent:
            out.append(parent[out[-1]])
        return out
    events = {c for c in parent if "evt:Event" in ancestors(c) and c != "evt:Event"}
    leaves = {c for c in events if c not in parent.values()}
    consts = {}
    for module, body in re.findall(r'ns!\((\w+), "[^"]+", \{(.*?)\}\);', (ROOT / "ograph-core/src/vocab.rs").read_text(), re.S):
        for const, local in re.findall(r'(\w+) = "(\w+)"', body):
            consts[f"{module}:{local}"] = f"{module}::{const}"
    code = ""
    for f in sorted((ROOT / "ograph-core/src/enrich").glob("*.rs")):
        text = f.read_text()
        code += re.sub(r"pub const WRITES.*?\];", "", text, flags=re.S)
    unwritten = sorted(c for c in leaves if not re.search(re.escape(consts[c]) + r"\b", code))
    signals = re.search(r"evt:signal og:vocabulary(.*?) \.\n", TTL, re.S).group(1)
    values = re.findall(r'og:value "(\w+)"', signals)
    unwritten += sorted(f'evt:signal "{v}"' for v in values if f'"{v}"' not in code)
    timed = set()
    for block in SHAPES.split("\n\n"):
        if re.search(r"sh:path evt:at ; sh:minCount 1", block):
            m = re.search(r"sh:targetClass ([^;]+);", block)
            timed |= {t.strip() for t in m.group(1).split(",")} if m else set()
    untimed = sorted(c for c in leaves if not timed & set(ancestors(c)))
    return unwritten, untimed

def ograph(binary, root, *args):
    p = subprocess.run([binary, "--root", str(root), "--json", *args], capture_output=True, text=True)
    if p.returncode == 2:
        sys.exit(f"ograph {' '.join(args)}: {p.stderr}")
    return p.returncode, json.loads(p.stdout)

def listed(warnings, prefix):
    line = next((w for w in warnings if w.startswith(prefix)), "")
    return sorted(t.strip() for t in line.split(": ", 1)[1].split(", ")) if line else []

def workspace(binary, src, dst, ttl, shapes):
    if dst.exists():
        shutil.rmtree(dst)
    (dst / ".ograph").mkdir(parents=True)
    if src:
        for f in src.iterdir():
            if f.name != ".ograph":
                (dst / f.name).symlink_to(f.resolve())
        shutil.copy(src / ".ograph" / "config.toml", dst / ".ograph" / "config.toml")
        shutil.copytree(src / ".ograph" / "cache", dst / ".ograph" / "cache")
    else:
        subprocess.run([binary, "--root", str(dst), "init"], check=True, capture_output=True)
    (dst / ".ograph" / "ontology.ttl").write_text(ttl)
    (dst / ".ograph" / "shapes.ttl").write_text(shapes)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--demo", type=pathlib.Path, required=True)
    ap.add_argument("--work", type=pathlib.Path, required=True)
    ap.add_argument("--stores", type=pathlib.Path)
    args = ap.parse_args()
    binary = str(pathlib.Path(args.bin).resolve())
    ok = True

    unwritten, untimed = expected_from_sources()
    ws = args.work / "shipped"
    workspace(binary, None, ws, TTL, SHAPES)
    rc, out = ograph(binary, ws, "ontology", "check")
    got_unwritten = listed(out["warnings"], "declared but no code writes it")
    got_untimed = listed(out["warnings"], "event class no shape requires an evt:at of")
    print("1. shipped ontology, empty store")
    for w in out["warnings"]:
        print(f"   warning: {w}")
    print(f"   expected from the sources, unwritten: {unwritten}")
    print(f"   expected from the sources, untimed:   {untimed}")
    same = got_unwritten == sorted(unwritten) and got_untimed == untimed and rc == 0 and not out["problems"]
    print(f"   exit {rc}, problems {len(out['problems'])}; warnings equal the sources: {same}")
    ok &= same

    ws = args.work / "proposed"
    workspace(binary, None, ws, TTL + SAMPLE_PROPOSALS, SHAPES)
    rc, out = ograph(binary, ws, "ontology", "check")
    proposed = sorted(re.findall(r"^(ops:\w+) a ", SAMPLE_PROPOSALS, re.M))
    flagged = listed(out["warnings"], "term still marked Proposed")
    print("2. after merging a sample proposed.ttl")
    for w in out["warnings"]:
        print(f"   warning: {w}")
    every = flagged == proposed and all(p in listed(out["warnings"], "declared but no code writes it") for p in proposed) and rc == 0
    print(f"   proposed terms {proposed}; flagged as proposed {flagged}; exit {rc}; every term flagged: {every}")
    ok &= every

    ws = args.work / "misspelled"
    bad = SHAPES + "\nshape:Paged a sh:NodeShape ; sh:targetClass evt:Aletr ; sh:property [ sh:path evt:severity ; sh:minCount 1 ] .\n"
    workspace(binary, args.demo, ws, TTL, bad)
    rc_check, out = ograph(binary, ws, "ontology", "check")
    print("3. shapes.ttl with sh:targetClass evt:Aletr")
    for p in out["problems"]:
        print(f"   problem: {p}")
    subprocess.run([binary, "--root", str(ws), "harvest"], check=True, capture_output=True)
    rc_validate, v = ograph(binary, ws, "validate")
    print(f"   validate: {len(v['violations'])} violations, exit {rc_validate}; focus nodes {v['shapes']}")
    print(f"   validate: matched nothing {v['unmatched']}")
    reported = rc_check == 1 and out["problems"] == ["shape:Paged in shapes.ttl names undeclared class evt:Aletr"] and "shape:Paged" in v["unmatched"] and rc_validate == 0
    print(f"   check exit {rc_check}; reported by check and validate: {reported}")
    ok &= reported

    if args.stores:
        unmatched, warned = collections.Counter(), collections.Counter()
        stores = sorted(d for d in args.stores.iterdir() if (d / ".ograph" / "store.redb").exists())
        for d in stores:
            unmatched.update(ograph(binary, d, "validate")[1]["unmatched"])
            warned.update(w.split(" (")[0] for w in ograph(binary, d, "ontology", "check")[1]["warnings"])
        print(f"4. {len(stores)} harvested stores: shapes matching nothing {dict(sorted(unmatched.items()))}")
        print(f"   check warnings by kind (stores with it) {dict(sorted(warned.items()))}")

    print("ONTOLOGY LINT GATE:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
