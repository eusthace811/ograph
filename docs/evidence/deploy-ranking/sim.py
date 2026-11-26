#!/usr/bin/env python3
import json, sys
def q(x): return round(x * 1e9)
def rank(d, on=True, first=False, delta=None, nochronic=False, needsev=None):
    apps = d["apps"]
    by = {}
    root_at = {r["app"]: r["chain"]["root"]["at_ms"] for r in apps if r.get("chain")}
    for r in apps:
        c = r.get("chain")
        if not c or not on: continue
        if nochronic and c["root"].get("chronic"): continue
        if needsev is not None and c["root"]["severity"] < needsev: continue
        path = [c["root"]] + c["links"]
        for k in range(1, len(path)):
            if path[k]["app_name"] == path[k-1]["app_name"]: continue
            if c["scores"][k-1]["time"] < 1.0: break
            y = path[k]["app_name"]
            if y == r["app"]: continue
            if first and y in root_at and path[k]["at_ms"] > root_at[y]: continue
            by.setdefault(y, []).append(r["app"])
    sc = {r["app"]: q(r["score"]) for r in apps}
    conf = {r["app"]: q(r["score"]) for r in apps}
    for _ in range(len(apps)):
        ch = False
        for y, xs in by.items():
            for x in xs:
                if delta is not None and conf[x] < sc[y] - q(delta): continue
                if sc[x] < sc[y]: sc[x] = sc[y]; ch = True
        if not ch: break
    def key(r):
        c = r.get("chain"); s = sc[r["app"]]
        under = any(sc[x] >= s and not (delta is not None and conf[x] < conf[r["app"]] - q(delta)) for x in by.get(r["app"], []))
        br = len({x["app_name"] for x in [c["root"]] + c["links"]}) if c else 0
        return (-s, under, -br, c["root"]["at_ms"] if c else 1 << 62, r["app"])
    return [r["app"] for r in sorted(apps, key=key)]
if __name__ == "__main__":
    pass

def explain_edges(d, first=True, needsev=None, nochronic=False, chain_known=False):
    apps = d["apps"]; by = []
    root_at = {r["app"]: r["chain"]["root"]["at_ms"] for r in apps if r.get("chain")}
    for r in apps:
        c = r.get("chain")
        if not c: continue
        if nochronic and c["root"].get("chronic"): continue
        if needsev is not None and c["root"]["severity"] < needsev: continue
        path = [c["root"]] + c["links"]
        for k in range(1, len(path)):
            if path[k]["app_name"] == path[k-1]["app_name"]: continue
            if c["scores"][k-1]["time"] < 1.0: break
            y = path[k]["app_name"]
            if y == r["app"]: continue
            if first and y in root_at and path[k]["at_ms"] > root_at[y]: continue
            by.append((r["app"], y))
    return by

def rank_move(d, **kw):
    order = rank(d, on=False)
    edges = explain_edges(d, **kw)
    for _ in range(len(order) * 2):
        ch = False
        for x, y in edges:
            if y not in order or x not in order: continue
            if order.index(x) > order.index(y):
                order.remove(x); order.insert(order.index(y), x); ch = True
        if not ch: break
    return order
