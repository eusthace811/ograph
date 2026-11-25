#!/usr/bin/env python3
"""Gate for `--known-at` and `ograph changes`: answers as of an earlier time are the answers
ograph gave at that time, byte for byte, and the history costs nothing while nothing changes.

Part 1, a scripted sequence in --work (its own git repository, so ogrep reads its files):
  catalog v1 (checkout -> payments-api -> ledger-db, fraud-api apart), an incident on checkout,
  alerts on payments-api and fraud-api, ledger-db supporting the billing process. Harvest, note
  the time t1, save what each command prints. Three more harvests of the same files. Then
  catalog v2 adds checkout -> fraud-api and the ownership file moves ledger-db to settlement.
  Pass: every saved output differs from the command's output now and equals its output with
  `--known-at t1`; `changes --since t1` lists exactly those two changes; the unchanged harvests
  grow the store file by 0 bytes; a time before the store existed is refused.

Part 2, with --cases (a copy of the RCAEval cases that this script may write to; --cases-from
SRC first builds it as links to the files of SRC, which stays untouched):
  for each case, harvest, save `chains` in both rank modes, empty the metrics and traces files,
  harvest again, and compare `chains --known-at t1` with what was saved.

Usage: known_at_gate.py --bin target/release/ograph --work target/known-at-gate
       [--cases DIR [--cases-from SRC]] [--limit N] [--jobs 3]
"""
import argparse, concurrent.futures as cf, datetime, json, pathlib, shutil, statistics, subprocess, time

T = 1_795_386_400

def run(binp, cwd, *args):
    p = subprocess.run([binp, *args], cwd=cwd, capture_output=True, text=True)
    return p.returncode, p.stdout, p.stderr

def now_iso():
    time.sleep(0.05)
    t = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
    time.sleep(0.05)
    return t

def compose(extra):
    return f"""services:
  checkout:
    image: shop/checkout
    depends_on:
      - payments-api{extra}
  payments-api:
    image: shop/payments
    depends_on:
      - ledger-db
  ledger-db:
    image: postgres
  fraud-api:
    image: shop/fraud
"""

def ownership(ledger):
    rows = [("checkout", "billing"), ("payments-api", "settlement"), ("ledger-db", ledger)]
    return "".join(json.dumps({"app": a, "process": p}) + "\n" for a, p in rows)

def alert(app, name, at):
    return {"labels": {"alertname": name, "service": app, "severity": "critical"}, "startsAt": at}

def history_rows(binp, w):
    stats = json.loads(run(binp, w, "stats", "--json")[1])[1]
    rows = {name: n for name, n, _, _ in stats if name in ("born", "added", "history")}
    rows["table bytes"] = sum(stored + over for name, _, stored, over in stats if name in ("born", "added", "history"))
    return rows

def scripted(binp, w):
    shutil.rmtree(w, ignore_errors=True); w.mkdir(parents=True)
    subprocess.run(["git", "init", "-q"], cwd=w, check=True)
    before_store = now_iso()
    assert run(binp, w, "init")[0] == 0
    (w / ".ograph/config.toml").write_text("".join(f'[[source]]\nkind = "{k}"\npath = "{p}"\n' for k, p in
        [("catalog", "."), ("ownership", "ownership.jsonl"), ("incidents", "incidents.json"), ("alerts", "alerts.json")]))
    (w / "docker-compose.yml").write_text(compose(""))
    (w / "ownership.jsonl").write_text(ownership("billing"))
    (w / "incidents.json").write_text(json.dumps([{"id": "INC-1", "service": "checkout", "opened": T, "severity": "SEV1", "status": "open"}]))
    (w / "alerts.json").write_text(json.dumps([alert("payments-api", "ErrorRate", T - 300), alert("fraud-api", "ScoreTimeouts", T - 600)]))
    store = w / ".ograph/store.redb"
    rc, out, err = run(binp, w, "harvest")
    assert rc == 0, err
    print("harvest of v1:\n" + "".join("  " + l.split(" (")[0] + "\n" for l in out.splitlines()[:-1]), end="")
    commands = [
        ["chains", "INC-1"], ["chains", "INC-1", "--json"], ["chains", "INC-1", "--rank", "propagation", "--json"],
        ["before", "INC-1", "--window", "30m"], ["before", "INC-1", "--window", "30m", "--json"],
        ["blast", "fraud-api"], ["blast", "ledger-db", "--json"],
        ["sparql", "SELECT ?a ?b WHERE { ?a ops:dependsOn ?b } ORDER BY ?a ?b"],
        ["sparql", "SELECT ?p WHERE { id:app\\/ledger-db ops:supports ?p }", "--json"],
        ["sparql", "SELECT ?g ?at WHERE { GRAPH ?g { ?g harv:at ?at ; harv:source \"catalog\" } }"],
    ]
    saved = [run(binp, w, *c)[:2] for c in commands]
    t1 = now_iso()
    ok = True

    sizes = [store.stat().st_size]
    for _ in range(3):
        assert run(binp, w, "harvest")[0] == 0
        sizes.append(store.stat().st_size)
    growth = [b - a for a, b in zip(sizes, sizes[1:])]
    rows = history_rows(binp, w)
    print(f"\nstore after v1: {sizes[0]} bytes; growth of 3 harvests of the same files: {growth} bytes; history rows {rows}")
    ok &= growth == [0, 0, 0] and rows["added"] == 0 and rows["history"] == 0

    (w / "docker-compose.yml").write_text(compose("\n      - fraud-api"))
    (w / "ownership.jsonl").write_text(ownership("settlement"))
    rc, out, err = run(binp, w, "harvest")
    assert rc == 0, err
    print("\nharvest of v2:\n" + "".join("  " + l.split(" (")[0] + "\n" for l in out.splitlines()[:-1]), end="")
    after = store.stat().st_size
    rows = history_rows(binp, w)
    print(f"store after v2: {after} bytes ({after - sizes[-1]:+d}, in pages of the file's allocator); history rows {rows}")

    print(f"\n| command | now differs from t1 | --known-at t1 equals t1 |\n|---|---|---|")
    for c, (rc0, out0) in zip(commands, saved):
        rc_now, out_now, _ = run(binp, w, *c)
        rc_then, out_then, err_then = run(binp, w, *c, "--known-at", t1)
        differs, equal = (rc_now, out_now) != (rc0, out0), (rc_then, out_then) == (rc0, out0)
        ok &= differs and equal
        print(f"| `{' '.join(c)}` | {'yes' if differs else 'NO'} | {'yes' if equal else 'NO'} ({len(out0)} bytes, exit {rc0}) |")
    print("\nstandard error of the last one:\n  " + err_then.strip())

    rc, out, err = run(binp, w, "changes", "--since", t1, "--json")
    got = [(c["change"], c["subject"], c["predicate"], c["object"], c.get("was"), c["source"]) for c in json.loads(out)]
    want = [("added", "id:app/checkout", "ops:dependsOn", "id:app/fraud-api", None, "catalog"),
            ("changed", "id:app/ledger-db", "ops:supports", "id:process/settlement", "id:process/billing", "ownership")]
    ok &= got == want
    print(f"\n`changes --since t1`: {len(got)} listed, {'exactly the two made' if got == want else 'NOT the two made: ' + repr(got)}")
    print("".join("  " + l + "\n" for l in run(binp, w, "changes", "--since", t1)[1].splitlines()), end="")
    rc, out, _ = run(binp, w, "changes", "--since", now_iso())
    ok &= (rc, out) == (1, "")
    print(f"`changes --since now`: {len(out.splitlines())} listed, exit {rc}")

    rc, out, err = run(binp, w, "chains", "INC-1", "--known-at", before_store)
    ok &= rc == 2 and out == "" and "no record of what was known at" in err
    print(f"\na time before the store existed: exit {rc}, {err.strip()[:60]}...")

    # Retention: with history_days = 0 compact keeps nothing, and t1 is then refused.
    cfg = w / ".ograph/config.toml"
    cfg.write_text("history_days = 0\n" + cfg.read_text())
    rc, out, _ = run(binp, w, "compact", "--json")
    pruned = json.loads(out)["history_rows_pruned"]
    rows_after = history_rows(binp, w)
    rc2, _, err = run(binp, w, "chains", "INC-1", "--known-at", t1)
    ok &= pruned == rows["added"] + rows["history"] and rows_after["added"] == rows_after["history"] == 0 and rc2 == 2
    print(f"compact with history_days = 0: dropped {pruned} rows, {rows_after} left; --known-at t1 then exits {rc2}")
    return ok

def one_case(binp, d, incident):
    store = d / ".ograph/store.redb"
    store.unlink(missing_ok=True)
    assert run(binp, d, "harvest")[0] == 0
    modes = [["chains", incident, "--json", "--top", "5", "--rank", m] for m in ("chain", "propagation")]
    saved = [run(binp, d, *c)[:2] for c in modes]
    size1 = store.stat().st_size
    assert run(binp, d, "harvest")[0] == 0
    idle = store.stat().st_size - size1
    t1 = now_iso()
    quads = json.loads(run(binp, d, "stats", "--json")[1])[1]
    quads = next(n for name, n, _, _ in quads if name == "gspo")
    for name in ("metrics.csv", "traces.jsonl"):
        f = d / name
        if not f.exists():
            continue
        head = f.open().readline() if name.endswith(".csv") else ""
        f.unlink(); f.write_text(head)
    assert run(binp, d, "harvest")[0] == 0
    size2 = store.stat().st_size
    rows = history_rows(binp, d)
    now = [run(binp, d, *c)[:2] for c in modes]
    then = [run(binp, d, *c, "--known-at", t1)[:2] for c in modes]
    return {"equal": then == saved, "differs": now != saved, "idle": idle, "quads": quads, "history": rows["history"] + rows["added"],
            "table": rows["table bytes"], "growth": size2 - size1, "size": size1, "dir": d, "chains": modes[0], "t1": t1}

def latency(binp, res, runs=5):
    """One process per call, the two forms taking turns, after the harvests are over."""
    ms = {"now": [], "known-at": []}
    for r in res:
        for i in range(runs):
            for k in (("now", "known-at") if i % 2 == 0 else ("known-at", "now")):
                t = time.perf_counter()
                run(binp, r["dir"], *r["chains"], *(["--known-at", r["t1"]] if k == "known-at" else []))
                ms[k].append((time.perf_counter() - t) * 1000)
    return {k: (statistics.median(v), min(v), max(v)) for k, v in ms.items()}

def mirror(src, dst):
    shutil.rmtree(dst, ignore_errors=True); dst.mkdir(parents=True)
    shutil.copy(src / "labels.json", dst / "labels.json")
    for case in json.loads((src / "labels.json").read_text()):
        (dst / case / ".ograph").mkdir(parents=True)
        shutil.copy(src / case / ".ograph/config.toml", dst / case / ".ograph/config.toml")
        for f in (src / case).iterdir():
            if f.name != ".ograph":
                (dst / case / f.name).symlink_to(f)

def cases(binp, root, limit, jobs):
    labels = json.loads((root / "labels.json").read_text())
    ids = sorted(labels)[:limit]
    with cf.ThreadPoolExecutor(jobs) as ex:
        res = list(ex.map(lambda c: one_case(binp, root / c, labels[c]["incident"]), ids))
    equal, differs = sum(r["equal"] for r in res), sum(r["differs"] for r in res)
    med = lambda k: statistics.median(r[k] for r in res)
    print(f"\nRCAEval, {len(res)} cases, metrics and traces emptied after t1:")
    print(f"  chains --known-at t1 equal to the chains saved at t1, both rank modes: {equal} of {len(res)}")
    print(f"  chains now differ from t1: {differs} of {len(res)}")
    print(f"  store growth of a harvest of the same files: max {max(r['idle'] for r in res)} bytes")
    print(f"  medians per case: {med('quads'):.0f} quads in a {med('size'):.0f}-byte store file at t1; after the re-harvest {med('history'):.0f} rows in `added` and `history`, {med('table'):.0f} bytes in the history tables, file size {med('growth'):+.0f} bytes")
    lat = latency(binp, res[:20])
    print(f"  one `chains --json` process on each of {min(len(res), 20)} cases, 5 runs each, the two forms taking turns; median (min to max) ms:")
    for k, (m, lo, hi) in lat.items():
        print(f"    {k:<9} {m:.1f} ({lo:.1f} to {hi:.1f})")
    return equal == len(res) and max(r["idle"] for r in res) == 0

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--work", required=True)
    ap.add_argument("--cases"); ap.add_argument("--cases-from"); ap.add_argument("--limit", type=int, default=1000); ap.add_argument("--jobs", type=int, default=3)
    a = ap.parse_args()
    binp = str(pathlib.Path(a.bin).resolve())
    ok = scripted(binp, pathlib.Path(a.work).resolve())
    if a.cases_from:
        mirror(pathlib.Path(a.cases_from).resolve(), pathlib.Path(a.cases).resolve())
    if a.cases:
        ok &= cases(binp, pathlib.Path(a.cases).resolve(), a.limit, a.jobs)
    print("\nGATE " + ("PASSED" if ok else "FAILED"))
    raise SystemExit(0 if ok else 1)

main()
