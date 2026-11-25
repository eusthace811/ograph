#!/usr/bin/env python3
"""Gate for `ograph resolve`: no mention may resolve to the wrong application.

Three catalogs: the demo's, RCAEval's online-boutique fixture, and the six applications of the
gateway gate (ograph-bench/src/gateway_eval.rs) with its four bad names. Each catalog name gets
variants whose labels follow from a rule fixed before any run:

- the same application: the name, upper case, `-` and `_` swapped, one middle letter deleted,
  two middle letters swapped (names of 6 letters or more), and a `-svc` suffix;
- no application (near-twins): `-canary`, `-v2`, a `legacy-` prefix, and the last word of a
  multi-word name replaced by another word (`-eu` -> `-uk`).

A variant whose normalized form (lower case, letters and digits) is a catalog name or alias is
labeled that application instead. Wrong = resolved to an application other than the label, or
resolved when the label is none. Precision = right resolutions / resolutions. Recall = right
resolutions / mentions that name an application. The gate: precision 1.0.

Usage: resolve_eval.py --bin target/release/ograph --demo examples/demo
       --boutique data/fixtures/online-boutique --work target/resolve-eval
"""
import argparse, json, pathlib, re, shutil, subprocess

GATEWAY_APPS = ["checkout", "payments-api", "cart", "currency", "email", "inventory-service-eu"]
GATEWAY_BAD = [("inventory-service-uk", None), ("inventory-service-eu-canary", None), ("Ledger Service", None), ("currencies-svc", "currency")]

def norm(s): return re.sub(r"[^0-9a-z]", "", s.lower())

def ograph(b, root, *args):
    p = subprocess.run([b, "--root", str(root), *args], capture_output=True, text=True)
    if p.returncode == 2: raise SystemExit(f"ograph {' '.join(args)}: {p.stderr}")
    return p.stdout

def workspace(b, root, catalog):
    if root.exists(): shutil.rmtree(root)
    root.mkdir(parents=True)
    ograph(b, root, "init")
    (root / ".ograph" / "config.toml").write_text(f'[[source]]\nkind = "catalog"\npath = "{catalog}"\n')
    ograph(b, root, "harvest")
    return root

def compose_repo(path, apps):
    if path.exists(): shutil.rmtree(path)
    path.mkdir(parents=True)
    (path / "docker-compose.yml").write_text("services:\n" + "".join(f"  {a}:\n    image: x/{a}\n" for a in apps))
    for c in (["init", "-q"], ["add", "."], ["-c", "user.email=e@x", "-c", "user.name=e", "commit", "-qm", "catalog"]):
        subprocess.run(["git", *c], cwd=path, check=True)
    return path

def catalog(b, root):
    rows = lambda p: json.loads(ograph(b, root, "--json", "sparql", f"SELECT ?a ?n WHERE {{ ?a a ops:Application ; {p} ?n }}"))["rows"]
    names = {a: n for a, n in rows("ops:name")}
    iri = {norm(n): a for a, n in rows("ops:name") + rows("ops:alias")}
    return names, iri

def variants(name):
    same = [name, name.upper(), name.replace("-", "_") if "-" in name else name.replace("_", "-"), f"{name}-svc"]
    if len(name) >= 6:
        m = len(name) // 2
        same += [name[:m] + name[m + 1:], name[:m - 1] + name[m] + name[m - 1] + name[m + 1:]]
    words = re.split(r"[-_]", name)
    twins = [f"{name}-canary", f"{name}-v2", f"legacy-{name}"]
    if len(words) > 1:
        twins.append("-".join(words[:-1] + ["uk" if words[-1] != "uk" else "us"]))
    return same, twins

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
    ap.add_argument("--boutique", required=True); ap.add_argument("--work", required=True)
    a = ap.parse_args()
    b = str(pathlib.Path(a.bin).resolve()); work = pathlib.Path(a.work).resolve()
    sets = [
        ("demo", pathlib.Path(a.demo).resolve(), []),
        ("rcaeval online-boutique", workspace(b, work / "boutique", pathlib.Path(a.boutique).resolve()), []),
        ("gateway gate", workspace(b, work / "gateway", compose_repo(work / "gateway-catalog", GATEWAY_APPS)), GATEWAY_BAD),
    ]
    total = {"mentions": 0, "named": 0, "resolved": 0, "right": 0, "wrong": 0}
    for label, root, extra in sets:
        names, iri = catalog(b, root)
        mentions = []
        for app, name in sorted(names.items()):
            same, twins = variants(name)
            mentions += [(m, app) for m in same] + [(m, None) for m in twins]
        mentions += [(m, f"id:app/{t}" if t else None) for m, t in extra]
        mentions = [(m, iri.get(norm(m), want)) for m, want in mentions]
        seen = set(); mentions = [x for x in mentions if not (x[0] in seen or seen.add(x[0]))]
        c = {"mentions": len(mentions), "named": sum(1 for _, w in mentions if w), "resolved": 0, "right": 0, "wrong": 0}
        wrong, missed = [], []
        for k in range(0, len(mentions), 5):
            batch = mentions[k:k + 5]
            out = json.loads(ograph(b, root, "--json", "resolve", *[m for m, _ in batch]))
            for (m, want), r in zip(batch, out["results"]):
                if r["status"] == "resolved":
                    c["resolved"] += 1
                    if r["app"] == want: c["right"] += 1
                    else: c["wrong"] += 1; wrong.append(f"{m} -> {r['app']} (label {want})")
                elif want:
                    missed.append(f"{m} ({r['status']}: {r['rationale']})")
        print(f"## {label}: {len(names)} applications, {c['mentions']} mentions ({c['named']} name an application)")
        print(f"resolved {c['resolved']}, right {c['right']}, wrong {c['wrong']}; recall {c['right'] / c['named']:.3f}")
        for w in wrong: print("  WRONG", w)
        for m in missed: print("  missed", m)
        for k in total: total[k] += c[k]
    precision = total["right"] / total["resolved"] if total["resolved"] else 1.0
    print(f"\nall: {total['mentions']} mentions, {total['named']} name an application, resolved {total['resolved']}, wrong {total['wrong']}")
    print(f"precision {precision:.3f}  recall {total['right'] / total['named']:.3f}")
    print("RESOLVE GATE:", "PASS" if total["wrong"] == 0 else "FAIL")

main()
