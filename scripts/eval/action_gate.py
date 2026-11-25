#!/usr/bin/env python3
"""Operator-action gate on examples/demo (discussions/actions.json, labels in actions-labels.json).

1. Accuracy: the configured model's committed evt:OperatorAction facts from actions.json against
   the labels, matched on (kind, application, time). Precision must be at least 0.9 and recall
   at least 0.8. Every committed action's time must be the time of the message its evidence
   names, and no thread labeled with no action may get one.
2. Replay: a second harvest makes no model call.
3. Broken model: a copy of the workspace with scripts/eval/bad_model.py as the model. Every
   committed action must have a kind from the vocabulary, a catalog application named in its
   thread, words found in the message its evidence names, and that message's time.

Usage: action_gate.py --bin target/release/ograph --demo examples/demo
"""
import argparse, collections, datetime, json, pathlib, shutil, subprocess, sys, tempfile

KINDS = {"ROLLBACK", "RESTART", "SCALE", "FAILOVER", "FLAG", "CONFIG", "DEPLOY"}
APPS = {"web-frontend", "checkout", "payments-api", "orders-api", "search-api", "inventory", "auth-service", "ledger-db", "notifications", "elasticsearch"}
Q = """SELECT ?k ?app ?at ?q ?rec WHERE { GRAPH ?g { ?a a evt:OperatorAction ; evt:actionKind ?k ; evt:on ?app ; evt:at ?at ; evt:summary ?q ; evt:sourceRecord ?rec } FILTER(CONTAINS(STR(?g), "/discussions/")) }"""
QUARANTINE = """SELECT ?f ?r WHERE { GRAPH ?g { ?x a harv:Quarantined ; harv:fact ?f ; harv:reason ?r } FILTER(CONTAINS(STR(?g), "discussions")) FILTER(CONTAINS(?f, "action/")) }"""

def og(binp, root, *args):
    p = subprocess.run([binp, "--root", str(root), *args], capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr

def rows(binp, root, q):
    rc, out, err = og(binp, root, "--json", "sparql", q)
    if rc not in (0, 1): raise SystemExit(err)
    return json.loads(out)["rows"]

def secs(iso):
    return int(datetime.datetime.fromisoformat(iso.replace("Z", "+00:00")).timestamp())

def records(root):
    out = {}
    for f in (root / "discussions").iterdir():
        for i, m in enumerate(json.loads(f.read_text())):
            out[f"discussions/{f.name}#{i + 1}"] = m
    return out

def check(binp, root):
    """Committed actions, and those that break a rule, with the rule."""
    recs = records(root)
    thread_text = collections.defaultdict(str)
    for m in recs.values():
        thread_text[(m.get("channel"), m.get("thread_ts", m["ts"]))] += " " + m["text"].lower()
    got, bad = [], []
    for k, app, at, q, rec in rows(binp, root, Q):
        app = app.rsplit("/", 1)[-1]
        m = recs.get(rec)
        got.append((k, app, at, q, rec))
        why = [w for w, broken in [
            ("kind outside the vocabulary", k not in KINDS),
            ("application outside the catalog", app not in APPS),
            ("evidence is not a message", m is None),
            ("words not in the message", m is not None and " ".join(q.lower().split()) not in " ".join(m["text"].lower().split())),
            ("time is not the message's", m is not None and secs(at) != int(float(m["ts"]))),
            ("thread does not name the application", m is not None and app not in thread_text[(m.get("channel"), m.get("thread_ts", m["ts"]))]),
        ] if broken]
        if why: bad.append((k, app, at, q, rec, why))
    return got, bad

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
    a = ap.parse_args(); binp = str(pathlib.Path(a.bin).resolve()); demo = pathlib.Path(a.demo).resolve()
    labels = json.loads((demo / "actions-labels.json").read_text())
    gold = {(x["kind"], x["app"], int(float(x["ts"]))) for acts in labels.values() for x in acts}
    trap_roots = {r for r, acts in labels.items() if not acts}
    got, bad = check(binp, demo)
    recs = records(demo)
    mine = [(k, app, secs(at), q, rec) for k, app, at, q, rec in got if rec.startswith("discussions/actions.json")]
    found = {(k, app, t) for k, app, t, _, _ in mine}
    tp = found & gold
    precision = len(tp) / max(len(found), 1); recall = len(tp) / len(gold)
    print(f"1. accuracy on {len(labels)} threads ({len(gold)} labeled actions, {len(trap_roots)} threads with none): committed {len(found)}, correct {len(tp)}; precision {precision:.2f}, recall {recall:.2f}")
    for x in sorted(found - gold): print(f"   wrong: {x}")
    for x in sorted(gold - found): print(f"   missed: {x}")
    on_traps = [x for x in mine if recs[x[4]].get("thread_ts", recs[x[4]]["ts"]) in trap_roots]
    print(f"   actions committed on threads that only ask, propose or plan: {len(on_traps)} {on_traps}")
    print(f"   committed actions breaking a rule: {len(bad)} {bad}")
    other = [x for x in got if not x[4].startswith("discussions/actions.json")]
    print(f"   actions from the 16 unlabeled category threads (ops.json), not scored: {len(other)}")
    for x in sorted(other): print(f"     {x[0]} {x[1]} at {x[2]}: {x[3]}")
    reasons = collections.Counter(r.split('"')[-1].strip() if r.startswith('"') else r.split(":")[0] for _, r in rows(binp, demo, QUARANTINE))
    print(f"   actions the model proposed and the checks refused, by reason: {dict(sorted(reasons.items()))}")
    cache = demo / ".ograph" / "cache" / "llm.jsonl"
    before = len(cache.read_text().splitlines())
    og(binp, demo, "harvest")
    after = len(cache.read_text().splitlines())
    print(f"2. replay: cache lines {before} -> {after} (new model calls: {after - before})")
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="ograph-badactions-"))
    shutil.copytree(demo, tmp / "demo", ignore=shutil.ignore_patterns("store.redb", "cache"))
    cfg = (tmp / "demo" / ".ograph" / "config.toml").read_text().split("[llm.extract]")[0]
    bad_model = pathlib.Path(__file__).with_name("bad_model.py").resolve()
    (tmp / "demo" / ".ograph" / "config.toml").write_text(cfg + f'[llm.extract]\ncommand = ["python3", "{bad_model}"]\nmodel = "broken-fixture"\nparallel = 4\n')
    og(binp, tmp / "demo", "harvest")
    bgot, bbad = check(binp, tmp / "demo")
    breasons = collections.Counter(r.split('"')[-1].strip() if r.startswith('"') else r.split(":")[0] for _, r in rows(binp, tmp / "demo", QUARANTINE))
    _, audit, _ = og(binp, tmp / "demo", "validate")
    print(f"3. broken model (every action it reports is invented): committed {len(bgot)}, breaking a rule {len(bbad)} {bbad[:3]}")
    for x in sorted(bgot): print(f"     committed: {x[0]} {x[1]} at {x[2]}: {x[3]} ({x[4]})")
    print(f"   refused, by reason: {dict(sorted(breasons.items()))}")
    print(f"   audit: {audit.strip().splitlines()[-1]}")
    shutil.rmtree(tmp)
    exact = all(secs(at) == int(float(recs[rec]["ts"])) for _, _, at, _, rec in got)
    ok = precision >= 0.9 and recall >= 0.8 and not on_traps and not bad and exact and after == before and not bgot
    print("ACTION GATE:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

main()
