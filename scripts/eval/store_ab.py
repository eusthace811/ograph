#!/usr/bin/env python3
"""Runs two builds of `store_bench` in turns and reports each number's median and range, so a
change to the store is judged against run-to-run noise on a machine that may be busy.

Usage: store_ab.py OLD_STORE_BENCH NEW_STORE_BENCH [--runs 5] [--dir target/store-ab]
"""
import argparse, pathlib, re, statistics, subprocess

def parse(out):
    v = {}
    rows = [[c.strip() for c in l.strip("|").split("|")] for l in out.splitlines() if l.startswith("|")]
    for r in rows:
        if re.fullmatch(r"[\d.]+", r[0]) and len(r) == 6:
            v["load (s)"], v["on disk after compaction (MiB)"], v["open (ms)"] = float(r[0]), float(r[2]), float(r[4])
        for name in ("W1 Bedrock", "W2 10 min", "W3 blast radius, hub"):
            if r[0].startswith(name) and len(r) == 7:
                v[f"{name.split()[0]} SPARQL p50 (ms)"], v[f"{name.split()[0]} native p50 (ms)"] = float(r[2]), float(r[4])
    for key, pat in [("re-harvest, unchanged (ms)", r"unchanged: removed 0, added 0 in ([\d.]+) ms"),
                     ("re-harvest, 3,276 changed (ms)", r"changed timestamps: removed \d+, added \d+ in ([\d.]+) ms"),
                     ("re-harvest back (ms)", r"back to the original: removed \d+, added \d+ in ([\d.]+) ms")]:
        v[key] = float(re.search(pat, out).group(1))
    v["mismatches"] = float(re.search(r"correctness: (\d+) mismatches", out).group(1))
    return v

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("old"); ap.add_argument("new")
    ap.add_argument("--runs", type=int, default=5); ap.add_argument("--dir", default="target/store-ab")
    a = ap.parse_args()
    sides = {"old": a.old, "new": a.new}
    got = {k: [] for k in sides}
    last = {}
    for i in range(a.runs):
        for k in (("old", "new") if i % 2 == 0 else ("new", "old")):
            d = pathlib.Path(a.dir) / k
            p = subprocess.run([sides[k], "--scale", "1", "--chunk", "4000000", "--dir", str(d)], capture_output=True, text=True)
            assert p.returncode == 0, p.stdout[-2000:] + p.stderr[-2000:]
            got[k].append(parse(p.stdout)); last[k] = p.stdout
            for f in d.glob("*.redb"):
                f.unlink()
    print(f"store_bench --scale 1, {a.runs} runs per build, the builds taking turns; median (min to max)\n")
    print("| | old | new | new / old |\n|---|---|---|---|")
    for key in got["old"][0]:
        cell = {}
        for k in sides:
            xs = [r[key] for r in got[k]]
            cell[k] = (statistics.median(xs), min(xs), max(xs))
        ratio = cell["new"][0] / cell["old"][0] if cell["old"][0] else float("nan")
        fmt = lambda c: f"{c[0]:.3f} ({c[1]:.3f} to {c[2]:.3f})"
        print(f"| {key} | {fmt(cell['old'])} | {fmt(cell['new'])} | {ratio:.2f} |")
    print("\nlast run of the new build:\n")
    print(last["new"])

main()
