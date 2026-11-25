#!/usr/bin/env python3
"""Gate for the known-applications slice in the discussions prompt (Phase 13, lane P).

Written before the candidate code. Parent and candidate binaries harvest the same scratch copies
of examples/demo, with three discussions sources and no actions: the 16 demo threads, the 31
action threads, and the 30 threads of chat-mentions, which name applications the way people do or
only look like they do. apps-labels.json lists the catalog applications each thread refers to.

A. Accuracy, model-backed, --runs fresh workspaces per binary with no cache, interleaved:
   committed disc:about applications against the labels. Precision, recall, false positives
   (committed applications the thread does not refer to), and quarantined application names on
   threads where a labeled application was missed (real mentions the gateway refused).
B. Cache keys, free: echo_model.py logs every call. Harvest, then add an application to the
   catalog and an alias that no thread uses to `notifications`, and harvest again. Pass when the
   second harvest asks exactly the threads whose first known-applications list held
   `notifications`, and no other.

Pass for KEEP, fixed before any model run: mean recall of the candidate above the parent's, with
the candidate's lowest run at least the parent's highest; mean precision not lower; mean false
positives not higher; B passes. Equal recall is no gain.

Usage: catalog_slice_gate.py --parent BIN --candidate BIN --demo examples/demo --out DIR [--runs 3]
       catalog_slice_gate.py --candidate BIN --demo examples/demo --out DIR --keys-only
"""
import argparse, datetime, json, pathlib, re, shutil, statistics, subprocess, sys

HERE = pathlib.Path(__file__).resolve().parent
SOURCES = '[[source]]\nkind = "discussions"\npath = "discussions"\n[[source]]\nkind = "discussions"\npath = "chat-actions"\n[[source]]\nkind = "discussions"\npath = "chat-mentions"\n'
APP = "https://ograph.dev/id/app/"

def og(binp, root, *args):
    p = subprocess.run([binp, "--root", str(root), *args], capture_output=True, text=True)
    if p.returncode == 2: raise SystemExit(f"{args}: {p.stderr}")
    return p.stdout

def rows(binp, root, q):
    return json.loads(og(binp, root, "--json", "sparql", q))["rows"]

def workspace(demo, d, model=None):
    if d.exists(): shutil.rmtree(d)
    shutil.copytree(demo, d, ignore=shutil.ignore_patterns("store.redb", "cache"))
    cfg = d / ".ograph" / "config.toml"
    text = re.sub(r'\[\[source\]\]\nkind = "discussions"\npath = "discussions"\n\[\[source\]\]\nkind = "discussions"\npath = "chat-actions"\nactions = true\n', SOURCES, cfg.read_text())
    assert SOURCES in text, "demo config changed shape"
    if model:
        text = text.split("[llm.extract]")[0] + f'[llm.extract]\ncommand = {json.dumps(model)}\nmodel = "echo"\nparallel = 4\n'
    cfg.write_text(text)

def root_key(channel, at):
    sec = int(datetime.datetime.fromisoformat(at.replace("Z", "+00:00")).timestamp())
    return channel, sec

def score(binp, d, labels):
    gold = {(ch, int(float(ts))): set(apps) for ch, by in labels.items() for ts, apps in by.items()}
    got = {k: set() for k in gold}
    for ch, at, a in rows(binp, d, 'SELECT ?ch ?at ?a WHERE { ?t a disc:Thread ; disc:channel ?ch ; disc:at ?at ; disc:about ?a . FILTER(STRSTARTS(STR(?a), "' + APP + '")) }'):
        got.setdefault(root_key(ch, at), set()).add(a[len(APP):])
    quarantined = {}
    for f, r in rows(binp, d, 'SELECT ?f ?r WHERE { GRAPH ?g { ?q harv:fact ?f ; harv:reason ?r } FILTER(CONTAINS(STR(?g), "discussions")) }'):
        m = re.search(r'thread/([a-z]+)-(\d+)-\d+ disc:about ops:Application\?"(.*)"$', f)
        if m: quarantined.setdefault((m.group(1), int(m.group(2))), []).append((m.group(3), r))
    tp = sum(len(got[k] & gold[k]) for k in gold)
    fp = sorted((k[0], k[1], a) for k in gold for a in got[k] - gold[k])
    fn = sorted((k[0], k[1], a) for k in gold for a in gold[k] - got[k])
    missed = {(c, t) for c, t, _ in fn}
    refused = sorted((k[0], k[1], n, r) for k, qs in quarantined.items() if k in missed for n, r in qs)
    all_q = sum(len(v) for v in quarantined.values())
    per = {}
    for ch in labels:
        ks = [k for k in gold if k[0] == ch]
        t = sum(len(got[k] & gold[k]) for k in ks); p = sum(len(got[k]) for k in ks); g = sum(len(gold[k]) for k in ks)
        per[ch] = f"P {t}/{p} R {t}/{g}"
    pred = sum(len(got[k]) for k in gold); want = sum(len(v) for v in gold.values())
    calls = len(rows(binp, d, 'SELECT ?a ?b WHERE { GRAPH ?g { ?a ops:dependsOn ?b } FILTER(CONTAINS(STR(?g), "/discussions/")) }'))
    degraded = [l for l in (d / "harvest.out").read_text().splitlines() if "discussions" in l]
    return dict(precision=tp / pred if pred else 1.0, recall=tp / want, tp=tp, pred=pred, gold=want, fp=fp, fn=fn,
                refused_real=refused, quarantined_apps=all_q, per=per, calls=calls, harvest=degraded)

def accuracy(a, labels):
    out = pathlib.Path(a.out).resolve()
    res = {"parent": [], "candidate": []}
    for r in range(1, a.runs + 1):
        for name in ["parent", "candidate"]:
            d = out / f"run{r}-{name}"
            done = d / "score.json"
            if not done.exists():
                workspace(pathlib.Path(a.demo).resolve(), d)
                (d / "harvest.out").write_text(og(getattr(a, name), d, "harvest"))
                done.write_text(json.dumps(score(getattr(a, name), d, labels), indent=1))
            s = json.loads(done.read_text())
            res[name].append(s)
            print(f"run {r} {name}: precision {s['precision']:.3f} ({s['tp']}/{s['pred']}), recall {s['recall']:.3f} ({s['tp']}/{s['gold']}), "
                  f"false positives {len(s['fp'])}, real mentions quarantined {len(s['refused_real'])}, app names quarantined {s['quarantined_apps']}, "
                  f"model calls committed {s['calls']}; {s['per']}")
            for line in s["harvest"]: print("   ", line)
            for x in s["fp"]: print("    false positive:", x)
            for x in s["fn"]: print("    missed:", x)
            for x in s["refused_real"]: print("    quarantined real mention:", x)
    m = {n: {k: statistics.mean(s[k] if not isinstance(s[k], list) else len(s[k]) for s in v) for k in ["precision", "recall", "fp", "refused_real"]} for n, v in res.items()}
    span = {n: (min(s["recall"] for s in v), max(s["recall"] for s in v)) for n, v in res.items()}
    for n in m:
        print(f"{n}: mean precision {m[n]['precision']:.3f}, mean recall {m[n]['recall']:.3f} (runs {span[n][0]:.3f}-{span[n][1]:.3f}), "
              f"mean false positives {m[n]['fp']:.2f}, mean real mentions quarantined {m[n]['refused_real']:.2f}")
    p, c = m["parent"], m["candidate"]
    gain = c["recall"] > p["recall"] and span["candidate"][0] >= span["parent"][1]
    ok = gain and c["precision"] >= p["precision"] and c["fp"] <= p["fp"]
    print(f"A. recall gain beyond the spread: {gain}; precision not lower: {c['precision'] >= p['precision']}; false positives not higher: {c['fp'] <= p['fp']}")
    return ok

def keys(a):
    out = pathlib.Path(a.out).resolve(); d = out / "keys"; log = out / "keys.log"
    log.unlink(missing_ok=True)
    workspace(pathlib.Path(a.demo).resolve(), d, ["python3", str(HERE / "echo_model.py"), str(log)])
    og(a.candidate, d, "harvest")
    first = [json.loads(l) for l in log.read_text().splitlines()]
    with open(d / "catalog" / "catalog-info.yaml", "a") as f:
        f.write("---\napiVersion: backstage.io/v1alpha1\nkind: Component\nmetadata:\n  name: billing-api\nspec:\n  type: service\n  owner: team-payments\n")
    with open(d / "runbooks" / "platform.md", "a") as f:
        f.write("\nnotifications (aka mailer) sends email and push.\n")
    subprocess.run(["git", "-C", str(d / "catalog"), "add", "-A"], capture_output=True)
    subprocess.run(["git", "-C", str(d / "catalog"), "-c", "user.name=gate", "-c", "user.email=gate@example.invalid", "commit", "-qm", "billing-api"], capture_output=True)
    log.write_text("")
    og(a.candidate, d, "harvest")
    second = [json.loads(l) for l in log.read_text().splitlines()]
    expect = sorted(x["thread"] for x in first if "notifications" in x["known"])
    asked = sorted(x["thread"] for x in second)
    with_slice = sum(1 for x in first if x["known"])
    ok = asked == expect and len(first) > 0
    print(f"B. cache keys: first harvest asked {len(first)} threads, {with_slice} with a known-applications list, {len(expect)} of them listing notifications; "
          f"after the catalog edit the harvest asked {len(asked)}: {'exactly those' if ok else 'NOT those'}")
    for t in sorted(set(asked) ^ set(expect)): print("    differs:", t[:100])
    return ok

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--parent"); ap.add_argument("--candidate", required=True); ap.add_argument("--demo", required=True)
    ap.add_argument("--out", required=True); ap.add_argument("--runs", type=int, default=3); ap.add_argument("--keys-only", action="store_true")
    a = ap.parse_args()
    a.candidate = str(pathlib.Path(a.candidate).resolve())
    pathlib.Path(a.out).mkdir(parents=True, exist_ok=True)
    k = keys(a)
    if a.keys_only:
        sys.exit(0 if k else 1)
    a.parent = str(pathlib.Path(a.parent).resolve())
    labels = json.loads((pathlib.Path(a.demo) / "apps-labels.json").read_text())
    ok = accuracy(a, labels) and k
    print("CATALOG SLICE GATE:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

main()
