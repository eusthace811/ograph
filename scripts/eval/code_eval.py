#!/usr/bin/env python3
"""Measure what ograph gains from pointing it at code, on a real repository.

The repository, its commits, its dependency graph and its log statements are real (the
OpenTelemetry demo by default). The incidents and the telemetry are synthetic: each case is
built from one real commit or one real log statement, so the right answer is known.

  A  log line to code     a burst of a real log message; is evt:codeLocation the statement?
  B  stack frame to code  an error report whose stack names a real file and line
  C  the files to open    `ograph code` files, from the incident's text alone
  D  commit named         a real commit to a file, then errors from that file
  E  red-herring commit   a fault on one dependency, an unrelated real commit on another
  F  no telemetry         an incident with no events at all; `chains` falls back to suspects
  G  write-ups            `ograph code` writeups for incidents with no earlier incident

D and E are scored without the code source and with it. Timings compare the harvest with and
without the code source.

Usage: code_eval.py --bin target/release/ograph --repo PATH --work DIR [--seed N]
"""
import argparse, collections, json, pathlib, random, re, shutil, statistics, subprocess, time

CODE = {".go", ".py", ".js", ".ts", ".tsx", ".java", ".kt", ".cs", ".rs", ".rb", ".php", ".cc", ".cpp", ".ex", ".exs"}
CALL = re.compile(r"""(?:\b(?:log|logger|logging|console|Logger|LOG|slog|_logger|Log|span)\w*\s*(?:\.|->|::)\s*\w+\s*\(|\b(?:info|warn|error|debug|trace|println|eprintln)!\s*\(|\b(?:fmt\.Printf|fmt\.Println|puts|print|echo|Console\.WriteLine)\s*\(?)\s*(?:f|\$)?(["'`])((?:\\.|(?!\1).)*)\1""")
HOLE = re.compile(r"%[-+0-9.]*[a-zA-Z]|\$\{[^}]*\}|\{[^}]*\}|#\{[^}]*\}|\$\w+")
FAULTS = {
    "pool": ("connection pool ran out of free connections under load", "connection pool exhausted, requests waiting for a connection"),
    "disk": ("data volume filled up and writes were refused", "no space left on disk, writes failing"),
    "cert": ("TLS certificate expired and handshakes were rejected", "certificate expired, TLS handshake errors"),
    "memory": ("container was killed for exceeding its memory limit", "out of memory kills, container restarting"),
    "dns": ("resolver returned stale records after a zone change", "DNS lookups failing, name not resolved"),
    "queue": ("consumer lag grew until the message queue hit its retention", "message queue backlog, consumers lagging"),
}


def sh(cmd, cwd=None, stdin=None):
    return subprocess.run(cmd, cwd=cwd, input=stdin, capture_output=True, text=True)


def iso(t):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t))


def statements(repo, services):
    """Real log calls with at least three constant words, whose text appears once in the repository."""
    out = []
    for f in sh(["git", "ls-files", "src"], repo).stdout.split("\n"):
        p = repo / f
        low = f.lower()
        if p.suffix not in CODE or "test" in low or "/gen" in low or ".pb." in low or f.split("/")[1] not in services:
            continue
        try:
            lines = p.read_text().split("\n")
        except (UnicodeDecodeError, OSError):
            continue
        for n, line in enumerate(lines, 1):
            m = CALL.search(line)
            if m and len([w for w in HOLE.sub(" ", m.group(2)).split() if re.search("[A-Za-z]{2}", w)]) >= 3:
                out.append((f, n, m.group(2)))
    seen = collections.Counter(s[2] for s in out)
    return [s for s in out if seen[s[2]] == 1]


def render(literal):
    values = iter(["42", "user-7", "3.5", "eu-west", "17", "abc"] * 4)
    text = HOLE.sub(lambda _: next(values), literal)
    return re.sub(r"\\[nrt]", " ", text).replace('\\"', '"').replace("\\'", "'").strip()


def commits(repo):
    """Every commit as (sha, time, files), newest first, and the one service each touches, if only one."""
    out, cur = [], None
    for l in sh(["git", "log", "--format=@%H %ct", "--name-only"], repo).stdout.splitlines():
        if l.startswith("@"):
            h, t = l[1:].split()
            cur = {"sha": h, "t": int(t), "files": []}
            out.append(cur)
        elif l.strip():
            cur["files"].append(l)
    for c in out:
        svcs = {f.split("/")[1] for f in c["files"] if f.startswith("src/") and f.count("/") >= 2}
        c["svc"] = next(iter(svcs)) if len(svcs) == 1 and all(f.startswith("src/") for f in c["files"]) else None
    return out


def catalog(repo):
    deps, ids = collections.defaultdict(set), set()
    for l in sh(["ogrep", "--entities", ".", "--json"], repo).stdout.splitlines():
        d = json.loads(l)["data"]
        if d["entity"]["kind"] == "service":
            ids.add(d["entity"]["id"])
        for e in d["edges"]:
            if e["relation"] == "depends_on":
                deps[e["from"]].add(e["to"])
    return ids, deps


def frame(path, line):
    """A stack trace in the language of `path`, as it would look from inside a container."""
    ext, base = pathlib.Path(path).suffix, pathlib.Path(path).name
    if ext == ".py":
        return f'Traceback (most recent call last):\n  File "/usr/lib/python3.12/asyncio/runners.py", line 118, in run\n  File "/app/{path}", line {line}, in handle\nRuntimeError: request failed'
    if ext == ".go":
        return f"panic: runtime error: invalid memory address\n\ngoroutine 41 [running]:\nmain.handle(...)\n\t/app/{path}:{line} +0x1d\nnet/http.HandlerFunc.ServeHTTP(...)\n\t/usr/local/go/src/net/http/server.go:2220 +0x29"
    if ext in (".java", ".kt"):
        return f"java.lang.IllegalStateException: request failed\n\tat oteldemo.Handler.handle({base}:{line})\n\tat java.base/java.lang.Thread.run(Thread.java:1583)"
    if ext == ".cs":
        return f"System.InvalidOperationException: request failed\n   at Shop.Handler.Handle() in /app/{path}:line {line}\n   at System.Threading.Tasks.Task.Execute()"
    if ext == ".rs":
        return f"thread 'main' panicked at {path}:{line}:9:\nrequest failed\nstack backtrace:\n   0: rust_begin_unwind\n   1: shop::handle\n             at ./{path}:{line}:9"
    return f"Error: request failed\n    at handle (/app/{path}:{line}:11)\n    at process.processTicksAndRejections (node:internal/process/task_queues:95:5)"


def burst(path, svc, t, message):
    rows = [{"ts": iso(t - 300 + k * 10), "service": svc, "level": "info", "message": f"request handled id={1000 + k} in {20 + k % 7}ms"} for k in range(30)]
    rows += [{"ts": iso(t + k * 2), "service": svc, "level": "error", "message": message} for k in range(8)]
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


def spike(path, series, t0, t1):
    """`series`: {service: (start, factor)}; a 5 s grid from t0 to t1 with a two-minute rise."""
    names = sorted(series)
    rnd = random.Random(str(path.name))
    rows = ["time," + ",".join(f"{n}_latency" for n in names)]
    for t in range(t0, t1, 5):
        cells = []
        for n in names:
            start, factor = series[n]
            cells.append(f"{(100 + rnd.uniform(-2, 2)) * (factor if start <= t < start + 120 else 1):.2f}")
        rows.append(f"{t}," + ",".join(cells))
    path.write_text("\n".join(rows) + "\n")


class Workspace:
    def __init__(self, bin_, root):
        self.bin, self.root = bin_, root

    def run(self, *args):
        p = sh([self.bin, *args, "--json"], self.root)
        try:
            return p.returncode, json.loads(p.stdout)
        except json.JSONDecodeError:
            return p.returncode, {"error": (p.stderr or p.stdout).strip()[:300]}

    def harvest(self, fresh=True):
        if fresh:
            (self.root / ".ograph" / "store.redb").unlink(missing_ok=True)
        t = time.perf_counter()
        p = sh([self.bin, "harvest"], self.root)
        return time.perf_counter() - t, p

    def rows(self, query):
        rc, a = self.run("sparql", query, "--limit", "100000")
        return a.get("rows", [])


def init(bin_, root, sources, extra=""):
    root.mkdir(parents=True, exist_ok=True)
    assert sh([bin_, "init"], root).returncode == 0
    body = "".join(f'[[source]]\nkind = "{k}"\npath = "{p}"\n{o}' for k, p, o in sources)
    (root / ".ograph" / "config.toml").write_text(body + "[correlation]\nwindow_s = 1800\n" + extra)
    return Workspace(bin_, root)


def spaced(items, key, gap):
    """Greedy subset whose times are at least `gap` seconds apart."""
    out = []
    for x in sorted(items, key=key):
        if not out or key(x) - key(out[-1]) >= gap:
            out.append(x)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True); ap.add_argument("--repo", required=True); ap.add_argument("--work", required=True)
    ap.add_argument("--seed", type=int, default=7); ap.add_argument("--cases", type=int, default=40)
    a = ap.parse_args()
    bin_ = str(pathlib.Path(a.bin).resolve())
    work = pathlib.Path(a.work).resolve()
    shutil.rmtree(work, ignore_errors=True)
    main_root = work / "main"
    main_root.mkdir(parents=True)
    shop = main_root / "shop"
    sh(["git", "clone", "-q", str(pathlib.Path(a.repo).resolve()), str(shop)])
    head = sh(["git", "rev-parse", "HEAD"], shop).stdout.strip()
    rnd = random.Random(a.seed)
    ids, deps = catalog(shop)
    log = commits(shop)
    stmts = statements(shop, ids)
    print(f"repository {sh(['git', 'config', '--get', 'remote.origin.url'], pathlib.Path(a.repo)).stdout.strip()} at {head[:12]}: {len(ids)} services, {len(log)} commits, {len(stmts)} unique log statements in {len({pathlib.Path(s[0]).suffix for s in stmts})} languages")
    print(f"ogrep {sh(['ogrep', '--version']).stdout.strip()}; seed {a.seed}\n")

    for d in ("logs", "metrics"):
        (main_root / d).mkdir()
    incidents, errors = [], []

    # A, C: one burst per statement, far before the first commit so nothing else is near.
    t_a = 1782864000  # 2026-07-01
    a_cases = []
    for i, (f, n, lit) in enumerate(stmts):
        svc, t = f.split("/")[1], t_a + i * 7200
        burst(main_root / "logs" / f"A-{i:03}.jsonl", svc, t, render(lit))
        incidents.append({"id": f"INC-A-{i:03}", "title": f"{svc} errors", "service": svc, "opened": t + 120, "severity": "SEV2", "status": "open"})
        a_cases.append({"id": f"INC-A-{i:03}", "key": f"logs/A-{i:03}.jsonl", "file": f"shop/{f}", "line": n})

    # B: one error report per statement position, in the file's language.
    t_b = 1780272000  # 2026-06-01
    b_cases = []
    for i, (f, n, _) in enumerate(stmts):
        errors.append({"service": f.split("/")[1], "at": iso(t_b + i * 7200), "message": f"request failed in handler {i}", "stack": frame(f, n), "level": "error"})
        b_cases.append({"record": f"errors.jsonl#{i + 1}", "file": f"shop/{f}", "line": n, "ext": pathlib.Path(f).suffix})

    # D: the newest commit to a statement's file, then that statement's errors.
    by_file = {}
    for c in log:
        for f in c["files"]:
            by_file.setdefault(f, c)
    d_pool = [(by_file[f], f, n, lit) for f, n, lit in stmts if f in by_file]
    d_pool = list({c["sha"]: (c, f, n, lit) for c, f, n, lit in d_pool}.values())
    d_cases = []
    for i, (c, f, n, lit) in enumerate(spaced(d_pool, lambda x: x[0]["t"], 5400)[: a.cases]):
        svc, t = f.split("/")[1], c["t"]
        burst(main_root / "logs" / f"D-{i:03}.jsonl", svc, t + 600, render(lit))
        other = sorted(deps.get(svc, ()) - {"otel-collector"})
        if other:
            spike(main_root / "metrics" / f"D-{i:03}.csv", {other[0]: (t + 540, 6)}, t - 900, t + 900)
        incidents.append({"id": f"INC-D-{i:03}", "title": f"{svc} errors after a change", "service": svc, "opened": t + 720, "severity": "SEV2", "status": "open"})
        d_cases.append({"id": f"INC-D-{i:03}", "app": svc, "sha": c["sha"], "file": f"shop/{f}", "distractor": other[0] if other else None})
    taken = [c["sha"] for c, *_ in d_pool]

    # E: a fault on dependency D of U, and a real commit on another dependency three minutes before the incident.
    e_pool = []
    for c in log:
        if c["svc"] and c["sha"] not in taken:
            for u in sorted(ids):
                rest = sorted(deps.get(u, set()) - {c["svc"], "otel-collector"})
                if c["svc"] in deps.get(u, ()) and rest:
                    e_pool.append((c, u, rest))
                    break
    e_cases = []
    busy = [x["t"] for x in (by_file[c["file"][5:]] for c in d_cases)]
    for c, u, rest in spaced(e_pool, lambda x: x[0]["t"], 5400):
        if len(e_cases) >= a.cases or any(abs(c["t"] - b) < 5400 for b in busy):
            continue
        i, t, d = len(e_cases), c["t"], rnd.choice(rest)
        spike(main_root / "metrics" / f"E-{i:03}.csv", {d: (t + 60, 8), u: (t + 120, 4)}, t - 900, t + 600)
        incidents.append({"id": f"INC-E-{i:03}", "title": f"{u} slow", "service": u, "opened": t + 240, "severity": "SEV2", "status": "open"})
        e_cases.append({"id": f"INC-E-{i:03}", "app": d, "commit_on": c["svc"], "sha": c["sha"]})

    # G: write-ups committed to the repository, and incidents that word the same fault differently.
    t_g = 1777593600  # 2026-05-01
    pm = shop / "docs" / "postmortems"
    pm.mkdir(parents=True)
    g_svcs = sorted(s for s in ids if s in deps)[:6]
    g_cases = []
    for s in g_svcs:
        for k, (written, said) in FAULTS.items():
            (pm / f"{s}-{k}.md").write_text(f"# {s}: {written}\n\nWhat happened: in {s} the {written}. Customers saw errors until the on-call engineer mitigated it.\n")
            i = len(g_cases)
            incidents.append({"id": f"INC-G-{i:03}", "title": f"{s}: {said}", "service": s, "opened": t_g + i * 21600, "severity": "SEV2", "status": "open"})
            g_cases.append({"id": f"INC-G-{i:03}", "path": f"shop/docs/postmortems/{s}-{k}.md"})
    sh(["git", "add", "-A"], shop)
    sh(["git", "-c", "user.name=eval", "-c", "user.email=eval@example.invalid", "commit", "-qm", "Add postmortems"], shop)

    (main_root / "incidents.json").write_text(json.dumps(incidents))
    (main_root / "errors.jsonl").write_text("".join(json.dumps(e) + "\n" for e in errors))
    telemetry = [("logs", "logs", ""), ("metrics", "metrics", ""), ("incidents", "incidents.json", "")]
    with_code = init(bin_, main_root, [("catalog", "shop", ""), ("code", "shop", 'since = "2020-01-01"\n'), ("errors", "errors.jsonl", "")] + telemetry)
    plain_root = work / "plain"
    plain_root.mkdir()
    for n in ("logs", "metrics", "incidents.json", "shop"):
        (plain_root / n).symlink_to(main_root / n)
    without = init(bin_, plain_root, [("catalog", "shop", "")] + telemetry)
    recent_root = work / "recent"
    recent_root.mkdir()
    for n in ("logs", "metrics", "incidents.json", "errors.jsonl", "shop"):
        (recent_root / n).symlink_to(main_root / n)
    recent = init(bin_, recent_root, [("catalog", "shop", ""), ("code", "shop", ""), ("errors", "errors.jsonl", "")] + telemetry)

    # Timings: three fresh harvests each, then again with an ogrep index in the repository.
    def timed(ws):
        runs = [ws.harvest() for _ in range(3)]
        assert runs[-1][1].returncode == 0, runs[-1][1].stderr
        return statistics.median(r[0] for r in runs), runs[-1][1].stdout.strip().splitlines()[-1]
    t_plain, last_plain = timed(without)
    t_recent, _ = timed(recent)
    t_code, last_code = timed(with_code)
    sh(["ogrep", "index", "."], shop)
    t_indexed, _ = timed(with_code)
    quads = lambda ws: int(sh([ws.bin, "stats"], ws.root).stdout.split("distinct triples")[0].split()[-1])
    print("## Harvest cost\n")
    print("| workspace | harvest, median of 3 | distinct triples |\n|---|---|---|")
    print(f"| telemetry and catalog only | {t_plain:.2f} s | {quads(without)} |")
    print(f"| with the code and errors sources, commits of the last 30 days (the default) | {t_recent:.2f} s | {quads(recent)} |")
    print(f"| the same with every commit since 2020 ({len(log)} commits), no ogrep index | {t_code:.2f} s | {quads(with_code)} |")
    print(f"| the same after `ogrep index .` in the repository | {t_indexed:.2f} s | {quads(with_code)} |")
    print(f"\n{last_code}")
    rc, problems = with_code.run("validate")
    print(f"`ograph validate` on the workspace with code: {len(problems) if isinstance(problems, list) else problems} violations")
    kinds = collections.Counter(r[0] for r in with_code.rows("SELECT ?s WHERE { ?e evt:signal ?s }"))
    print(f"events by signal: {dict(sorted(kinds.items()))}\n")

    # A: locations of log events.
    loc = collections.defaultdict(list)
    for rec, where in with_code.rows("SELECT ?r ?l WHERE { ?e evt:sourceRecord ?r ; evt:codeLocation ?l }"):
        loc[rec.split("@")[0].split("#")[0] if rec.startswith("logs/") else rec].append(where)
    fired = {r[0].split("@")[0] for r in with_code.rows("SELECT ?r WHERE { ?e evt:sourceRecord ?r ; evt:signal \"LOG_ERROR\" }")}
    def score(cases, key):
        got = [(c, loc.get(c[key], [])) for c in cases]
        return (sum(any(l.rsplit(":", 1)[0] == c["file"] for l in ls) for c, ls in got), sum(f"{c['file']}:{c['line']}" in ls for c, ls in got), sum(bool(ls) for _, ls in got))
    f_ok, l_ok, any_ = score(a_cases, "key")
    n = len(a_cases)
    print("## A. Log line to code\n")
    print(f"{n} bursts of real log messages; the detector fired on {sum(c['key'] in fired for c in a_cases)}.")
    print(f"evt:codeLocation names the right file for {f_ok}/{n} = {f_ok / n:.2f}, the exact line for {l_ok}/{n} = {l_ok / n:.2f}; a location was given for {any_}, so {any_ - f_ok} are wrong files and {n - any_} have none.")
    by_ext = collections.defaultdict(lambda: [0, 0])
    for c in a_cases:
        e = by_ext[pathlib.Path(c["file"]).suffix]
        e[1] += 1
        e[0] += any(l.rsplit(":", 1)[0] == c["file"] for l in loc.get(c["key"], []))
    print("right file by language: " + ", ".join(f"{k} {v[0]}/{v[1]}" for k, v in sorted(by_ext.items())) + "\n")

    # B: stack frames.
    f_ok, l_ok, any_ = score(b_cases, "record")
    n = len(b_cases)
    print("## B. Stack frame to code\n")
    print(f"{n} error reports; the right file for {f_ok}/{n} = {f_ok / n:.2f}, the exact line for {l_ok}/{n} = {l_ok / n:.2f}; {n - any_} have no location.")
    by_ext = collections.defaultdict(lambda: [0, 0])
    for c in b_cases:
        by_ext[c["ext"]][1] += 1
        by_ext[c["ext"]][0] += f"{c['file']}:{c['line']}" in loc.get(c["record"], [])
    print("exact line by language: " + ", ".join(f"{k} {v[0]}/{v[1]}" for k, v in sorted(by_ext.items())) + "\n")

    # C: files from the incident's text.
    top1 = top3 = 0
    lat = []
    for c in a_cases:
        t = time.perf_counter()
        rc, out = with_code.run("code", c["id"])
        lat.append((time.perf_counter() - t) * 1000)
        files = [f["path"] for f in out.get("files", [])]
        top1 += files[:1] == [c["file"]]
        top3 += c["file"] in files[:3]
    n = len(a_cases)
    print("## C. The files to open\n")
    print(f"`ograph code INCIDENT` for the {n} incidents of A: the statement's file is first in `files` for {top1}/{n} = {top1 / n:.2f}, in the first three for {top3}/{n} = {top3 / n:.2f}.")
    print(f"`ograph code` took a median of {statistics.median(lat):.0f} ms (p90 {sorted(lat)[int(len(lat) * 0.9)]:.0f} ms), with the ogrep index.\n")

    # D and E: ranking with and without the code source and the code rule.
    def ranking(ws, cases, sets=()):
        app1 = named = 0
        for c in cases:
            rc, out = ws.run("chains", c["id"], "--top", "5", *[x for s in sets for x in ("--set", s)])
            apps = out.get("apps", [])
            app1 += bool(apps) and apps[0]["app"] == c["app"]
            root = (apps[0].get("chain") or {}).get("root", {}) if apps else {}
            named += root.get("kind") == "COMMIT" and c["sha"][:12] in root.get("iri", "")
        return app1, named
    print("## D. A commit, then errors from the file it changed\n")
    n = len(d_cases)
    listed = touched = 0
    for c in d_cases:
        rc, out = with_code.run("code", c["id"])
        cs = out.get("commits", [])
        listed += any(x.get("revision") == c["sha"] for x in cs)
        touched += bool(cs) and cs[0].get("revision") == c["sha"] and bool(cs[0].get("touches"))
    print(f"{n} cases, each a real commit to one service, a burst of a message that file writes 10 minutes later, and an unrelated metric spike on a dependency ({sum(bool(c['distractor']) for c in d_cases)} of them).\n")
    print("| configuration | right application first | top chain is rooted at the commit |\n|---|---|---|")
    for name, ws, sets in (("no code source", without, ()), ("with the code source", with_code, ())):
        a1, nm = ranking(ws, d_cases, sets)
        print(f"| {name} | {a1}/{n} = {a1 / n:.2f} | {nm}/{n} = {nm / n:.2f} |")
    print(f"\n`ograph code` lists the commit for {listed}/{n} and puts it first with the file it touches for {touched}/{n}.\n")

    print("## E. A fault on one dependency, an unrelated commit on another\n")
    n = len(e_cases)
    print(f"{n} cases: a strong latency spike on dependency D, a weaker one on the incident's service, and a real commit to a different dependency four minutes before the incident. The right answer is D.\n")
    print("| configuration | right application first | top chain is rooted at the commit |\n|---|---|---|")
    for name, ws, sets in (("no code source", without, ()), ("with the code source", with_code, ())):
        a1, nm = ranking(ws, e_cases, sets)
        print(f"| {name} | {a1}/{n} = {a1 / n:.2f} | {nm}/{n} = {nm / n:.2f} |")
    print()

    # G: write-ups against signature similarity on a store with no earlier incident of the kind.
    n = len(g_cases)
    hit1 = hit3 = sim = 0
    for c in g_cases:
        rc, out = with_code.run("code", c["id"])
        paths = [w["path"] for w in out.get("writeups", [])]
        hit1 += paths[:1] == [c["path"]]
        hit3 += c["path"] in paths[:3]
        rc, s = with_code.run("similar", c["id"])
        sim += isinstance(s, list) and len(s) > 0
    print("## G. Write-ups\n")
    print(f"{n} incidents with no telemetry, and {n} write-ups in the repository ({len(g_svcs)} services by {len(FAULTS)} faults) that describe each fault in other words.")
    print(f"`ograph code` puts the right write-up first for {hit1}/{n} = {hit1 / n:.2f}, in the first three for {hit3}/{n} = {hit3 / n:.2f}. `ograph similar` returns something for {sim}/{n}.\n")

    # F: no telemetry at all, the repository as it was at the commit.
    # The services and dependencies are read at the commit, since both changed over the years.
    f_pool = spaced([c for c in log if c["svc"]], lambda c: c["t"], 86400)
    before = r1 = r3 = failed = n = 0
    by_dist = collections.defaultdict(lambda: [0, 0])
    for c in f_pool:
        if n >= a.cases:
            break
        root = work / "f" / f"{n:03}"
        root.mkdir(parents=True)
        sh(["git", "clone", "-q", str(shop), str(root / "shop")])
        sh(["git", "checkout", "-q", c["sha"]], root / "shop")
        then_ids, then_deps = catalog(root / "shop")
        near = sorted(u for u in then_ids if c["svc"] in then_deps.get(u, ()))
        far = sorted(u for u in then_ids if any(c["svc"] in then_deps.get(m, ()) for m in then_deps.get(u, ())) and u not in near and u != c["svc"])
        if c["svc"] not in then_ids or not near:
            shutil.rmtree(root)
            continue
        dist = 2 if far and n % 2 else 1
        u = rnd.choice(far if dist == 2 else near)
        (root / "incidents.json").write_text(json.dumps([{"id": "INC-F", "title": f"{u} failing", "service": u, "opened": c["t"] + 1800, "severity": "SEV2", "status": "open"}]))
        ws = init(bin_, root, [("catalog", "shop", ""), ("code", "shop", 'since = "1d"\n'), ("incidents", "incidents.json", "")])
        ws.harvest()
        rc, out = ws.run("chains", "INC-F")
        before += bool(out.get("apps"))
        names = [x["app"] for x in out.get("suspects", [])]
        failed += not names
        by_dist[dist][1] += 1
        by_dist[dist][0] += names[:1] == [c["svc"]]
        r1 += names[:1] == [c["svc"]]
        r3 += c["svc"] in names[:3]
        n += 1
    print("## F. An incident with no telemetry\n")
    print(f"{n} cases: the repository checked out at a real commit to service S, and an incident 30 minutes later on a service that depends on S directly or through one other service. No logs, metrics, traces or alerts.")
    print(f"`ograph chains` ranked an application from telemetry in {before}/{n} cases. Its suspects fallback names S first for {r1}/{n} = {r1 / n:.2f}, in the first three for {r3}/{n} = {r3 / n:.2f}; it returned nothing for {failed}.")
    print("first by distance: " + ", ".join(f"distance {k} {v[0]}/{v[1]}" for k, v in sorted(by_dist.items())))


if __name__ == "__main__":
    main()
