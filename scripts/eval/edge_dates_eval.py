#!/usr/bin/env python3
"""How often does a dependency's date matter, and what does dating cost?

For each git repository: `ogrep --entities . --json` and `--dated`, three runs each, timed. Of the
dependencies ograph keeps (`depends_on`, and `calls` as with `inferred = true`), it counts those
ograph dates: a pair whose every edge has a commit that differs from the commit since which the files have declared its
service. An edge that came with its service is not dated (see docs/EVIDENCE.md, Phase 13 lane O).

Then, for each dated edge, the incidents it would be withheld from: those that opened before it.
With --incidents CASES it reads RCAEval's incident times (RE2-OB runs on Online Boutique, which is
GoogleCloudPlatform/microservices-demo); without, it counts edges dated within 30, 90 and 365 days
before the repository's last commit, i.e. for an incident that long ago.

  edge_dates_eval.py --ogrep BIN REPO... [--incidents data/rcaeval-cases]
"""
import argparse, datetime, json, pathlib, statistics, subprocess, time

DAY = 86400

def entities(ogrep, repo, dated):
    args = [ogrep, "--entities", ".", "--json"] + (["--dated"] if dated else [])
    t = time.monotonic()
    out = subprocess.run(args, cwd=repo, capture_output=True, text=True, check=True).stdout
    return time.monotonic() - t, [json.loads(l)["data"] for l in out.splitlines()]

def day(t):
    return datetime.datetime.fromtimestamp(t, datetime.UTC).strftime("%Y-%m-%d")

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ogrep", required=True); ap.add_argument("repos", nargs="+")
    ap.add_argument("--incidents"); ap.add_argument("--runs", type=int, default=3)
    a = ap.parse_args()
    opened = []
    if a.incidents:
        for f in sorted(pathlib.Path(a.incidents).glob("*/incidents.json")):
            opened += [i["opened"] for i in json.loads(f.read_text())]
    for repo in a.repos:
        repo = pathlib.Path(repo).resolve()
        git = lambda *x: subprocess.run(["git", *x], cwd=repo, capture_output=True, text=True).stdout.strip()
        head_time = int(git("log", "-1", "--format=%ct"))
        touching = len(git("log", "--first-parent", "--format=%h", "HEAD", "--", "*.yaml", "*.yml", "*.env").split())
        plain = [entities(a.ogrep, repo, False)[0] for _ in range(a.runs)]
        runs = [entities(a.ogrep, repo, True) for _ in range(a.runs)]
        dated_s = [r[0] for r in runs]
        data = runs[0][1]
        print(f"{repo.name}: HEAD {git('rev-parse', '--short', 'HEAD')} of {day(head_time)}, "
              f"{git('rev-list', '--first-parent', '--count', 'HEAD')} first-parent commits, {touching} change an operations file")
        print(f"  --entities         {', '.join(f'{s:.2f}' for s in plain)} s (median {statistics.median(plain):.2f})")
        print(f"  --entities --dated {', '.join(f'{s:.2f}' for s in dated_s)} s (median {statistics.median(dated_s):.2f})")
        since = {(d["entity"]["kind"], d["entity"]["id"]): d["entity"].get("since") for d in data}
        # ograph keeps one ops:dependsOn per pair, from any file and relation: undated when any
        # edge of the pair is, else dated by the earliest.
        pairs = {}
        for d in data:
            for e in d["edges"]:
                if e["relation"] not in ("depends_on", "calls"):
                    continue
                s, svc = e.get("since"), since.get(("service", e["from"]))
                state = "none" if s is None else "with" if svc is not None and svc["commit"] == s["commit"] else s
                pairs.setdefault((e["from"], e["to"]), []).append(state)
        dated, came_with, none = [], 0, 0
        for (f, t), states in sorted(pairs.items()):
            if "none" in states:
                none += 1
            elif "with" in states:
                came_with += 1
            else:
                dated.append({"from": f, "to": t, "since": min(states, key=lambda s: s["time"])})
        edges = pairs
        print(f"  dependencies (pairs): {len(edges)}; {came_with} came with their service, {none} have no commit, {len(dated)} were added to a service already declared")
        for e in sorted(dated, key=lambda e: (-e["since"]["time"], e["from"], e["to"])):
            print(f"    {day(e['since']['time'])}  {e['from']} -> {e['to']} {e['since']['commit'][:7]}")
        if opened:
            hit = [e for e in dated if e["since"]["time"] > min(opened)]
            print(f"  RCAEval incidents opened {day(min(opened))} to {day(max(opened))} ({len(opened)}): "
                  f"{len(hit)} dated edges are later than the first, {sum(e['since']['time'] > max(opened) for e in dated)} later than the last")
            for e in hit:
                n = sum(t < e["since"]["time"] for t in opened)
                print(f"    {e['from']} -> {e['to']}: withheld from {n} of {len(opened)} incidents")
        for days in (30, 90, 365):
            n = sum(e["since"]["time"] > head_time - days * DAY for e in dated)
            print(f"  an incident {days} days before HEAD: {n} of {len(edges)} dependencies withheld")
        print()

if __name__ == "__main__":
    main()
