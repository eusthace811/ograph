#!/usr/bin/env python3
"""`ograph similar` for incidents with no telemetry: the 36 write-up incidents of code_eval.py (G).

Six services of the OpenTelemetry demo by six faults. Each fault has a past incident, titled
the way its write-up words it (code_eval's write-up heading), and a new incident on the same
service that words the same fault differently (code_eval's G incident title). No incident has
any telemetry. For each new incident, `ograph similar` over the incidents opened before it:
is the most similar one the past incident of the same service and fault? Earlier new incidents
are candidates too, but none shares both.

  base      the binary before this change
  plain     the new binary, no model: the signature is what the records state
  classify  the new binary, `classify = true`: the model proposes each title's fault class
  telemetry-base, telemetry
            every incident also has the same burst of the same error message on its service, so
            the telemetry signatures of one service's incidents are equal; no model
  both      as telemetry, with `classify = true`: rank fusion of the two signatures

Chance is a uniform pick among the incidents opened before each new one; "chance given the
application" is a uniform pick among those on the same service.

Usage: similar_writeups_eval.py --bin target/release/ograph --base OLD_BIN --repo OTEL_DEMO --work DIR
       [--modes base,plain,classify,telemetry-base,telemetry,both]
"""
import argparse, collections, json, pathlib, shutil, subprocess, sys

sys.dont_write_bytecode = True
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from code_eval import FAULTS, burst, catalog, iso, sh  # noqa: E402

T_PAST = 1774915200  # 2026-03-31
T_NEW = 1777593600  # 2026-05-01, code_eval's t_g
STEP = 21600
LLM = '[llm.extract]\ncommand = ["pi", "-p", "--no-tools", "--no-session", "--no-extensions", "--no-skills", "--no-context-files", "--offline", "--thinking", "off", "--model", "openai/gpt-6-luna"]\nmodel = "pi:openai/gpt-6-luna"\nparallel = 4\n'


def build(root, repo, svcs, mode):
    root.mkdir(parents=True)
    sh(["git", "clone", "-q", str(repo), str(root / "shop")])
    past, new, label = [], [], {}
    for s in svcs:
        for k, (written, said) in FAULTS.items():
            i = len(past)
            past.append({"id": f"INC-P-{i:03}", "title": f"{s}: {written}", "service": s, "opened": T_PAST + i * STEP, "severity": "SEV2", "status": "resolved", "resolved": T_PAST + i * STEP + 3600, "mitigation": f"see docs/postmortems/{s}-{k}.md"})
            new.append({"id": f"INC-G-{i:03}", "title": f"{s}: {said}", "service": s, "opened": T_NEW + i * STEP, "severity": "SEV2", "status": "open"})
            label[f"INC-P-{i:03}"] = label[f"INC-G-{i:03}"] = (s, k)
    (root / "incidents").mkdir()
    (root / "incidents" / "past.json").write_text(json.dumps(past))
    (root / "incidents" / "new.json").write_text(json.dumps(new))
    sources = '[[source]]\nkind = "catalog"\npath = "shop"\n[[source]]\nkind = "incidents"\npath = "incidents"\n'
    if mode in ("classify", "both"):
        sources += "classify = true\n"
    if mode in ("telemetry-base", "telemetry", "both"):
        (root / "logs").mkdir()
        for x in past + new:
            burst(root / "logs" / f"{x['id']}.jsonl", x["service"], x["opened"] - 300, "request failed: upstream returned an error")
        sources += '[[source]]\nkind = "logs"\npath = "logs"\n'
    assert sh([BIN[mode], "init"], root).returncode == 0
    (root / ".ograph" / "config.toml").write_text(sources + "[correlation]\nwindow_s = 1800\n" + (LLM if mode in ("classify", "both") else ""))
    cache = root / ".ograph" / "cache" / "llm.jsonl"
    if mode in ("classify", "both") and CACHE.exists():
        cache.parent.mkdir(exist_ok=True)
        shutil.copy(CACHE, cache)
    p = sh([BIN[mode], "harvest"], root)
    assert p.returncode == 0, p.stderr
    if cache.exists():
        shutil.copy(cache, CACHE)
    return past, new, label, p.stdout.strip().splitlines()[-1]


def og(mode, root, *args):
    p = sh([BIN[mode], "--root", str(root), "--json", *args])
    return json.loads(p.stdout) if p.returncode == 0 and p.stdout.strip() else None


def score(mode, root, past, new, label):
    hit1 = hit3 = answered = 0
    chance = chance_app = 0.0
    per = collections.Counter()
    misses = []
    opened = {x["id"]: x["opened"] for x in past + new}
    for q in new:
        earlier = [i for i, t in opened.items() if t < q["opened"]]
        chance += 1 / len(earlier)
        chance_app += 1 / sum(label[i][0] == q["service"] for i in earlier)
        out = og(mode, root, "similar", q["id"], "--top", "3") or []
        answered += bool(out)
        got = [label.get(o["incident"].rsplit("/", 1)[-1].upper()) for o in out]
        hit1 += got[:1] == [label[q["id"]]]
        hit3 += label[q["id"]] in got
        per[label[q["id"]][1]] += got[:1] == [label[q["id"]]]
        if got[:1] != [label[q["id"]]]:
            misses.append(f"{q['id']} {label[q['id']]} -> {[(o['incident'], o['similarity']) for o in out[:2]]}")
    return hit1, hit3, answered, chance, chance_app, per, misses


def classes(mode, root):
    rows = (og(mode, root, "sparql", "SELECT ?id ?c WHERE { ?i inc:id ?id ; inc:faultClass ?c }", "--limit", "1000") or {}).get("rows", [])
    return dict(rows)


BIN, CACHE = {}, pathlib.Path()


def main():
    global CACHE
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--base", required=True); ap.add_argument("--repo", required=True); ap.add_argument("--work", required=True)
    ap.add_argument("--modes", default="base,plain,classify,telemetry-base,telemetry,both")
    a = ap.parse_args()
    new_bin, old_bin = str(pathlib.Path(a.bin).resolve()), str(pathlib.Path(a.base).resolve())
    BIN.update({"base": old_bin, "telemetry-base": old_bin, "plain": new_bin, "classify": new_bin, "telemetry": new_bin, "both": new_bin})
    work = pathlib.Path(a.work).resolve()
    repo = pathlib.Path(a.repo).resolve()
    CACHE = work / "llm.jsonl"  # kept across runs, so a rerun asks the model nothing new
    ids, deps = catalog(repo)
    svcs = sorted(s for s in ids if s in deps)[:6]
    head = sh(["git", "rev-parse", "HEAD"], repo).stdout.strip()
    print(f"OpenTelemetry demo at {head[:12]}; services {', '.join(svcs)}; faults {', '.join(FAULTS)}")
    print(f"{len(svcs) * len(FAULTS)} past incidents worded as their write-ups, {len(svcs) * len(FAULTS)} new incidents worded differently\n")
    print("| configuration | some answer | top-1 same service and fault | in top 3 | chance | chance given the application |\n|---|---|---|---|---|---|")
    detail = []
    for mode in a.modes.split(","):
        root = work / mode
        shutil.rmtree(root, ignore_errors=True)
        past, new, label, last = build(root, repo, svcs, mode)
        h1, h3, ans, ch, cha, per, misses = score(mode, root, past, new, label)
        n = len(new)
        print(f"| {mode} | {ans}/{n} | {h1}/{n} = {h1 / n:.2f} | {h3}/{n} = {h3 / n:.2f} | {ch / n:.3f} | {cha / n:.3f} |")
        detail.append((mode, last, per, misses, classes(mode, root) if mode in ("classify", "both") else {}, label))
    for mode, last, per, misses, cls, label in detail:
        print(f"\n## {mode}\n\nharvest: {last}")
        print("top-1 hits by fault: " + ", ".join(f"{k} {per[k]}/{len(per) and 6}" for k in FAULTS))
        if cls:
            by = collections.defaultdict(collections.Counter)
            for i, c in cls.items():
                by[("past" if i.startswith("INC-P") else "new", label[i][1])][c] += 1
            print("fault class proposed, by fault (past titles | new titles):")
            for k in FAULTS:
                print(f"  {k:<7} {dict(by[('past', k)])} | {dict(by[('new', k)])}")
        for m in misses[:12]:
            print("  miss " + m)


if __name__ == "__main__":
    main()
