#!/usr/bin/env python3
"""Do agents that investigate an incident with ograph name the right root cause?

Each case is one incident with a known root-cause application. Each arm answers it N times:

  mcp     headless pi with only the ograph tools (the extension scripts/agent/install.py writes)
  skill   headless pi with skills/ograph, bash and read, `ograph` on PATH
  raw     headless pi with bash and read over the raw source files; no store, no `ograph`
  chains  no agent: rank 1 of `ograph chains`, the floor

Every agent arm gets the same prompt and ends its reply with `ROOT: <application>`. A run passes
when that application is the labeled root cause (AC@1). Per run the script also records tool
calls, wall time, the tokens and cost pi reports, and whether the reply calls the cause a
hypothesis. Per case and arm the N runs give one verdict: stable_pass, flaky, stable_fail, or
error (pi failed, or the run touched something it must not see).

Labels never enter an agent's workspace. Each run works in a fresh copy of the case's files at a
neutral path, with a copied binary and a copied skill. The copy is scanned before the run and
the transcript after it (`leaks_in_dir`, `leaks_in_transcript`); a hit voids the run.

Cases: a seeded sample of the RCAEval export (--cases, labels in its labels.json) and the demo
incidents (--demo, labeled below from the story examples/demo/generate.py writes).

Usage:
  agent_rca_eval.py --bin target/release/ograph --cases data/rcaeval-cases --sample 30 \\
      --demo examples/demo --runs 3 --out docs/evidence/agent-rca [--arms mcp,skill,raw,chains]
      [--reference rcaeval_score.json] [--misses] [--parallel 4] [--work DIR] [--fresh]
  agent_rca_eval.py --mirror-cases data/rcaeval-cases target/rcaeval-ref
      (a writable copy of the cases by symlink, to run rcaeval_score.py for --reference)

Rows already in OUT/runs.jsonl are kept and not rerun, unless they are errors or --fresh is set.
A run the provider refuses for its usage limit stops the model arms; rerun later to finish."""
import argparse, collections, concurrent.futures, dataclasses, itertools, json, math, os, pathlib, random, re, shutil, statistics, subprocess, sys, tempfile, threading, time, uuid

HERE = pathlib.Path(__file__).resolve()
REPO = HERE.parents[2]

PROMPT = """Incident {incident} is open. The workspace for it is the current directory.

Investigate it and tell me which application is the root cause: the one where the fault started, not the ones that only show its effects. Explain briefly what you found and the evidence for it.

End your reply with one line in exactly this form:
ROOT: <application name>"""

# The demo's story, from examples/demo/generate.py: the ledger-db pool exhausted (INC-4120, and
# again for INC-5377 after its max_connections change), the checkout v4.1.0 release (INC-4388),
# and the elasticsearch cluster that lost two data nodes (INC-4501).
DEMO_ROOTS = {"INC-4120": "ledger-db", "INC-4388": "checkout", "INC-4501": "elasticsearch", "INC-5377": "ledger-db"}
DEMO_HIDDEN = ("sparql-questions.json", "*-labels.json", "questions*.txt", "generate.py", "store.redb", "store.lock", "lab.html")

HYPOTHESIS = re.compile(r"hypothes|not (?:yet )?(?:a )?(?:confirmed|conclusi|verified|proven)|unconfirmed|unverified|needs? (?:confirm|verif)|until (?:confirmed|verified)", re.I)


@dataclasses.dataclass(frozen=True)
class Case:
    suite: str          # "rcaeval" or "demo"
    id: str
    incident: str
    root: str
    source: pathlib.Path

    @property
    def key(self): return f"{self.suite}/{self.id}/{self.incident}"


@dataclasses.dataclass
class Answer:
    status: str                 # pass, fail, error, leak; set by `judge`
    root: str = ""
    why: str = ""
    text: str = ""
    tool_calls: int = 0
    tools: dict = dataclasses.field(default_factory=dict)
    wall_s: float = 0.0
    tokens: int = 0
    cost: float = 0.0
    events: list = dataclasses.field(default_factory=list)


@dataclasses.dataclass(frozen=True)
class Arm:
    name: str
    about: str
    store: bool                 # the workspace holds a harvested .ograph store
    answer: object              # (ctx, case, workspace) -> Answer
    unavailable: object = None  # (ctx) -> reason the arm cannot run here, or None
    rule: object = None         # (tool name, args as text) -> what the call broke, or None


class Ctx:
    """Paths one run of the harness shares. Everything an agent may open sits under `work`."""
    halt = None  # why no further model call is worth making: the provider's usage limit

    def __init__(self, a):
        self.a = a
        self.work = pathlib.Path(a.work).resolve() if a.work else pathlib.Path(tempfile.mkdtemp(prefix="ops-"))
        self.work.mkdir(parents=True, exist_ok=True)
        self.bin = self.work / "bin" / "ograph"
        self.nobin = self.work / "nobin"
        self.skill = self.work / "skills" / "ograph"
        self.extension = self.work / "ext" / "ograph.ts"
        self.cases_dir = pathlib.Path(a.cases).resolve() if a.cases else None
        self.secret = [REPO, *( [self.cases_dir] if self.cases_dir else [])]
        main = REPO.parents[2] if REPO.parent.name == "worktrees" else REPO
        if main != REPO: self.secret.append(main)
        self.lock = threading.Lock()
        self.staged = {}

    def setup(self):
        for d in (self.bin.parent, self.nobin, self.skill.parent, self.extension.parent): d.mkdir(parents=True, exist_ok=True)
        shutil.copy2(pathlib.Path(self.a.bin).resolve(), self.bin)
        shim = self.nobin / "ograph"
        shim.write_text("#!/bin/sh\necho 'ograph: command not found' >&2\nexit 127\n"); shim.chmod(0o755)
        shutil.rmtree(self.skill, ignore_errors=True)
        shutil.copytree(REPO / "skills" / "ograph", self.skill)

    def env(self, first):
        """The harness's environment without anything that names the repository or the cases."""
        hide = [str(p) for p in self.secret]
        env = {k: v for k, v in os.environ.items() if k not in ("PWD", "OLDPWD") and not any(h in v for h in hide) or k == "PATH"}
        env["PATH"] = os.pathsep.join([str(first)] + [p for p in os.environ["PATH"].split(os.pathsep) if not any(h in p for h in hide)])
        return env


def run(cmd, cwd, **kw):
    return subprocess.run([str(c) for c in cmd], cwd=cwd, capture_output=True, text=True, **kw)


# ---------- cases

def rcaeval_cases(cases_dir, sample, seed):
    """One case per (root service, fault) cell, the repetition drawn by `seed`; `sample` cells in
    a seeded order, so a smaller sample is a prefix of a larger one."""
    labels = json.loads((cases_dir / "labels.json").read_text())
    cells = collections.defaultdict(list)
    for cid in sorted(labels): cells[(labels[cid]["root"], labels[cid]["fault"])].append(cid)
    rng = random.Random(seed)
    picks = [rng.choice(cells[c]) for c in sorted(cells)]
    rng.shuffle(picks)
    return [Case("rcaeval", cid, labels[cid]["incident"], labels[cid]["root"], cases_dir / cid) for cid in picks[:sample]]


def missed_cases(cases_dir, reference, taken):
    """Every case whose rank 1 in the reference run (rcaeval_score.py --out) is wrong and that
    the sample does not hold already. Chosen by ograph's own result, so reported as their own
    suite: on them the chains arm scores 0 by construction, and the question is whether an agent
    that reads the same chains recovers."""
    labels = json.loads((cases_dir / "labels.json").read_text())
    ref = json.loads(pathlib.Path(reference).read_text())["results"]
    return [Case("rcaeval-miss", cid, labels[cid]["incident"], labels[cid]["root"], cases_dir / cid)
            for cid in sorted(labels) if (ref[cid]["chain"] or [""])[0] != labels[cid]["root"] and cid not in taken]


def demo_cases(demo):
    return [Case("demo", "demo", inc, root, demo) for inc, root in DEMO_ROOTS.items()]


# ---------- workspaces

def clone(src, dst):
    """Copy a tree; a copy-on-write clone where the filesystem has one (APFS), so 100 MB of
    telemetry per run costs nothing."""
    if sys.platform == "darwin" and subprocess.run(["cp", "-cRp", str(src), str(dst)], capture_output=True).returncode == 0: return
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(src, dst, symlinks=True)


def stage(ctx, case):
    """The case's files once under `work`, harvested, with every source path made relative so
    nothing in a workspace points back at the repository. Returns the staged directory."""
    with ctx.lock:
        slot = ctx.staged.setdefault(case.source, threading.Lock())
    dst = ctx.work / "stage" / f"{case.suite}-{case.id}"
    with slot:
        if (dst / ".staged").exists(): return dst
        shutil.rmtree(dst, ignore_errors=True)
        shutil.copytree(case.source, dst, symlinks=True, ignore=shutil.ignore_patterns(*DEMO_HIDDEN))
        config = dst / ".ograph" / "config.toml"
        def relocate(m):
            p = pathlib.Path(m[1])
            if not p.is_absolute(): return m[0]
            name = "catalog" if not (dst / "catalog").exists() else p.name
            shutil.copytree(p, dst / name, symlinks=True) if p.is_dir() else shutil.copy2(p, dst / name)
            return f'path = "{name}"'
        config.write_text(re.sub(r'path = "([^"]*)"', relocate, config.read_text()))
        p = run([ctx.bin, "harvest", "--json"], dst, env=ctx.env(ctx.bin.parent))
        if p.returncode != 0: raise RuntimeError(f"harvest failed: {p.stderr.strip()[-300:]}")
        (dst / ".staged").touch()
    return dst


def workspace(ctx, case, arm):
    """A fresh private copy for one run: <work>/runs/<id>/ops. Its parent holds nothing else."""
    ws = ctx.work / "runs" / uuid.uuid4().hex[:10] / "ops"
    ws.parent.mkdir(parents=True)
    clone(stage(ctx, case), ws)
    (ws / ".staged").unlink()
    if not arm.store: shutil.rmtree(ws / ".ograph")
    return ws


# ---------- leak detection

LABEL_NAMES = re.compile(r"label|question|generate\.py", re.I)
LABEL_KEYS = re.compile(rb'"(?:root|inject|fault)"\s*:')
PATHLIKE = re.compile(r"""(?<![\w:/.\\])(/[\w.@+-]+(?:/[\w.@+-]+)*)""")


def secret_words(ctx):
    return [b"rcaeval", b"labels.json"] + [str(p).lower().encode() for p in ctx.secret]


def leaks_in_dir(ctx, ws):
    """What in a workspace could give the answer away: a label file by name, label-shaped JSON,
    or any mention of the dataset, the labels file, the repository or the cases directory."""
    found = []
    words = secret_words(ctx)
    for p in sorted(ws.rglob("*")):
        rel = p.relative_to(ws)
        if LABEL_NAMES.search(p.name): found.append(f"{rel}: file name")
        if p.is_symlink(): found.append(f"{rel}: symlink to {os.readlink(p)}")
        elif p.is_file():
            data = p.read_bytes()
            low = data.lower()
            found += [f"{rel}: contains {w.decode()!r}" for w in words if w in low]
            if LABEL_KEYS.search(data): found.append(f"{rel}: label-shaped JSON keys")
    return found


def leaks_in_transcript(ctx, ws, events, arm):
    """What a run did that voids it: a call that reaches outside its workspace (the home
    directory, another volume, another run), a call the arm does not allow, or tool output that
    mentions the dataset, the labels file, the repository or the cases directory."""
    found = []
    words = secret_words(ctx)
    allowed = [str(ws), str(ctx.bin.parent), str(ctx.nobin), str(ctx.skill.parent)]
    banned = [str(ctx.work), str(pathlib.Path.home()), "/Volumes", "/Users", "/home", "/mnt"] + [str(p) for p in ctx.secret]
    for e in events:
        if e.get("type") == "tool_execution_start":
            name, args = e.get("toolName", ""), json.dumps(e.get("args", {}), ensure_ascii=False)
            if arm.rule and (broke := arm.rule(name, args)): found.append(f"{name}: {broke}")
            flat = args.replace("\\\\", "\\").replace('\\"', '"').replace("\\n", "\n")
            for path in PATHLIKE.findall(flat):
                real = os.path.realpath(path) if path.startswith(("/tmp", "/var")) else path
                if any(real == a or real.startswith(a + "/") for a in allowed): continue
                if any(real == b or real.startswith(b + "/") for b in banned): found.append(f"{name}: reaches {path}")
            if re.search(r"(?:^|[\s\"'/=])\.\.(?:[/\s\"']|$)", flat): found.append(f"{name}: climbs out with ..")
            if re.search(r"(?:^|[\s\"'=])~/|\$\{?HOME\b", flat): found.append(f"{name}: reaches the home directory")
            found += [f"{name}: asks for {w.decode()!r}" for w in words if w in flat.lower().encode()]
        elif e.get("type") == "tool_execution_end":
            low = json.dumps(e.get("result", {}), ensure_ascii=False).lower().encode()
            found += [f"{e.get('toolName')}: output contains {w.decode()!r}" for w in words if w in low]
    return sorted(set(found))


# ---------- answers

def parse_root(text):
    """The application on the reply's last `ROOT:` line, as a bare lowercase name."""
    lines = re.findall(r"^[\s>*_`-]*ROOT[*_`]*\s*:\s*(.+?)\s*$", text, re.M)
    if not lines: return ""
    name = lines[-1].strip("*_`\"'<>. ")
    return name.split("/")[-1].split()[0].strip("*_`\"'<>.,;:()").lower() if name else ""


def digest(stdout):
    """pi's --mode json stream reduced to what the score and the transcript need."""
    events, text, tokens, cost, tools = [], "", 0, 0.0, collections.Counter()
    for line in stdout.splitlines():
        try: e = json.loads(line)
        except ValueError: continue
        kind = e.get("type")
        if kind == "tool_execution_start":
            tools[e.get("toolName", "?")] += 1
            events.append({k: e.get(k) for k in ("type", "toolName", "args")})
        elif kind == "tool_execution_end":
            events.append({k: e.get(k) for k in ("type", "toolName", "result", "isError")})
        elif kind == "message_end" and e.get("message", {}).get("role") == "assistant":
            m = e["message"]
            said = "".join(c.get("text", "") for c in m.get("content", []) if c.get("type") == "text")
            if said: text = said; events.append({"type": "assistant", "text": said})
            usage = m.get("usage") or {}
            tokens += usage.get("totalTokens", 0); cost += (usage.get("cost") or {}).get("total", 0)
    return events, text, tokens, cost, dict(tools)


def pi_answer(flags, first_on_path):
    """An arm that asks headless pi. `flags(ctx)` are the arm's own pi flags; the rest, and the
    retry on a provider failure, follow sparql_agent_eval.py."""
    def answer(ctx, case, ws):
        cmd = ["pi", "-p", "--mode", "json", "--no-session", "--no-extensions", "--no-context-files", "--no-prompt-templates",
               "--no-skills", "--no-mcp", "--no-approve", *flags(ctx), "--model", ctx.a.model, PROMPT.format(incident=case.incident)]
        if ctx.halt: return Answer("error", why=f"not run: {ctx.halt}")
        why = ""
        for attempt in range(3):
            t = time.time()
            try:
                # stdin closed: pi -p waits for piped input otherwise.
                p = subprocess.run(cmd, cwd=ws, env=ctx.env(first_on_path(ctx)), stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=ctx.a.timeout)
                out, timed_out = p.stdout, False
            except subprocess.TimeoutExpired as e:
                out, timed_out = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or ""), True
            events, text, tokens, cost, tools = digest(out)
            got = Answer("", parse_root(text), "", text, sum(tools.values()), tools, time.time() - t, tokens, cost, events)
            if timed_out and tools:
                got.why = f"no answer within {ctx.a.timeout} s"; return got
            if not timed_out and p.returncode == 0 and text.strip(): return got
            # An empty reply, a failed exit or a hang before any tool call is the provider, not the agent.
            why = f"no reply within {ctx.a.timeout} s" if timed_out else f"exit {p.returncode}: {(p.stderr or p.stdout).strip()[-200:]}"
            if not timed_out and re.search(r"usage limit", p.stderr + p.stdout, re.I):
                ctx.halt = "the provider's usage limit is reached"; break
            if attempt < 2: time.sleep(30 * (attempt + 1))
        return Answer("error", why=f"pi failed 3 times, last: {why}")
    return answer


def chains_answer(ctx, case, ws):
    t = time.time()
    p = run([ctx.bin, "chains", case.incident, "--json", "--top", "5", "--rank", "chain"], ws)
    if p.returncode not in (0, 1): return Answer("error", why=f"ograph chains exit {p.returncode}: {p.stderr.strip()[-200:]}")
    apps = [x["app"] for x in json.loads(p.stdout)["apps"]] if p.stdout.strip() else []
    return Answer("", apps[0].lower() if apps else "", "" if apps else "no chain", " > ".join(apps), wall_s=time.time() - t)


# ---------- arms

def extension_tools(ctx):
    return [t["name"] for t in json.loads(re.search(r"^const tools: any\[\] = (.*);$", ctx.extension.read_text(), re.M)[1])]


def mcp_unavailable(ctx):
    if not shutil.which("pi"): return "pi is not on PATH"
    seed = ctx.work / "ext" / "seed"
    shutil.rmtree(seed, ignore_errors=True); (seed / ".ograph").mkdir(parents=True)
    (seed / ".ograph" / "config.toml").write_text("")
    p = run([sys.executable, REPO / "scripts" / "agent" / "install.py", "--bin", ctx.bin, "--root", seed, "--out", ctx.extension], seed)
    if p.returncode != 0 or not ctx.extension.exists(): return f"scripts/agent/install.py failed: {(p.stderr or p.stdout).strip()[-200:]}"
    return None if extension_tools(ctx) else "the generated extension registers no tools"


def needs_pi(ctx):
    return None if shutil.which("pi") else "pi is not on PATH"


ARMS = {arm.name: arm for arm in (
    Arm("mcp", "pi with only the ograph tools (scripts/agent extension)", True,
        pi_answer(lambda ctx: ["--extension", str(ctx.extension), "--tools", ",".join("ograph_" + t for t in extension_tools(ctx))], lambda ctx: ctx.nobin),
        mcp_unavailable, lambda name, args: None if name.startswith("ograph_") else "not an ograph tool"),
    Arm("skill", "pi with skills/ograph, bash and read", True,
        pi_answer(lambda ctx: ["--skill", str(ctx.skill), "--tools", "bash,read"], lambda ctx: ctx.bin.parent), needs_pi),
    Arm("raw", "pi with bash and read over the raw files, no ograph", False,
        pi_answer(lambda ctx: ["--tools", "bash,read"], lambda ctx: ctx.nobin),
        needs_pi, lambda name, args: "uses ograph" if re.search(r"\bograph\b", args) else None),
    Arm("chains", "no agent: `ograph chains` rank 1", True, chains_answer),
)}


# ---------- scoring

def judge(ctx, case, arm, ws, got):
    """Fill in the run's status. A leak outranks the answer: the run is void either way."""
    if got.status == "error": return got
    leaks = leaks_in_transcript(ctx, ws, got.events, arm)
    if leaks: got.status, got.why = "leak", "; ".join(leaks)[:600]
    elif not got.root: got.status, got.why = "fail", got.why or "no ROOT line in the reply"
    else: got.status = "pass" if got.root == case.root.lower() else "fail"
    return got


def verdict(statuses):
    if not statuses: return "missing"
    if any(s in ("error", "leak") for s in statuses): return "error"
    passed = sum(s == "pass" for s in statuses)
    return "stable_pass" if passed == len(statuses) else "stable_fail" if passed == 0 else "flaky"


def sign_test(wins, losses):
    """Two-sided exact binomial p for `wins` against `losses` at even odds."""
    n, k = wins + losses, min(wins, losses)
    return 1.0 if n == 0 else min(1.0, 2 * sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n)


def transcript(case, arm, n, row, events):
    out = [f"# {case.key} arm {arm.name} run {n}: {row['status']}, answered {row['root'] or '-'}, labeled {case.root}", ""]
    for e in events:
        if e["type"] == "tool_execution_start": out.append(f"$ {e['toolName']} {json.dumps(e['args'], ensure_ascii=False)}")
        elif e["type"] == "tool_execution_end":
            body = "".join(c.get("text", "") for c in (e.get("result") or {}).get("content", []) if isinstance(c, dict))
            out.append(("! " if e.get("isError") else "") + (body if len(body) <= 3000 else body[:3000] + f"\n... [{len(body) - 3000} more characters]") + "\n")
        else: out.append(e["text"] + "\n")
    return "\n".join(out)


def one(ctx, case, arm, n, out):
    ws = None
    try:
        ws = workspace(ctx, case, arm)
        planted = leaks_in_dir(ctx, ws)
        got = Answer("leak", why="workspace: " + "; ".join(planted)[:600]) if planted else judge(ctx, case, arm, ws, arm.answer(ctx, case, ws))
    except Exception as e:
        got = Answer("error", why=f"{type(e).__name__}: {e}"[:300])
    finally:
        if ws and not ctx.a.keep: shutil.rmtree(ws.parent, ignore_errors=True)
    row = {"case": case.key, "arm": arm.name, "run": n, "status": got.status, "root": got.root, "label": case.root, "why": got.why,
           "hypothesis": bool(HYPOTHESIS.search(got.text)), "tool_calls": got.tool_calls, "tools": got.tools,
           "wall_s": round(got.wall_s, 1), "tokens": got.tokens, "cost": round(got.cost, 4)}
    if got.events:
        path = out / "transcripts" / arm.name / f"{case.key.replace('/', '_')}-{n}.txt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(transcript(case, arm, n, row, got.events))
    return row


def report(cases, arms, skipped, rows, runs, reference, model):
    by = collections.defaultdict(list)
    for r in rows: by[(r["arm"], r["case"])].append(r)
    med = lambda xs: statistics.median(xs) if xs else 0
    print(f"model {model}; {len(cases)} cases x {runs} runs; arms {', '.join(a.name for a in arms)}")
    for name, why in skipped.items(): print(f"skipped arm {name}: {why}")
    for suite in sorted({c.suite for c in cases}):
        cs = [c for c in cases if c.suite == suite]
        print(f"\n{suite} ({len(cs)} cases)\n\n| arm | AC@1 over runs | stable_pass | flaky | stable_fail | error | says hypothesis | tool calls | wall s | tokens | cost $ |\n|---|---|---|---|---|---|---|---|---|---|---|")
        for arm in arms:
            mine = [r for c in cs for r in by[(arm.name, c.key)]]
            scored = [r for r in mine if r["status"] in ("pass", "fail")]
            v = collections.Counter(verdict([r["status"] for r in by[(arm.name, c.key)]]) for c in cs)
            passed = sum(r["status"] == "pass" for r in scored)
            hedged = f"{sum(r['hypothesis'] for r in scored)}/{len(scored)}" if arm.name != "chains" else "-"
            print(f"| {arm.name} | {passed}/{len(scored)} ({passed / max(len(scored), 1):.2f}) "
                  f"| {v['stable_pass']} | {v['flaky']} | {v['stable_fail']} | {v['error'] + v['missing']} "
                  f"| {hedged} | {med([r['tool_calls'] for r in scored]):g} | {med([r['wall_s'] for r in scored]):.1f} "
                  f"| {med([r['tokens'] for r in scored]):.0f} | {sum(r['cost'] for r in mine):.2f} |")
        print("\nmedians per scored run; cost is the arm's total as pi prices it")
        def rate(arm, c):
            s = [r["status"] == "pass" for r in by[(arm, c.key)] if r["status"] in ("pass", "fail")]
            return sum(s) / len(s) if s else None
        names = [a.name for a in arms]
        floor = {c.key: next((r["root"] for r in by[("chains", c.key)]), None) for c in cs}
        for arm in arms:
            scored = [r for c in cs for r in by[(arm.name, c.key)] if r["status"] in ("pass", "fail")]
            wrong = collections.Counter(r["root"] or "(no answer)" for r in scored if r["status"] == "fail")
            if wrong: print(f"{arm.name} wrong answers: " + ", ".join(f"{k} x{n}" for k, n in wrong.most_common()))
            moved = [r for r in scored if floor[r["case"]] is not None and r["root"] != floor[r["case"]]]
            if arm.name != "chains" and "chains" in names and scored:
                print(f"{arm.name} departs from chains rank 1 on {len(moved)} of {len(scored)} runs: right on {sum(r['status'] == 'pass' for r in moved)} of them, "
                      f"and rank 1 was right on {sum(floor[r['case']] == r['label'].lower() for r in moved)}")
        shared = [c for c in cs if all(rate(n, c) is not None for n in names)]
        if 0 < len(shared) < len(cs):
            print(f"on the {len(shared)} cases every arm answered, mean AC@1: " + ", ".join(f"{n} {sum(rate(n, c) for c in shared) / len(shared):.2f}" for n in names))
        for a, b in itertools.combinations(names, 2):
            pairs = [(rate(a, c), rate(b, c)) for c in cs]
            pairs = [(x, y) for x, y in pairs if x is not None and y is not None]
            wins, losses = sum(x > y for x, y in pairs), sum(x < y for x, y in pairs)
            if pairs: print(f"{a} against {b}: better on {wins} cases, worse on {losses}, equal on {len(pairs) - wins - losses} (sign test p = {sign_test(wins, losses):.3f})")
        print(f"\n| case | label | " + " | ".join(names) + " |\n|---|---|" + "---|" * len(names))
        for c in cs:
            cells = []
            for arm in arms:
                rs = sorted(by[(arm.name, c.key)], key=lambda r: r["run"])
                cells.append(f"{verdict([r['status'] for r in rs])}: " + ", ".join(r["root"] or r["status"] for r in rs))
            print(f"| {c.id if suite != 'demo' else c.incident} | {c.root} | " + " | ".join(cells) + " |")
    voided = [r for r in rows if r["status"] in ("leak", "error")]
    print(f"\nleak check: {sum(r['status'] == 'leak' for r in rows)} of {len(rows)} runs voided; {sum(r['status'] == 'error' for r in rows)} errors")
    for r in voided: print(f"  {r['status']} {r['arm']} {r['case']} run {r['run']}: {r['why']}")
    if reference and "chains" in [a.name for a in arms]:
        ref = json.loads(pathlib.Path(reference).read_text())["results"]
        cs = [c for c in cases if c.suite.startswith("rcaeval")]
        same = [c for c in cs if {r["root"] for r in by[("chains", c.key)]} == {(ref[c.id]["chain"] or [""])[0].lower()}]
        ours = sum(r["status"] == "pass" for c in cs for r in by[("chains", c.key)]) / max(sum(len(by[("chains", c.key)]) for c in cs), 1)
        theirs = sum((ref[c.id]["chain"] or [""])[0] == c.root for c in cs) / max(len(cs), 1)
        ok = len(same) == len(cs) and abs(ours - theirs) < 1e-12
        print(f"\nGATE chains arm reproduces rcaeval_score.py: {'PASS' if ok else 'FAIL'} ({len(same)}/{len(cs)} cases with the same rank 1 on every run; AC@1 {ours:.4f} against {theirs:.4f})")
        return ok
    return True


def mirror(src, dst):
    src, dst = pathlib.Path(src).resolve(), pathlib.Path(dst).resolve()
    for d in sorted(p for p in src.iterdir() if p.is_dir()):
        (dst / d.name / ".ograph").mkdir(parents=True, exist_ok=True)
        shutil.copy2(d / ".ograph" / "config.toml", dst / d.name / ".ograph" / "config.toml")
        for f in d.iterdir():
            link = dst / d.name / f.name
            if f.is_file() and not link.exists(): link.symlink_to(f)
    shutil.copy2(src / "labels.json", dst / "labels.json")
    print(f"mirrored {src} to {dst}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mirror-cases", nargs=2, metavar=("SRC", "DST"))
    ap.add_argument("--bin"); ap.add_argument("--cases"); ap.add_argument("--demo"); ap.add_argument("--out")
    ap.add_argument("--sample", type=int, default=30); ap.add_argument("--seed", type=int, default=12)
    ap.add_argument("--runs", type=int, default=3); ap.add_argument("--arms", default=",".join(ARMS))
    ap.add_argument("--model", default="openai/gpt-5.6-terra"); ap.add_argument("--parallel", type=int, default=4)
    ap.add_argument("--timeout", type=int, default=600); ap.add_argument("--work"); ap.add_argument("--reference")
    ap.add_argument("--misses", action="store_true", help="add the cases --reference ranks wrong, as suite rcaeval-miss")
    ap.add_argument("--report-only", action="store_true", help="run nothing; report the rows OUT/runs.jsonl holds")
    ap.add_argument("--fresh", action="store_true"); ap.add_argument("--keep", action="store_true", help="keep each run's workspace")
    ap.add_argument("--strict", action="store_true", help="an arm that cannot run fails the harness instead of being skipped")
    a = ap.parse_args(argv)
    if a.mirror_cases: return mirror(*a.mirror_cases)
    if not a.bin or not a.out or not (a.cases or a.demo): ap.error("--bin, --out and one of --cases or --demo are required")
    unknown = [n for n in a.arms.split(",") if n not in ARMS]
    if unknown: ap.error(f"unknown arm {', '.join(unknown)}; arms are {', '.join(ARMS)}")
    if a.runs < 1 or a.parallel < 1: ap.error("--runs and --parallel must be 1 or more")
    ctx = Ctx(a); ctx.setup()
    if a.misses and not (a.cases and a.reference): ap.error("--misses needs --cases and --reference")
    cases = rcaeval_cases(ctx.cases_dir, a.sample, a.seed) if a.cases else []
    if a.misses: cases += missed_cases(ctx.cases_dir, a.reference, {c.id for c in cases})
    if a.demo: cases += demo_cases(pathlib.Path(a.demo).resolve())
    arms, skipped = [], {}
    for name in a.arms.split(","):
        why = ARMS[name].unavailable and ARMS[name].unavailable(ctx)
        if why and a.strict: raise SystemExit(f"arm {name} cannot run: {why}")
        if why: skipped[name] = why
        else: arms.append(ARMS[name])
    out = pathlib.Path(a.out); out.mkdir(parents=True, exist_ok=True)
    log = out / "runs.jsonl"
    wanted = {(c.key, arm.name, n) for c in cases for arm in arms for n in range(1, a.runs + 1)}
    done = {}
    if log.exists() and not a.fresh:
        for line in log.read_text().splitlines():
            r = json.loads(line)
            if r["status"] != "error": done[(r["case"], r["arm"], r["run"])] = r
    todo = [(c, arm, n) for n in range(1, a.runs + 1) for c in cases for arm in arms if (c.key, arm.name, n) not in done and not a.report_only]
    print(f"{len(todo)} runs to do, {len(wanted & set(done))} kept from {log}", file=sys.stderr, flush=True)
    t = time.time()
    with concurrent.futures.ThreadPoolExecutor(a.parallel) as pool:
        for job in concurrent.futures.as_completed([pool.submit(one, ctx, *job, out) for job in todo]):
            row = job.result()
            done[(row["case"], row["arm"], row["run"])] = row
            log.write_text("".join(json.dumps(done[k]) + "\n" for k in sorted(done)))
            print(f"{row['arm']:<6} {row['case']} run {row['run']}: {row['status']} {row['root']} {row['wall_s']} s {row['why'][:120]}", file=sys.stderr, flush=True)
    if todo: print(f"{len(todo)} runs in {time.time() - t:.0f} s at --parallel {a.parallel}", file=sys.stderr)
    if not a.work: shutil.rmtree(ctx.work, ignore_errors=True)
    ok = report(cases, arms, skipped, [done[k] for k in sorted(done) if k in wanted], a.runs, a.reference, a.model)
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
