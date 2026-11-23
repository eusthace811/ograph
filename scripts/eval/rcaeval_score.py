#!/usr/bin/env python3
"""Score ograph's root-cause ranking on exported RCAEval cases.

For each case: `ograph harvest` (unless the store exists and --reuse), then
`ograph chains <incident> --json --top 5 --rank <mode>`. Reports AC@1, AC@3 and Avg@5
(RCAEval's metrics) per mode, per fault and per split: rep 1 is the tuning split, reps 2-3
are held out. Also scores a metrics-only "z-max" baseline computed here, as a sanity check
against the published BARO numbers.

Usage: rcaeval_score.py --bin target/release/ograph --cases data/rcaeval-cases [--jobs 6]
       [--reuse] [--modes chain,propagation] [--window 600] [--out FILE]
"""
import argparse, concurrent.futures as cf, csv, json, math, pathlib, statistics, subprocess, time

def run(cmd, cwd):
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr

def zmax(case, inject):
    """Rank services by the largest robust z of any of their metrics in the 5 minutes after the
    injection, against the 5 minutes before it, with the same scale floor and one-sided rule as
    ograph's detector. It is told the injection time, which ograph is not: an oracle window."""
    rows = list(csv.reader(open(case / "metrics.csv")))
    head, data = rows[0], rows[1:]
    t = [float(r[0]) for r in data]
    best = {}
    for c in range(1, len(head)):
        svc = head[c].split("_")[0]
        vals = [(ti, float(r[c])) for ti, r in zip(t, data) if r[c] not in ("", "nan")]
        base = [v for ti, v in vals if inject - 300 <= ti < inject]
        after = [v for ti, v in vals if inject <= ti < inject + 300]
        if len(base) < 30 or not after:
            continue
        m = statistics.median(base); mad = statistics.median([abs(v - m) for v in base])
        s = max(1.4826 * mad, 0.01 * abs(m), (max(base) - min(base)) / 6, 1e-6)
        two = "workload" in head[c]
        z = max((abs(v - m) if two else v - m) / s for v in after)
        best[svc] = max(best.get(svc, 0), z)
    return [k for k, _ in sorted(best.items(), key=lambda kv: -kv[1])]

def sparql_meanshift(case, incident_s):
    """What a pure knowledge graph can rank with SPARQL 1.1 aggregates alone: per series, the
    mean of the 10 minutes before the incident against the 10 minutes before that, in units of
    the earlier window's standard deviation (AVG and AVG of squares; SPARQL has no median, MAD
    or window functions). Same windows relative to the incident as ograph, no oracle."""
    rows = list(csv.reader(open(case / "metrics.csv")))
    head, data = rows[0], rows[1:]
    best = {}
    for c in range(1, len(head)):
        svc = head[c].split("_")[0]
        pts = [(float(r[0]), float(r[c])) for r in data if r[c] not in ("", "nan")]
        a = [v for t, v in pts if incident_s - 1200 <= t < incident_s - 600]
        b = [v for t, v in pts if incident_s - 600 <= t <= incident_s]
        if len(a) < 2 or not b: continue
        m = sum(a) / len(a); sd = max(math.sqrt(max(sum(v * v for v in a) / len(a) - m * m, 0)), 1e-9)
        best[svc] = max(best.get(svc, 0), abs(sum(b) / len(b) - m) / sd)
    return [k for k, _ in sorted(best.items(), key=lambda kv: -kv[1])]

def score(ranked, root):
    pos = ranked.index(root) + 1 if root in ranked else 99
    return {"ac1": pos <= 1, "ac3": pos <= 3, "avg5": sum(pos <= k for k in range(1, 6)) / 5}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True)
    ap.add_argument("--jobs", type=int, default=6); ap.add_argument("--reuse", action="store_true")
    ap.add_argument("--modes", default="chain,propagation"); ap.add_argument("--window", default=None)
    ap.add_argument("--out", default=None)
    ap.add_argument("--set", action="append", default=[], help="correlation override passed to `ograph chains --set`")
    ap.add_argument("--grid", action="store_true", help="choose propagation weights on rep 1, then score reps 2-3 with them")
    a = ap.parse_args()
    binp = str(pathlib.Path(a.bin).resolve()); root = pathlib.Path(a.cases)
    labels = json.loads((root / "labels.json").read_text())
    modes = a.modes.split(",")
    def one(cid):
        d = root / cid; lab = labels[cid]
        t0 = time.time()
        if not (a.reuse and (d / ".ograph" / "store.redb").exists()):
            (d / ".ograph" / "store.redb").unlink(missing_ok=True)
            rc, out, err = run([binp, "harvest", "--json"], d)
            if rc != 0:
                return cid, {"error": err.strip()[-300:]}
        harvest_s = time.time() - t0
        res = {"harvest_s": harvest_s}
        for m in modes:
            cmd = [binp, "chains", lab["incident"], "--json", "--top", "5", "--rank", m]
            if a.window: cmd += ["--window", a.window]
            for kv in a.set: cmd += ["--set", kv]
            rc, out, err = run(cmd, d)
            apps = [x["app"] for x in json.loads(out)["apps"]] if rc in (0, 1) and out.strip() else []
            res[m] = apps
        res["zmax"] = zmax(d, lab["inject"])
        res["sparql_meanshift"] = sparql_meanshift(d, json.loads((d / "incidents.json").read_text())[0]["opened"])
        return cid, res
    t = time.time()
    results = {}
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for cid, r in ex.map(one, sorted(labels)):
            results[cid] = r
    wall = time.time() - t
    methods = modes + ["zmax", "sparql_meanshift"]
    def table(filt, title):
        cids = [c for c in sorted(labels) if filt(labels[c])]
        print(f"\n{title} ({len(cids)} cases)\n\n| method | AC@1 | AC@3 | Avg@5 |\n|---|---|---|---|")
        for m in methods:
            s = [score(results[c].get(m, []), labels[c]["root"]) for c in cids]
            n = max(len(s), 1)
            print(f"| {m} | {sum(x['ac1'] for x in s)/n:.2f} | {sum(x['ac3'] for x in s)/n:.2f} | {sum(x['avg5'] for x in s)/n:.2f} |")
    errors = [c for c in results if "error" in results[c]]
    print(f"{len(results)} cases in {wall:.0f} s; {len(errors)} errors {errors[:3]}")
    hs = sorted(r["harvest_s"] for r in results.values() if "harvest_s" in r)
    if hs: print(f"harvest per case: median {statistics.median(hs):.2f} s, max {max(hs):.2f} s")
    table(lambda l: True, "All RE2-OB")
    table(lambda l: l["rep"] == 1, "Tuning split (rep 1)")
    table(lambda l: l["rep"] != 1, "Held-out split (reps 2-3)")
    for f in sorted({l["fault"] for l in labels.values()}):
        table(lambda l, f=f: l["fault"] == f, f"Fault {f}")
    table(lambda l: l["traces"], "Cases with traces")
    if a.out:
        pathlib.Path(a.out).write_text(json.dumps({"results": results, "labels": labels}, indent=1))

def grid(a, labels, root, binp):
    import itertools
    steps = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6]
    combos = [w for w in itertools.product(steps, repeat=4) if abs(sum(w) - 1) < 1e-9]
    def ranks(cid, w):
        cmd = [binp, "chains", labels[cid]["incident"], "--json", "--top", "5", "--rank", "propagation", "--set", f"prop_weights={json.dumps(list(w))}"]
        rc, out, _ = run(cmd, root / cid)
        return [x["app"] for x in json.loads(out)["apps"]] if out.strip() else []
    tune = [c for c in sorted(labels) if labels[c]["rep"] == 1]
    held = [c for c in sorted(labels) if labels[c]["rep"] != 1]
    def evaluate(cids, w):
        with cf.ThreadPoolExecutor(a.jobs) as ex:
            s = list(ex.map(lambda c: score(ranks(c, w), labels[c]["root"]), cids))
        n = len(s)
        return (sum(x["ac1"] for x in s) / n, sum(x["ac3"] for x in s) / n, sum(x["avg5"] for x in s) / n)
    results = sorted(((evaluate(tune, w), w) for w in combos), key=lambda r: (r[0][0], r[0][2]), reverse=True)
    print(f"{len(combos)} weight combinations on the tuning split; top 5:")
    for (ac1, ac3, avg5), w in results[:5]:
        print(f"  strength,chain,spread,onset={w}: AC@1 {ac1:.2f} AC@3 {ac3:.2f} Avg@5 {avg5:.2f}")
    best = results[0][1]
    default = (0.4, 0.5, 0.1, 0.0)
    for name, w in [("chosen on rep 1", best), ("shipped default", default)]:
        h = evaluate(held, w); t = evaluate(tune, w)
        print(f"{name} {w}: tuning AC@1 {t[0]:.2f}/{t[1]:.2f}/{t[2]:.2f}; held-out AC@1 {h[0]:.2f} AC@3 {h[1]:.2f} Avg@5 {h[2]:.2f}")

if __name__ == "__main__":
    import sys
    if "--grid" in sys.argv:
        ap = argparse.ArgumentParser(); ap.add_argument("--bin"); ap.add_argument("--cases"); ap.add_argument("--jobs", type=int, default=8); ap.add_argument("--grid", action="store_true")
        a = ap.parse_args(); root = pathlib.Path(a.cases)
        grid(a, json.loads((root / "labels.json").read_text()), root, str(pathlib.Path(a.bin).resolve()))
        raise SystemExit
    main()
