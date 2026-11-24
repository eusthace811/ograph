#!/usr/bin/env python3
"""What does the new-message detector report on real telemetry?

RCAEval RE2-OB: 90 fault injections into Online Boutique, with the service logs of each. For
every case this harvests, lists the LOG_NEW events (the first error line or two of a message a
service had not written in the ten minutes of log before), and says whether each is on the
service the fault was injected into or on a service that calls it, after the injection.

  lognew_eval.py --bin target/release/ograph --cases data/rcaeval-cases
"""
import argparse, collections, concurrent.futures as cf, datetime, json, pathlib, subprocess


def run(cmd, cwd):
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True).stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True); ap.add_argument("--jobs", type=int, default=6)
    a = ap.parse_args()
    binp = str(pathlib.Path(a.bin).resolve()); root = pathlib.Path(a.cases).resolve()
    labels = json.loads((root / "labels.json").read_text())
    q = 'SELECT ?a ?t ?n ?tpl WHERE { ?e evt:signal "LOG_NEW" ; evt:on ?a ; evt:at ?t ; evt:count ?n ; evt:template ?tpl }'
    def one(cid):
        d = root / cid
        (d / ".ograph" / "store.redb").unlink(missing_ok=True)
        run([binp, "harvest"], d)
        rows = json.loads(run([binp, "sparql", q, "--json"], d) or '{"rows": []}')["rows"]
        callers = {r[0].rsplit("/", 1)[-1] for r in json.loads(run([binp, "sparql", f'SELECT ?c WHERE {{ ?c ops:dependsOn id:app\\/{labels[cid]["root"]} }}', "--json"], d) or '{"rows": []}')["rows"]}
        bursts = json.loads(run([binp, "sparql", 'SELECT (COUNT(*) AS ?n) WHERE { ?e evt:signal "LOG_ERROR" }', "--json"], d))["rows"][0][0]
        return cid, rows, callers, int(bursts)
    events, bursts, cases_with = [], 0, 0
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for cid, rows, callers, b in ex.map(one, sorted(labels)):
            bursts += b
            cases_with += bool(rows)
            for app, t, n, tpl in rows:
                app = app.rsplit("/", 1)[-1]
                at = datetime.datetime.fromisoformat(t.replace("Z", "+00:00")).timestamp()
                where = "the injected service" if app == labels[cid]["root"] else "a caller of it" if app in callers else "another service"
                events.append((cid, labels[cid]["fault"], labels[cid]["root"], app, where, at >= labels[cid]["inject"], n, tpl))
    print(f"{len(labels)} cases: {bursts} log bursts (LOG_ERROR) and {len(events)} new messages (LOG_NEW), in {cases_with} cases.")
    by = collections.Counter((w, after) for *_, w, after, _, _ in events)
    for (w, after), k in sorted(by.items()):
        print(f"  {k:>3} on {w}, {'after' if after else 'before'} the injection")
    print("\n| case | fault on | event on | lines | message |\n|---|---|---|---|---|")
    for cid, fault, root_, app, where, after, n, tpl in events:
        print(f"| {cid} | {root_} ({fault}) | {app} | {n} | {tpl[:90]} |")


if __name__ == "__main__":
    main()
