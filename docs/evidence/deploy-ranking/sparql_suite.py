import json, subprocess, sys
P, C, dp, dc, qs = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], json.load(open(sys.argv[5]))
same = 0
for q in qs:
    r = []
    for b, d in ((P, dp), (C, dc)):
        o = subprocess.run([b, "--json", "sparql", q["gold"]], cwd=d, capture_output=True, text=True).stdout
        r.append(json.loads(o)["rows"] if o.strip() else None)
    ok = r[0] == r[1] and bool(r[0])
    same += ok
    if not ok: print("DIFF", q["q"])
print(f"SPARQL suite: reference queries with identical non-empty rows under Parent and Candidate: {same}/{len(qs)}")
