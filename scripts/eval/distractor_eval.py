#!/usr/bin/env python3
"""Run and score the distractor suite (examples/distractors) with agent_rca_eval.py's harness.

  check   no model: per case, stage it the way the harness does (agent_rca_eval.stage: a copy
          under --work, harvested), scan a workspace of each arm for leaks (leaks_in_dir), and
          record the harvest's quarantine, `ograph chains` rank 1, the distractor
          application's rank in chains and whether `ograph before` lists it. Writes
          check.json and prints a table per distractor type.
  run     agent_rca_eval.main over every case of the suite (--cases SUITE --sample all), with
          the arms, runner and model given; then `report`.
  report  AC@1 per arm and per distractor type from OUT/runs.jsonl and the suite's labels, and
          how often each arm answered the planted distractor's application (fooled).

Usage:
  python3 examples/distractors/generate.py
  distractor_eval.py check --bin BIN --suite examples/distractors/cases --work DIR
  distractor_eval.py run --bin BIN --suite examples/distractors/cases --work DIR --out OUT \\
      [--arms mcp,skill,raw,chains] [--runs 3] [--runner claude --model claude-haiku-5-5] [--parallel 4]
  distractor_eval.py report --suite examples/distractors/cases --out OUT"""
import argparse, collections, json, pathlib, shutil, sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import agent_rca_eval as rca  # noqa: E402

TYPES = {"a": "chronic", "b": "deploy", "c": "alert", "d": "sparse", "e": "precedent"}


def labels_of(suite):
    return json.loads((pathlib.Path(suite) / "labels.json").read_text())


def check(a):
    suite = pathlib.Path(a.suite).resolve()
    labels = labels_of(suite)
    ctx = rca.Ctx(argparse.Namespace(work=a.work, bin=a.bin, cases=str(suite), guidance=None)); ctx.setup()
    rows = {}
    for cid, lab in sorted(labels.items()):
        case = rca.Case("rcaeval", cid, lab["incident"], lab["root"], suite / cid)
        staged = rca.stage(ctx, case)
        p = rca.run([ctx.bin, "harvest", "--json"], staged, env=ctx.env(ctx.bin.parent))
        harvest = json.loads(p.stdout) if p.returncode == 0 else []
        quarantine = [q["reason"] for g in harvest for q in g.get("quarantined", [])]
        degraded = sorted({d for g in harvest for d in g.get("degraded", [])})
        leaks = {}
        for arm in ("mcp", "raw"):
            ws = rca.workspace(ctx, case, rca.ARMS[arm])
            leaks[arm] = rca.leaks_in_dir(ctx, ws)
            shutil.rmtree(ws.parent, ignore_errors=True)
        ch = rca.run([ctx.bin, "--json", "chains", lab["incident"], "--top", "10"], staged)
        apps = [x["app"] for x in json.loads(ch.stdout)["apps"]] if ch.returncode in (0, 1) and ch.stdout.strip() else []
        be = rca.run([ctx.bin, "--json", "before", lab["incident"]], staged)
        events = json.loads(be.stdout).get("events", []) if be.returncode == 0 and be.stdout.strip() else []
        lure = lab["distractor_app"]
        lure_ev = [e for e in events if e.get("app") == lure]
        rows[cid] = {"type": lab["distractor"], "root": lab["root"], "distractor_app": lure, "chains": apps[:5],
                     "rank1_right": bool(apps) and apps[0] == lab["root"],
                     "root_rank": apps.index(lab["root"]) + 1 if lab["root"] in apps else None,
                     "distractor_rank": apps.index(lure) + 1 if lure in apps else None,
                     "distractor_in_before": [f"{e['signal']} {e.get('severity')}" for e in lure_ev],
                     "quarantine": quarantine, "degraded": degraded, "leaks": leaks}
    out = pathlib.Path(a.work) / "check.json"
    out.write_text(json.dumps(rows, indent=1))
    print(f"| type | cases | chains rank 1 right | distractor in chains top 5 | distractor ranked above the root | distractor in before | quarantined facts | leak hits |\n|---|---|---|---|---|---|---|---|")
    for k, name in TYPES.items():
        rs = [r for r in rows.values() if r["type"] == k]
        above = sum(r["distractor_rank"] is not None and (r["root_rank"] is None or r["distractor_rank"] < r["root_rank"]) for r in rs)
        print(f"| {k} {name} | {len(rs)} | {sum(r['rank1_right'] for r in rs)} | {sum(r['distractor_rank'] is not None and r['distractor_rank'] <= 5 for r in rs)} "
              f"| {above} | {sum(bool(r['distractor_in_before']) for r in rs)} | {sum(len(r['quarantine']) for r in rs)} | {sum(len(v) for r in rs for v in r['leaks'].values())} |")
    deg = collections.Counter(d for r in rows.values() for d in r["degraded"])
    for d, n in deg.most_common(): print(f"degraded on {n} cases: {d}")
    for cid, r in rows.items():
        for q in r["quarantine"]: print(f"quarantine {cid}: {q}")
        for arm, ls in r["leaks"].items():
            for l in ls: print(f"leak {cid} {arm}: {l}")
    print(f"wrote {out}")
    return 0


def report(a):
    labels = labels_of(a.suite)
    rows = [json.loads(l) for l in (pathlib.Path(a.out) / "runs.jsonl").read_text().splitlines()]
    rows = [r for r in rows if r["case"].split("/")[1] in labels]
    arms = list(dict.fromkeys(r["arm"] for r in rows))
    print("AC@1 over scored runs (fooled: runs that answered the planted distractor's application)\n")
    print("| arm | all | " + " | ".join(f"{k} {n}" for k, n in TYPES.items()) + " | voided |\n|---|---|" + "---|" * len(TYPES) + "---|")
    for arm in arms:
        mine = [r for r in rows if r["arm"] == arm]
        cells = []
        for k in [None, *TYPES]:
            rs = [r for r in mine if r["status"] in ("pass", "fail") and (k is None or labels[r["case"].split("/")[1]]["distractor"] == k)]
            p = sum(r["status"] == "pass" for r in rs)
            fooled = sum(r["root"] == labels[r["case"].split("/")[1]]["distractor_app"] for r in rs)
            cells.append(f"{p}/{len(rs)} ({p / max(len(rs), 1):.2f}), fooled {fooled}")
        voided = sum(r["status"] in ("leak", "error") for r in mine)
        print(f"| {arm} | " + " | ".join(cells) + f" | {voided} |")
    print()
    print("| type | fooled, agent arms | fooled share | max share of one arm |\n|---|---|---|---|")
    for k, n in TYPES.items():
        per = {}
        for arm in arms:
            if arm == "chains": continue
            rs = [r for r in rows if r["arm"] == arm and r["status"] in ("pass", "fail") and labels[r["case"].split("/")[1]]["distractor"] == k]
            per[arm] = (sum(r["root"] == labels[r["case"].split("/")[1]]["distractor_app"] for r in rs), len(rs))
        f, t = sum(x[0] for x in per.values()), sum(x[1] for x in per.values())
        top = max((x[0] / x[1] for x in per.values() if x[1]), default=0)
        print(f"| {k} {n} | {f}/{t} | {f / max(t, 1):.2f} | {top:.2f} |")
    cost = sum(r.get("cost", 0) for r in rows)
    print(f"\ncost by the runner's report: ${cost:.2f} over {len(rows)} rows")
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mode", choices=("check", "run", "report"))
    ap.add_argument("--suite", required=True); ap.add_argument("--bin"); ap.add_argument("--work"); ap.add_argument("--out")
    ap.add_argument("--arms", default="mcp,skill,raw,chains"); ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--runner", default="claude"); ap.add_argument("--model", default="claude-haiku-5-5")
    ap.add_argument("--parallel", type=int, default=4); ap.add_argument("--run-budget", default="1.0")
    a = ap.parse_args(argv)
    if a.mode == "check":
        if not (a.bin and a.work): ap.error("check needs --bin and --work")
        return check(a)
    if a.mode == "run":
        if not (a.bin and a.work and a.out): ap.error("run needs --bin, --work and --out")
        n = len(labels_of(a.suite))
        rc = rca.main(["--bin", a.bin, "--cases", a.suite, "--sample", str(n), "--runs", str(a.runs), "--arms", a.arms,
                       "--runner", a.runner, "--model", a.model, "--parallel", str(a.parallel), "--work", a.work, "--out", a.out,
                       "--run-budget", a.run_budget])
        print()
        report(a)
        return rc
    if not a.out: ap.error("report needs --out")
    return report(a)


if __name__ == "__main__":
    raise SystemExit(main())
