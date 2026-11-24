#!/usr/bin/env python3
"""Measure the second set of ogrep integrations on a real repository (see code_eval.py for the first).

The repository, its history, its tags and its log statements are real (the OpenTelemetry demo
by default). The incidents, logs and deployment records are synthetic, each built from one real
statement, commit or pair of tags, so the right answer is known.

  H  introducing commit   `ograph code` names the commit that first added a log message;
                          the truth is `git log -S`
  I  new message          one or two error lines of a message the service never wrote: the
                          burst detector misses them, the new-message detector should not
  J  deployment files     a deployment whose version and the version before it are tags:
                          its changed files against `git log P..V`, and `ograph code`
  K  hand-off             `ogrep --related-tests` and `ogrep --callers -w`, which the skill
                          tells an agent to run on a located line

J needs an ogrep that reads revisions as the bounds of `--timeline` (0.3 or newer).

Usage: code_history_eval.py --bin target/release/ograph --repo PATH --work DIR [--seed N]
"""
import argparse, collections, json, pathlib, random, re, shutil, statistics, sys, time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from code_eval import HOLE, sh, iso, statements, catalog, init  # noqa: E402


def render(literal, k=0):
    """The message a statement writes, with values that differ from line to line as real ones do."""
    values = iter([str(40 + 7 * k), f"user-{k + 3}", f"{k + 1}.5", ("eu-west", "us-east", "ap-south")[k % 3], str(17 + k), f"id{k}c"] * 4)
    text = HOLE.sub(lambda _: next(values), literal)
    return re.sub(r"\\[nrt]", " ", text).replace('\\"', '"').replace("\\'", "'").strip()


def quiet(svc, t, minutes):
    """`minutes` of ordinary lines from `svc`, ending at `t`."""
    return [{"ts": iso(t - minutes * 60 + k * 10), "service": svc, "level": "info", "message": f"request handled id={1000 + k} in {20 + k % 7}ms"} for k in range(minutes * 6)]


def write(path, rows):
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


def first_commit(repo, literal, until):
    """The oldest commit until `until` that changed how often `literal` occurs: (sha, time)."""
    out = sh(["git", "log", "--format=%H %ct", f"--until={until}", f"-S{literal}"], repo).stdout.split("\n")
    rows = [l.split() for l in out if l.strip()]
    return min(rows, key=lambda r: int(r[1])) if rows else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--repo", required=True); ap.add_argument("--work", required=True)
    ap.add_argument("--seed", type=int, default=7); ap.add_argument("--cases", type=int, default=40)
    a = ap.parse_args()
    bin_ = str(pathlib.Path(a.bin).resolve())
    work = pathlib.Path(a.work).resolve()
    shutil.rmtree(work, ignore_errors=True)
    root = work / "main"
    root.mkdir(parents=True)
    shop = root / "shop"
    sh(["git", "clone", "-q", str(pathlib.Path(a.repo).resolve()), str(shop)])
    head, head_t = sh(["git", "log", "-1", "--format=%H %ct"], shop).stdout.split()
    head_t = int(head_t)
    rnd = random.Random(a.seed)
    ids, deps = catalog(shop)
    stmts = statements(shop, ids)
    print(f"repository {sh(['git', 'config', '--get', 'remote.origin.url'], pathlib.Path(a.repo)).stdout.strip()} at {head[:12]}: {len(ids)} services, {len(stmts)} unique log statements")
    print(f"ogrep {sh(['ogrep', '--version']).stdout.strip()}; seed {a.seed}\n")
    sh(["ogrep", "index", "."], shop)
    t = time.perf_counter()
    built = sh(["ogrep", "index", "--history", "."], shop)
    t_history = time.perf_counter() - t
    size = sum(f.stat().st_size for f in (shop / ".ogrep" / "history").rglob("*") if f.is_file()) if (shop / ".ogrep" / "history").exists() else 0
    print(f"`ogrep index --history` took {t_history:.1f} s and holds {size / 1e6:.0f} MB for {sh(['git', 'rev-list', '--count', 'HEAD'], shop).stdout.strip()} commits (exit {built.returncode}).\n")

    (root / "logs").mkdir()
    incidents, deployments = [], []

    # H: a burst of each statement within the last week; the message was added long before.
    h_cases = []
    for i, (f, n, lit) in enumerate(stmts):
        svc, t = f.split("/")[1], head_t - 3600 - i * 7200
        write(root / "logs" / f"H-{i:03}.jsonl", quiet(svc, t, 5) + [{"ts": iso(t + k * 2), "service": svc, "level": "error", "message": render(lit, k)} for k in range(8)])
        incidents.append({"id": f"INC-H-{i:03}", "title": f"{svc} errors", "service": svc, "opened": t + 120, "severity": "SEV2", "status": "open"})
        h_cases.append({"id": f"INC-H-{i:03}", "file": f"shop/{f}", "truth": first_commit(shop, lit, t + 120), "opened": t + 120})

    # I: forty quiet minutes, then one line (odd cases) or two (even) of a message never written before.
    t_i = head_t - 40 * 86400
    i_cases = []
    for i, (f, n, lit) in enumerate(stmts):
        svc, t = f.split("/")[1], t_i + i * 7200
        lines = [{"ts": iso(t + k * 20), "service": svc, "level": "error", "message": render(lit, k)} for k in range(1 + (i % 2 == 0))]
        write(root / "logs" / f"I-{i:03}.jsonl", quiet(svc, t, 40) + lines)
        incidents.append({"id": f"INC-I-{i:03}", "title": f"{svc} degraded", "service": svc, "opened": t + 120, "severity": "SEV2", "status": "open"})
        i_cases.append({"id": f"INC-I-{i:03}", "key": f"logs/I-{i:03}.jsonl", "file": f"shop/{f}", "line": n})
    # Controls: the same kind of message, but written every five minutes from the start, so it is not new.
    c_cases = []
    for i, (f, n, lit) in enumerate(stmts[: a.cases]):
        svc, t = f.split("/")[1], t_i - 86400 * 20 + i * 7200
        old = [{"ts": iso(t - 2400 + k * 300), "service": svc, "level": "error", "message": render(lit, k)} for k in range(9)]
        write(root / "logs" / f"N-{i:03}.jsonl", sorted(quiet(svc, t, 40) + old, key=lambda r: r["ts"]))
        c_cases.append(f"logs/N-{i:03}.jsonl")

    # J: for a service and two consecutive release tags that differ in it, a deployment of each.
    tags = [x for x in sh(["git", "tag", "--sort=creatordate"], shop).stdout.split() if re.fullmatch(r"v?\d+\.\d+\.\d+", x)]
    by_file = collections.defaultdict(list)
    for f, n, lit in stmts:
        by_file[f].append((n, lit))
    pool = []
    for p, v in zip(tags, tags[1:]):
        changed = collections.defaultdict(set)
        for f in sh(["git", "log", "--format=", "--name-only", f"{p}..{v}", "--", "src"], shop).stdout.split("\n"):
            if f.count("/") >= 2 and f.split("/")[1] in ids:
                changed[f.split("/")[1]].add(f)
        for svc, files in sorted(changed.items()):
            pool.append({"p": p, "v": v, "svc": svc, "truth": sorted(files), "stmt": next(((f, *by_file[f][0]) for f in sorted(files) if f in by_file), None)})
    rnd.shuffle(pool)
    pool.sort(key=lambda c: c["stmt"] is None)
    j_cases = pool[: a.cases]
    t_j = head_t - 120 * 86400
    for i, c in enumerate(j_cases):
        t = t_j + i * 14400
        deployments += [{"service": c["svc"], "version": c["p"], "at": t - 3600}, {"service": c["svc"], "version": c["v"], "at": t}]
        if c["stmt"]:
            f, n, lit = c["stmt"]
            write(root / "logs" / f"J-{i:03}.jsonl", quiet(c["svc"], t + 600, 5) + [{"ts": iso(t + 600 + k * 2), "service": c["svc"], "level": "error", "message": render(lit, k)} for k in range(8)])
        incidents.append({"id": f"INC-J-{i:03}", "title": f"{c['svc']} errors after a deployment", "service": c["svc"], "opened": t + 720, "severity": "SEV2", "status": "open"})
        c.update(id=f"INC-J-{i:03}", at=t)

    (root / "incidents.json").write_text(json.dumps(incidents))
    write(root / "deployments.jsonl", deployments)
    sources = [("catalog", "shop", ""), ("code", "shop", ""), ("deployments", "deployments.jsonl", ""), ("logs", "logs", ""), ("incidents", "incidents.json", "")]
    ws = init(bin_, root, sources)
    off_root = work / "off"
    off_root.mkdir()
    for n in ("logs", "incidents.json", "deployments.jsonl", "shop"):
        (off_root / n).symlink_to(root / n)
    off = init(bin_, off_root, sources, "[detect]\nlog_new_after_s = 0\n")
    for w in (ws, off):
        secs, p = w.harvest()
        assert p.returncode == 0, p.stderr
        print(f"harvest {'with' if w is ws else 'without'} the new-message detector: {secs:.2f} s; {p.stdout.strip().splitlines()[-1]}")
    for line in sorted({l.strip() for l in sh([bin_, "harvest"], root).stdout.splitlines() if "DEGRADED" in l.upper()})[:5]:
        print(f"  {line[:300]}")
    rc, problems = ws.run("validate")
    print(f"`ograph validate`: {len(problems) if isinstance(problems, list) else problems} violations\n")

    # H
    n = len(h_cases)
    located = same = older = none = absent = 0
    days, lat, other = [], [], []
    for c in h_cases:
        t = time.perf_counter()
        rc, out = ws.run("code", c["id"])
        lat.append((time.perf_counter() - t) * 1000)
        locs = out.get("locations", [])
        located += bool(locs)
        intro = next((l["introduced"] for l in locs if l.get("introduced")), None)
        if intro is None:
            none += 1
            absent += c["truth"] is None
        elif c["truth"] and intro["revision"] == c["truth"][0]:
            same += 1
            days.append(intro["days_before"])
        elif not c["truth"] or intro["at"] < iso(int(c["truth"][1])):
            older += 1
            days.append(intro["days_before"])
        else:
            other.append((c["id"], intro["revision"][:10], c["truth"][0][:10]))
    print("## H. The commit that first added a message\n")
    print(f"{n} incidents within the last week, each a burst of one real log message; a line of code was located for {located}.")
    print(f"`ograph code` names the commit `git log -S` gives for the whole statement for {same}/{n} = {same / n:.2f}. For {older} more it names an older commit: the constant part of the message was in the code before the statement got its present wording. It names none for {none}, of which {absent} are right: the statement was added after the incident's time, so git has none either. It names a later commit for {len(other)}.")
    if days:
        print(f"The commits were made a median of {statistics.median(days):.0f} days before the incident (shortest {min(days):.1f}); {sum(d * 1440 > 30 for d in days)} of {len(days)} are older than the 30-minute window `commits` covers.")
    print(f"`ograph code` took a median of {statistics.median(lat):.0f} ms (p90 {sorted(lat)[int(len(lat) * 0.9)]:.0f} ms) with the history lookups.")
    for w in other[:6]:
        print(f"  later than git: {w}")
    print()

    # I
    def new_events(w):
        rows = w.rows('SELECT ?r ?s ?l WHERE { ?e evt:sourceRecord ?r ; evt:signal ?s . OPTIONAL { ?e evt:codeLocation ?l } FILTER(?s = "LOG_NEW" || ?s = "LOG_ERROR") }')
        out = collections.defaultdict(list)
        for r, s, *l in rows:
            out[r.split("@")[0]].append((s, l[0] if l else None))
        return out
    on, without = new_events(ws), new_events(off)
    n = len(i_cases)
    found = sum(any(s == "LOG_NEW" for s, _ in on.get(c["key"], [])) for c in i_cases)
    burst = sum(any(s == "LOG_ERROR" for s, _ in on.get(c["key"], [])) for c in i_cases)
    right_file = sum(any(l and l.rsplit(":", 1)[0] == c["file"] for _, l in on.get(c["key"], [])) for c in i_cases)
    was = sum(bool(without.get(c["key"])) for c in i_cases)
    seen = lambda w: sum(bool(w.run("before", c["id"])[1].get("events")) for c in i_cases)
    print("## I. A message seen once or twice\n")
    print(f"{n} cases: forty quiet minutes of one service, then one line (half the cases) or two of a real error message it had never written, and an incident two minutes later.")
    print(f"Without the detector (`log_new_after_s = 0`) {was}/{n} have any log event and `ograph before` shows an event for {seen(off)}/{n}.")
    print(f"With it, {found}/{n} = {found / n:.2f} have a LOG_NEW event ({burst} also a burst), `ograph before` shows an event for {seen(ws)}/{n}, and the event points at the right file for {right_file}/{n} = {right_file / n:.2f}.")
    false = sum(any(s == "LOG_NEW" for s, _ in on.get(k, [])) for k in c_cases)
    quiet_h = sum(any(s == "LOG_NEW" for s, _ in on.get(f"logs/H-{i:03}.jsonl", [])) for i in range(len(h_cases)))
    print(f"Controls: {len(c_cases)} logs where the same kind of message recurs every five minutes from the start give {false} LOG_NEW events; the {len(h_cases)} bursts of H, which the burst detector already reports, give {quiet_h}.\n")

    # J
    files = collections.defaultdict(set)
    for e, f in ws.rows("SELECT ?e ?f WHERE { ?e a evt:Deployment ; evt:changedFile ?f }"):
        files[e].add(f)
    ver = {(app.rsplit("/", 1)[-1], v, at): e for e, app, v, at in ws.rows("SELECT ?e ?a ?v ?t WHERE { ?e a evt:Deployment ; evt:on ?a ; evt:version ?v ; evt:at ?t }")}
    n = len(j_cases)
    have = full = capped = listed = touched = with_stmt = 0
    recall, extra = [], []
    for c in j_cases:
        got = files.get(ver.get((c["svc"], c["v"], iso(c["at"]))), set())
        truth = {f"shop/{f}" for f in c["truth"]}
        have += bool(got)
        if got:
            recall.append(len(got & truth) / len(truth))
            extra.append(len(got - truth) / len(got))
            full += truth <= got
            capped += len(got) == 200 and not truth <= got
        if c["stmt"]:
            with_stmt += 1
            rc, out = ws.run("code", c["id"])
            deps_ = out.get("deployments", [])
            listed += any(d["version"] == c["v"] for d in deps_)
            touched += bool(deps_) and deps_[0]["version"] == c["v"] and any(x.rsplit(":", 1)[0] == f"shop/{c['stmt'][0]}" for x in deps_[0]["touches"])
    print("## J. What a deployment changed\n")
    print(f"{n} cases from {len(tags)} release tags: service S deployed at tag P, then at the next tag V, where `git log P..V -- src/S` lists files ({with_stmt} with a file that holds one of the log statements, whose burst follows the deployment by ten minutes).")
    print(f"The deployment of V carries changed files for {have}/{n}; every file git lists for the service is among them for {full}/{n} ({capped} more are cut at the 200-file limit).")
    if recall:
        print(f"Mean share of git's files that the deployment lists: {statistics.mean(recall):.2f}. Mean share of its files outside `src/S` (commits that also touched other paths): {statistics.mean(extra):.2f}.")
    print(f"`ograph code` lists the deployment for {listed}/{with_stmt} of the incidents with a statement and puts it first with the located file among its changes for {touched}/{with_stmt}.\n")

    # K
    tracked = sh(["git", "ls-files", "src"], shop).stdout.split("\n")
    have_f = set(tracked)
    pairs = []
    for f in tracked:
        d, name = f.rsplit("/", 1) if "/" in f else ("", f)
        for pat, sub in ((r"(.+)_test\.go$", r"\1.go"), (r"test_(.+)\.py$", r"\1.py"), (r"(.+)\.(?:test|spec)\.(tsx?|jsx?)$", r"\1.\2"), (r"(.+)Tests?\.(java|kt|cs)$", r"\1.\2")):
            m = re.fullmatch(pat, name)
            src = m and f"{d}/{m.expand(sub)}"
            if src and src in have_f:
                pairs.append((src, f))
    first = top3 = 0
    for src, test in pairs:
        got = [json.loads(l)["data"]["path"] for l in sh(["ogrep", "--related-tests", src, ".", "--json"], shop).stdout.splitlines() if '"test"' in l]
        first += got[:1] == [test]
        top3 += test in got[:3]
    print("## K. From a located line to tests and callers\n")
    n = len(pairs)
    print(f"{n} source files with a test file named after them by their language's convention: `ogrep --related-tests FILE` puts that test first for {first}/{n}, in the first three for {top3}/{n}." if n else "no source file with a conventionally named test file")
    defs = collections.defaultdict(list)
    lines_of = {}
    for f in tracked:
        if not f.endswith((".go", ".py")) or "test" in f.lower() or "/gen" in f or ".pb." in f or "_pb2" in f:
            continue
        try:
            lines_of[f] = (shop / f).read_text().split("\n")
        except (UnicodeDecodeError, OSError):
            continue
        for l in lines_of[f]:
            m = re.match(r"(?:func (?:\([^)]*\) )?|def )([A-Za-z_]\w{5,})\(", l)
            if m:
                defs[m.group(1)].append(f)
    agree = asked = 0
    for name in sorted(k for k, v in defs.items() if len(v) == 1)[: 3 * a.cases]:
        call = re.compile(rf"(?<![\w.]){re.escape(name)}\(|\.{re.escape(name)}\(")
        truth = {f for f, ls in lines_of.items() if any(call.search(l) and not re.match(r"\s*(func|def) ", l) for l in ls)}
        if not truth:
            continue
        asked += 1
        got = set()
        for l in sh(["ogrep", "--callers", "-w", name, ".", "--json"], shop).stdout.splitlines():
            d = json.loads(l)
            if d["type"] == "scope":
                got.add(d["data"]["path"]["text"].removeprefix("./"))
        agree += truth <= got
        if asked == a.cases:
            break
    print(f"{asked} Go and Python functions defined once and called somewhere: `ogrep --callers -w NAME` lists every file a regular expression finds a call in for {agree}/{asked}.")


if __name__ == "__main__":
    main()
