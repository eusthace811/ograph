#!/usr/bin/env python3
"""Lab page gate: writes `ograph lab` for a workspace and checks the page against the CLI.
For every incident section: each `before` event has exactly one timeline mark, the top chain of
`chains` has exactly one arrow per link (root to each link, last link to the incident), and every
application in `blast` of the top cause and in `before` is a node of the neighbourhood.
Also prints the page size and bytes per incident.

Usage: lab_check.py --bin target/release/ograph --root WORKSPACE --out PAGE [--window 30m]
"""
import argparse, json, pathlib, re, subprocess, sys
from html.parser import HTMLParser


class Page(HTMLParser):
    def __init__(self):
        super().__init__(); self.sections = {}; self.cur = None; self.svg = None

    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag == "section":
            self.cur = self.sections.setdefault(a["data-incident"], {"marks": [], "hops": [], "hood": [], "ids": set(), "opens": []})
        if self.cur is None:
            return
        if tag == "svg":
            self.svg = a.get("class")
        if "id" in a:
            self.cur["ids"].add(a["id"])
        if "data-open" in a:
            self.cur["opens"].append(a["data-open"])
        if self.svg == "timeline" and tag == "rect" and "data-ev" in a:
            self.cur["marks"].append(a["data-ev"])
        if self.svg == "timeline" and tag == "path" and "data-hop" in a:
            self.cur["hops"].append(int(a["data-hop"]))
        if self.svg == "hood" and tag == "g" and "data-app" in a:
            self.cur["hood"].append(a["data-app"])

    def handle_endtag(self, tag):
        if tag == "svg":
            self.svg = None
        if tag == "section":
            self.cur = None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--root", required=True); ap.add_argument("--out", required=True); ap.add_argument("--window", default="30m")
    a = ap.parse_args()

    def og(*x):
        r = subprocess.run([a.bin, "--root", a.root, "--json", *x], capture_output=True, text=True)
        if r.returncode not in (0, 1):
            sys.exit(f"ograph {' '.join(x)} failed: {r.stderr.strip()[-300:]}")
        return json.loads(r.stdout) if r.stdout.strip() else None

    og("lab", "--out", a.out)
    text = pathlib.Path(a.out).read_text()
    p = Page(); p.feed(text)
    failures = []; n_marks = n_hops = n_hood = 0
    for inc, sec in sorted(p.sections.items()):
        local = inc.rsplit("/", 1)[-1]
        before = og("before", local, "--window", a.window)
        want = [e["event"] for e in before["events"]]
        for e in want:
            if sec["marks"].count(e) != 1:
                failures.append(f"{local}: event {e} has {sec['marks'].count(e)} marks")
        if sorted(sec["marks"]) != sorted(want):
            failures.append(f"{local}: {len(sec['marks'])} marks for {len(want)} before events")
        chains = og("chains", local)
        top = chains["apps"][0] if chains and chains["apps"] else None
        hops = len(top["chain"]["links"]) + 1 if top and top.get("chain") else 0
        if sorted(sec["hops"]) != list(range(hops)):
            failures.append(f"{local}: arrows {sorted(sec['hops'])} for a top chain of {hops} links")
        apps = {e["app"] for e in before["events"]} | {before["app"]}
        if top:
            blast = og("blast", top["app"])
            apps |= {d[0] for d in blast["dependents"]} | {blast["app"]}
        missing = apps - set(sec["hood"])
        if missing:
            failures.append(f"{local}: neighbourhood lacks {sorted(missing)}")
        if len(sec["hood"]) != len(set(sec["hood"])):
            failures.append(f"{local}: an application is drawn twice in the neighbourhood")
        dangling = [o for o in sec["opens"] if o not in sec["ids"]]
        if dangling:
            failures.append(f"{local}: {len(dangling)} clicks open nothing, e.g. {dangling[0]}")
        n_marks += len(want); n_hops += hops; n_hood += len(apps)
    n = len(p.sections)
    print(f"{a.out}: {len(text.encode())} bytes, {n} incidents, {len(text.encode()) // max(n, 1)} bytes per incident")
    print(f"checked {n_marks} before events, {n_hops} top-chain links and {n_hood} blast and before applications")
    for f in failures[:20]:
        print("FAIL", f)
    print(f"{len(failures)} failures")
    sys.exit(1 if failures or not n else 0)


main()
