#!/usr/bin/env python3
"""Gate for `ograph chains INC --since T`: when an incident's ranked root causes moved since T,
it names every source file that moved them and no other.

The gate was written before the code. Each step edits, adds or removes exactly one source file
and harvests. Before the step and after it, plain `ograph chains --json` is saved: whether the
top 5 moved is read from those two outputs, not from the feature under test, and the file the
step touched is the responsible one by construction.

  Pass, per step: `chains --since t` (t taken just before the step) reports `then` equal to the
  saved ranking before and `now` equal to the ranking after; it names exactly the file the step
  touched when the ranking moved, and names nothing when it did not.

  Pass, per window of several steps: the files named, with their `needed` and `enough` marks,
  are the ones that rebuilding stores from scratch gives. For each file that differs between the
  window's start and now, a new store harvested from today's files with that one file as it was
  at the start says whether it is `needed`, and a new store from the start's files with that one
  file as it is today says whether it is `enough`. No history view takes part in that answer.

  The first design expected window names from the step design alone, assuming the steps did not
  interact. They do (`chain-delta-first-design.txt`): dropping the rank-1 application's metrics
  lowers its score from 1.000 to 0.858, which reorders nothing on its own but puts it below the
  two alerts of a1.json and a2.json when they are present. The window expectation is therefore
  computed by the rebuilt stores above, which also covers a2dup.json: it holds the same alert as
  a2.json, so with both present neither is `needed`, and each is `enough`.

Part 1 is a copy of examples/demo (run `python3 generate.py` there first) with no discussion
sources and no model. Part 2 is copies of RCAEval cases (the first --limit by id; the source tree
is only read), with an `alerts` directory source added:

  s1  add alerts/a1.json: a critical alert on Z, 60 s before the incident  (Z enters the top 5)
  s2  add alerts/old.json: the same alert on Z three days earlier          (outside every window)
  s3  edit ownership.jsonl: one more process for an application            (ownership ranks nothing)
  s4  add alerts/a2.json: a critical alert on Z3, 90 s before the incident (Z3 enters the top 5)
  s5  add alerts/a2dup.json: the same alert as a2.json                     (nothing new in the union)
  s6  edit alerts/a1.json: the alert moves three days earlier              (Z leaves)
  s7  remove alerts/old.json
  s8  remove alerts/a2.json                                                (a2dup.json still holds it)
  s9  remove alerts/a2dup.json                                             (Z3 leaves)
  s10 edit metrics.csv: drop the columns of the rank-1 application
  windows: since t0 after s5, since t5 after s10, since t0 after s10

Part 3 times `chains --since` on one store of all 90 cases after one metrics file changes, and
after ten do.

Usage: chain_delta_gate.py --bin target/release/ograph --work target/chain-delta
       --demo examples/demo --cases data/rcaeval-cases [--limit 6] [--merged]
"""
import argparse, datetime, json, pathlib, shutil, statistics, subprocess, time

DAY = 86_400


def now_iso():
    time.sleep(0.05)
    t = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")
    time.sleep(0.05)
    return t


class Ws:
    def __init__(self, binp, root, incident):
        self.binp, self.root, self.incident = binp, root, incident

    def og(self, *args, ok=(0, 1)):
        p = subprocess.run([self.binp, "--root", str(self.root), *args], capture_output=True, text=True)
        assert p.returncode in ok, f"ograph {' '.join(args)}: {p.returncode} {p.stderr[-400:]}"
        return p.stdout

    def harvest(self):
        self.og("harvest", ok=(0,))

    def ranking(self, *extra):
        out = json.loads(self.og("chains", self.incident, "--json", "--top", "5", *extra))
        return out

    def order(self):
        return [a["app"] for a in self.ranking()["apps"]]

    def shift(self, t):
        return self.ranking("--since", t)["shift"]


def graph(kind, rel):
    return f"g:harvest/{kind}/{rel}"


def check_step(ws, name, edit, kind, rel, rows):
    before = ws.order()
    t = now_iso()
    edit()
    ws.harvest()
    after = ws.order()
    s = ws.shift(t)
    moved = before != after
    named = {c["graph"] for c in s["causes"]}
    want = {graph(kind, rel)} if moved else set()
    then, now = [a["app"] for a in s["then"]], [a["app"] for a in s["now"]]
    ok = then == before and now == after and named == want
    rows.append((name, moved, sorted(named), ok))
    return t, ok, moved


def capture(root):
    return {str(f.relative_to(root)): f.read_bytes() for f in sorted(root.rglob("*")) if f.is_file() and ".ograph" not in f.parts and ".git" not in f.parts}


def fresh_order(ws, files, kinds):
    """The top 5 of a new store harvested from `files` alone: what the ranking is when the
    sources are exactly these, with no history involved."""
    d = ws.root.parent / "oracle"
    shutil.rmtree(d, ignore_errors=True)
    (d / ".ograph").mkdir(parents=True)
    shutil.copy(ws.root / ".ograph/config.toml", d / ".ograph/config.toml")
    for rel, data in files.items():
        (d / rel).parent.mkdir(parents=True, exist_ok=True)
        (d / rel).write_bytes(data)
    for rel in kinds:
        if rel.endswith("/"):
            (d / rel).mkdir(parents=True, exist_ok=True)
    if (ws.root / ".git").exists():
        subprocess.run(["git", "init", "-q"], cwd=d, check=True)
    o = Ws(ws.binp, d, ws.incident)
    o.harvest()
    return o.order()


def with_file(files, rel, other):
    out = dict(files)
    out.pop(rel, None)
    if rel in other:
        out[rel] = other[rel]
    return out


def check_window(ws, name, t, then_files, kinds, rows):
    """Names expected: for each file that differs between t and now, rebuild two stores from
    scratch. `needed`: today's files with this one as it was at t rank differently from today's
    files. `enough`: the files of t with this one as it is today rank differently from the files
    of t."""
    s = ws.shift(t)
    got = {c["graph"]: (c["needed"], c["enough"]) for c in s["causes"]}
    now_files = capture(ws.root)
    base_now, base_then = fresh_order(ws, now_files, kinds), fresh_order(ws, then_files, kinds)
    want = {}
    for rel in sorted(set(now_files) | set(then_files)):
        if now_files.get(rel) == then_files.get(rel):
            continue
        needed = fresh_order(ws, with_file(now_files, rel, then_files), kinds) != base_now
        enough = fresh_order(ws, with_file(then_files, rel, now_files), kinds) != base_then
        if base_now != base_then and (needed or enough):
            want[graph(kind_of(kinds, rel), rel)] = (needed, enough)
    then, now = [a["app"] for a in s["then"]], [a["app"] for a in s["now"]]
    ok = got == want and then == base_then and now == base_now == ws.order()
    named = [f"{g} ({'needed' if n else ''}{', ' if n and e else ''}{'enough' if e else ''})" for g, (n, e) in sorted(got.items())]
    rows.append((name, then != now, named, ok))
    if not ok:
        print(f"  {name}: expected {want}, then {base_then}, now {base_now}; got {got}, then {then}, now {now}")
    return ok


def kind_of(kinds, rel):
    return next(k for prefix, k in kinds.items() if rel == prefix or (prefix.endswith("/") and rel.startswith(prefix)))


def report(title, rows):
    print(f"\n{title}\n| step | top 5 moved (plain chains) | files named by --since | pass |\n|---|---|---|---|")
    for name, moved, named, ok in rows:
        print(f"| {name} | {'yes' if moved else 'no'} | {', '.join(named) or 'none'} | {'yes' if ok else 'NO'} |")
    return all(r[3] for r in rows)


def demo(binp, src, work):
    d = work / "demo"
    shutil.rmtree(d, ignore_errors=True)
    shutil.copytree(src, d, ignore=shutil.ignore_patterns(".ograph"))
    (d / ".ograph").mkdir()
    cfg = (src / ".ograph/config.toml").read_text()
    blocks = cfg.split("[[source]]")
    kept = [b for b in blocks[1:] if 'kind = "discussions"' not in b]
    tail = kept[-1].split("\n[", 1)
    kept[-1] = tail[0] + "\n"
    rest = "[" + tail[1] if len(tail) > 1 else ""
    rest = rest.split("[llm.extract]")[0]
    (d / ".ograph/config.toml").write_text("".join("[[source]]" + b for b in kept) + rest)
    subprocess.run(["git", "init", "-q"], cwd=d, check=True)
    ws = Ws(binp, d, "INC-5377")
    ws.harvest()
    rows, ok = [], True
    kinds = {"alerts.json": "alerts", "news.jsonl": "news", "deployments.jsonl": "deployments", "runbooks/": "docs", "config.jsonl": "config_changes"}
    alerts = json.loads((d / "alerts.json").read_text())
    f0 = capture(d)
    write = lambda f, text: (lambda: (d / f).write_text(text))
    lines = lambda f: (d / f).read_text().splitlines(keepends=True)
    without = lambda f, needle: "".join(l for l in lines(f) if needle not in l)

    t0, k, c1 = check_step(ws, "d1 edit alerts.json: drop the LedgerPoolSaturated alert before INC-5377", write("alerts.json", json.dumps({"alerts": [a for a in alerts["alerts"] if a["startsAt"] != 1795385980]})), "alerts", "alerts.json", rows)
    t1, k2, c2 = check_step(ws, "d2 edit news.jsonl: one more vendor item", write("news.jsonl", "".join(lines("news.jsonl")) + json.dumps({"at": 1795380000, "title": "MailRelay maintenance", "summary": "MailRelay plans maintenance", "mentions": ["MailRelay"]}) + "\n"), "news", "news.jsonl", rows)
    ok &= check_window(ws, "W1 since t0 after d2", t0, f0, kinds, rows)
    t2, k3, c3 = check_step(ws, "d3 edit deployments.jsonl: drop payments-api 2.3.1", write("deployments.jsonl", without("deployments.jsonl", "2.3.1")), "deployments", "deployments.jsonl", rows)
    f3 = capture(d)
    t3, k4, c4 = check_step(ws, "d4 edit runbooks/platform.md: one more line", write("runbooks/platform.md", (d / "runbooks/platform.md").read_text() + "\nCheck the dashboards first.\n"), "docs", "runbooks/platform.md", rows)
    t4, k5, c5 = check_step(ws, "d5 edit config.jsonl: drop the ledger-db max_connections change", write("config.jsonl", without("config.jsonl", "max_connections")), "config_changes", "config.jsonl", rows)
    ok &= check_window(ws, "W2 since t3 after d5", t3, f3, kinds, rows)
    t5, k6, c6 = check_step(ws, "d6 edit alerts.json: back as generated", write("alerts.json", json.dumps(alerts)), "alerts", "alerts.json", rows)
    ok &= k and k2 and k3 and k4 and k5 and k6
    return report("examples/demo, INC-5377", rows) and ok


def alert(app, at, name="GateAlert"):
    return json.dumps([{"labels": {"alertname": name, "service": app, "severity": "critical"}, "startsAt": at}])


def case(binp, src, work, cid, incident):
    d = work / cid
    shutil.rmtree(d, ignore_errors=True)
    (d / ".ograph").mkdir(parents=True)
    (d / "alerts").mkdir()
    for f in (src / cid).iterdir():
        if f.is_file():
            shutil.copy(f, d / f.name)
    (d / ".ograph/config.toml").write_text((src / cid / ".ograph/config.toml").read_text() + '\n[[source]]\nkind = "alerts"\npath = "alerts"\n')
    ws = Ws(binp, d, incident)
    ws.harvest()
    opened = json.loads((d / "incidents.json").read_text())[0]["opened"]
    base = ws.order()
    apps = sorted(r.split(",")[0] for r in ws.og("sparql", "SELECT ?n WHERE { ?a a ops:Application ; ops:name ?n }").splitlines()[1:])
    home = json.loads((d / "incidents.json").read_text())[0]["service"]
    z, z3 = [a for a in apps if a not in base and a != home][:2]
    rows, ok = [], True
    put = lambda rel, text: (lambda: (d / rel).write_text(text))
    rm = lambda rel: (lambda: (d / rel).unlink())
    kinds = {"alerts/": "alerts", "ownership.jsonl": "ownership", "metrics.csv": "metrics"}
    f0 = capture(d)

    t0, k1, c1 = check_step(ws, f"s1 add alerts/a1.json ({z}, 60 s before)", put("alerts/a1.json", alert(z, opened - 60)), "alerts", "alerts/a1.json", rows)
    t1, k2, c2 = check_step(ws, "s2 add alerts/old.json (3 days before)", put("alerts/old.json", alert(z, opened - 3 * DAY)), "alerts", "alerts/old.json", rows)
    t2, k3, c3 = check_step(ws, "s3 edit ownership.jsonl", put("ownership.jsonl", (d / "ownership.jsonl").read_text() + json.dumps({"app": base[-1], "process": "gate process"}) + "\n"), "ownership", "ownership.jsonl", rows)
    t3, k4, c4 = check_step(ws, f"s4 add alerts/a2.json ({z3}, 90 s before)", put("alerts/a2.json", alert(z3, opened - 90)), "alerts", "alerts/a2.json", rows)
    t4, k5, c5 = check_step(ws, "s5 add alerts/a2dup.json (same alert)", put("alerts/a2dup.json", alert(z3, opened - 90)), "alerts", "alerts/a2dup.json", rows)
    t5, f5 = now_iso(), capture(d)
    ok &= check_window(ws, "W1 since t0 after s5", t0, f0, kinds, rows)
    t6, k6, c6 = check_step(ws, "s6 edit alerts/a1.json (3 days before)", put("alerts/a1.json", alert(z, opened - 3 * DAY)), "alerts", "alerts/a1.json", rows)
    t7, k7, c7 = check_step(ws, "s7 remove alerts/old.json", rm("alerts/old.json"), "alerts", "alerts/old.json", rows)
    t8, k8, c8 = check_step(ws, "s8 remove alerts/a2.json", rm("alerts/a2.json"), "alerts", "alerts/a2.json", rows)
    t9, k9, c9 = check_step(ws, "s9 remove alerts/a2dup.json", rm("alerts/a2dup.json"), "alerts", "alerts/a2dup.json", rows)
    head = (d / "metrics.csv").read_text().splitlines()
    cols = [i for i, h in enumerate(head[0].split(",")) if i == 0 or h.split("_")[0] != base[0]]
    trimmed = "".join(",".join(r.split(",")[i] for i in cols) + "\n" for r in head)
    t10, k10, c10 = check_step(ws, f"s10 edit metrics.csv (drop {base[0]})", put("metrics.csv", trimmed), "metrics", "metrics.csv", rows)
    ok &= check_window(ws, "W2 since t5 after s10", t5, f5, kinds, rows)
    ok &= check_window(ws, "W3 since t0 after s10", t0, f0, kinds, rows)
    by_design = c1 and c4 and c6 and c9 and not (c2 or c3 or c5 or c7 or c8)
    print(f"\n{cid} ({incident}, top 5 before: {', '.join(base)}; Z = {z}, Z3 = {z3}); the steps moved the ranking as designed: {'yes' if by_design else 'NO'}")
    ok &= all([k1, k2, k3, k4, k5, k6, k7, k8, k9, k10])
    return report(f"{cid}", rows) and ok and by_design


def merged(binp, src, work, labels):
    """All cases in one store, as similar_eval.py builds it, from copies of the input files."""
    out = work / "merged"
    shutil.rmtree(out, ignore_errors=True)
    (out / ".ograph").mkdir(parents=True)
    first = src / sorted(labels)[0]
    catalog = [s for s in (first / ".ograph/config.toml").read_text().split("[[source]]") if 'kind = "catalog"' in s][0]
    cfg = "[[source]]" + catalog + f'[[source]]\nkind = "ownership"\npath = "{first}/ownership.jsonl"\n\n'
    for cid in sorted(labels):
        (out / cid).mkdir()
        for f in ("incidents.json", "metrics.csv", "logs.jsonl"):
            shutil.copy(src / cid / f, out / cid / f)
            kind = f.split(".")[0]
            cfg += f'[[source]]\nkind = "{kind}"\npath = "{cid}/{f}"\n\n'
    (out / ".ograph/config.toml").write_text(cfg + '[correlation]\nwindow_s = 1800\nrank = "chain"\n')
    t = time.perf_counter()
    Ws(binp, out, "").harvest()
    print(f"\nmerged store of {len(labels)} cases: first harvest {time.perf_counter() - t:.1f} s")
    return out


def timing(binp, src, work):
    """Each round takes T, keeps only the first tenth of one case's metrics rows, which moves
    that case's ranking, and cuts the last quarter of the metrics rows of n - 1 other cases,
    which leaves that incident's ranking alone but must be tested."""
    labels = json.loads((src / "labels.json").read_text())
    out = merged(binp, src, work, labels)
    ids = sorted(labels)

    def keep(cid, part):
        f = out / cid / "metrics.csv"
        rows = f.read_text().splitlines(keepends=True)
        f.write_text("".join(rows[: max(2, int(len(rows) * part))]))

    def clock(ws, *args, runs=5):
        ms = []
        for _ in range(runs):
            t = time.perf_counter()
            ws.og("chains", ws.incident, "--json", *args)
            ms.append((time.perf_counter() - t) * 1000)
        return statistics.median(ms), min(ms), max(ms)

    print("\none process per call, median of 5 (range)")
    print("| incident | files changed since T | graphs changed | ranking moved | causes named | chains (ms) | chains --since T (ms) |\n|---|---|---|---|---|---|---|")
    for r, n in enumerate((1, 10, 90)):
        ws = Ws(binp, out, labels[ids[r]]["incident"])
        t = now_iso()
        keep(ids[r], 0.1)
        for cid in [c for c in ids if c != ids[r]][: n - 1]:
            keep(cid, 0.75)
        ws.harvest()
        s = ws.shift(t)
        moved = [a["app"] for a in s["then"]] != [a["app"] for a in s["now"]]
        base, since = clock(ws), clock(ws, "--since", t)
        print(f"| {ws.incident} | {n} | {s['changed_graphs']} | {'yes' if moved else 'no'} | {', '.join(c['graph'] for c in s['causes']) or 'none'} | {base[0]:.0f} ({base[1]:.0f} to {base[2]:.0f}) | {since[0]:.0f} ({since[1]:.0f} to {since[2]:.0f}) |")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--demo")
    ap.add_argument("--cases")
    ap.add_argument("--limit", type=int, default=6)
    ap.add_argument("--merged", action="store_true")
    a = ap.parse_args()
    binp, work = str(pathlib.Path(a.bin).resolve()), pathlib.Path(a.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    ok = True
    if a.demo:
        ok &= demo(binp, pathlib.Path(a.demo).resolve(), work)
    if a.cases:
        src = pathlib.Path(a.cases).resolve()
        labels = json.loads((src / "labels.json").read_text())
        for cid in sorted(labels)[: a.limit]:
            ok &= case(binp, src, work, cid, labels[cid]["incident"])
        if a.merged:
            timing(binp, src, work)
    print("\nGATE PASSED" if ok else "\nGATE FAILED")
    raise SystemExit(0 if ok else 1)


if __name__ == "__main__":
    main()
