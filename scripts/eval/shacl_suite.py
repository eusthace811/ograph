import sys, subprocess, re, pathlib
binp, root = sys.argv[1], pathlib.Path(sys.argv[2])
files = sorted(p for p in root.rglob("*.ttl") if "sht:Validate" in p.read_text())
if not files:
    sys.exit(f"no SHACL test cases under {root}")
ok = bad = err = 0
for f in files:
    t = f.read_text()
    m = re.search(r'sh:conforms\s+"?(true|false)', t)
    exp = m.group(1) if m else "?"
    nexp = len(re.findall(r'(?:\ba|rdf:type)\s+sh:ValidationResult', t))
    try:
        out = subprocess.run([binp, str(f)], capture_output=True, text=True, timeout=30)
        r = re.search(r'conforms=(true|false) results=(\d+)', out.stdout)
    except subprocess.TimeoutExpired:
        r = None; out = None
    rel = f.relative_to(root)
    if not r:
        err += 1; msg = (out.stderr.strip().splitlines() or ["timeout"]) if out else ["timeout"]
        print("ERR ", rel, [l for l in msg if "panicked" in l or "Error" in l][:1] or msg[-1:])
    elif r.group(1) == exp and int(r.group(2)) == nexp: ok += 1
    else: bad += 1; print("FAIL", rel, f"exp={exp}/{nexp} got={r.group(1)}/{r.group(2)}")
print(f"total={len(files)} pass={ok} fail={bad} err={err}")
sys.exit(0 if ok == len(files) else 1)
