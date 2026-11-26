#!/usr/bin/env python3
import json, sys, math
a, b = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
labels = json.load(open(sys.argv[3])) if len(sys.argv) > 3 else {}
better = [c for c in a if b[c]["right"] and not a[c]["right"]]; worse = [c for c in a if a[c]["right"] and not b[c]["right"]]
n = len(better) + len(worse); k = min(len(better), len(worse))
p = min(1.0, 2 * sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n) if n else 1.0
print(f"A {sum(x['right'] for x in a.values())}/{len(a)}  B {sum(x['right'] for x in b.values())}/{len(b)}  better {len(better)} worse {len(worse)} p={p:.4f}")
types = sorted({x["type"] for x in a.values() if x["type"]})
for t in types: print(f"  {t}: A {sum(x['right'] for x in a.values() if x['type']==t)} B {sum(x['right'] for x in b.values() if x['type']==t)}")
for c in better: print("  better", c, a[c]["type"], a[c]["apps"][:3], "->", b[c]["apps"][:3])
for c in worse: print("  worse", c, a[c]["type"], a[c]["apps"][:3], "->", b[c]["apps"][:3])
