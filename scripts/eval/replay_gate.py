#!/usr/bin/env python3
"""Gate for `ograph replay`: does counting flipped verdicts give the same AC@1 as scoring
`ograph chains` against the labels?

Each RCAEval case is harvested into its own store under its configured settings (chain
ranking, 30-minute window) and its true chain (the best-ranked chain rooted at the labeled
service) is accepted as a verdict. Then for the configured settings and for each of the 206
propagation weight combinations of `rcaeval_score.py --grid`, every case runs both
  ograph chains INCIDENT --json --top 5 --rank propagation --set prop_weights=W   (the grid)
  ograph replay --json --set rank=propagation --set prop_weights=W              (the replay)
AC@1 from the grid is the share of cases whose rank-1 application is the label. From the
replay it is the share of verdicts the configured settings agree with, minus the flips, plus
the fixed verdicts. The gate passes when the two agree on every case at every point.

Usage: replay_gate.py --bin target/release/ograph --cases target/rcaeval-cases [--jobs 4]
"""
import argparse, concurrent.futures as cf, itertools, json, pathlib, statistics, subprocess, sys, time

def run(binp, d, *args):
    t = time.time()
    p = subprocess.run([binp, "--json", *args], cwd=d, capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr, time.time() - t

def main():
    ap = argparse.ArgumentParser(); ap.add_argument("--bin", required=True); ap.add_argument("--cases", required=True); ap.add_argument("--jobs", type=int, default=4)
    a = ap.parse_args()
    binp = str(pathlib.Path(a.bin).resolve()); root = pathlib.Path(a.cases).resolve()
    labels = json.loads((root / "labels.json").read_text())
    cids = sorted(labels)

    def prepare(cid):
        d = root / cid; lab = labels[cid]
        (d / ".ograph" / "store.redb").unlink(missing_ok=True)
        rc, _, err, _ = run(binp, d, "harvest")
        if rc:
            return cid, f"harvest failed: {err[-200:]}"
        q = f'SELECT ?c ?r WHERE {{ ?c corr:leadsTo ?i ; corr:rank ?r ; corr:candidateApp id:app\\/{lab["root"]} . ?i inc:id "{lab["incident"]}" }} ORDER BY ?r LIMIT 1'
        rc, out, err, _ = run(binp, d, "sparql", q)
        rows = json.loads(out)["rows"] if out.strip() else []
        if not rows:
            return cid, "no chain rooted at the true service"
        rc, _, err, _ = run(binp, d, "verdict", rows[0][0], "accept", "--evidence", "RCAEval label", "--by", "eval")
        return cid, None if rc == 0 else f"verdict failed: {err[-200:]}"

    t = time.time()
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        failed = {c: e for c, e in ex.map(prepare, cids) if e}
    print(f"harvested {len(cids)} cases and accepted the true chain of {len(cids) - len(failed)} in {time.time() - t:.0f} s; failures: {failed}")
    if failed:
        sys.exit("every case needs its true cause accepted")

    steps = [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6]
    combos = [w for w in itertools.product(steps, repeat=4) if abs(sum(w) - 1) < 1e-9]
    points = [("configured (chain ranking)", [], [])] + [(f"propagation {w}", ["--rank", "propagation", "--set", f"prop_weights={json.dumps(list(w))}"], ["--set", "rank=propagation", "--set", f"prop_weights={json.dumps(list(w))}"]) for w in combos]

    def one(cid, chains_args, replay_args):
        d = root / cid; lab = labels[cid]
        _, out, _, _ = run(binp, d, "chains", lab["incident"], "--top", "5", *chains_args)
        apps = [x["app"] for x in json.loads(out)["apps"]] if out.strip() else []
        rc, out, err, secs = run(binp, d, "replay", *replay_args)
        r = json.loads(out)
        assert rc == (1 if r["flips"] else 0) and len(r["verdicts"]) == 1, (cid, rc, r["flips"], err)
        return apps[:1] == [lab["root"]], r, secs

    splits = {"all": cids, "tuning (rep 1)": [c for c in cids if labels[c]["rep"] == 1], "held out (reps 2-3)": [c for c in cids if labels[c]["rep"] != 1]}
    mismatched_cases = 0; replay_secs = []; rows = []
    t = time.time()
    with cf.ThreadPoolExecutor(a.jobs) as ex:
        for name, chains_args, replay_args in points:
            res = dict(zip(cids, ex.map(lambda c: one(c, chains_args, replay_args), cids)))
            held = {c: r["verdicts"][0]["before"]["contradicts"] is None for c, (_, r, _) in res.items()}
            mismatched_cases += sum(hit != ((held[c] and not r["flips"]) or (not held[c] and r["fixed"] == 1)) for c, (hit, r, _) in res.items())
            replay_secs += [s for _, _, s in res.values()]
            cells = []
            for split, cs in splits.items():
                grid = sum(res[c][0] for c in cs) / len(cs)
                flips = sum(res[c][1]["flips"] for c in cs); fixed = sum(res[c][1]["fixed"] for c in cs)
                cells.append((split, grid, (sum(held[c] for c in cs) - flips + fixed) / len(cs), flips, fixed, len(cs)))
            rows.append((name, cells))
    wall = time.time() - t

    for name, cells in rows:
        print(f"{name}: " + "; ".join(f"{s} grid AC@1 {g:.4f} replay {r:.4f} ({f} flips, {x} fixed of {n})" for s, g, r, f, x, n in cells))
    agree = sum(all(abs(g - r) < 1e-12 for _, g, r, _, _, _ in cells) for _, cells in rows)
    tune = sorted(((cells[1][1], cells[1][2], name) for name, cells in rows[1:]), key=lambda x: x[0], reverse=True)
    print(f"\ntop 5 propagation weights on the tuning split (grid AC@1 = replay AC@1):")
    for g, r, name in tune[:5]:
        print(f"  {name}: {g:.2f} = {r:.2f}")
    print(f"settings points: {len(rows)}; points where grid and replay AC@1 agree on every split: {agree}/{len(rows)}; cases that disagree: {mismatched_cases} of {len(rows) * len(cids)}")
    print(f"{len(replay_secs)} replay runs (one verdict each) in {wall:.0f} s wall for both commands with {a.jobs} jobs; replay per run: median {statistics.median(replay_secs) * 1000:.0f} ms, max {max(replay_secs) * 1000:.0f} ms")
    ok = agree == len(rows) and not mismatched_cases
    print("PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)

main()
