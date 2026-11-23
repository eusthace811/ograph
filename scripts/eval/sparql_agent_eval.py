#!/usr/bin/env python3
"""Agent-written SPARQL on examples/demo/sparql-questions.json.

An agent (headless pi with the skills/ograph skill, bash and read) answers each question by
writing SPARQL. It ends its reply with its final query; that query is run and its rows must
equal the rows of the question's reference query, as a set, on some choice and order of its
columns (extra columns are allowed, extra or missing rows are not). Doubles compare by value.
"recipe" questions are close to a recipe in skills/ograph/references/sparql.md; "novel" ones
are not.

--no-skill runs the same agent without the skill, to measure what the skill adds."""
import argparse, atexit, concurrent.futures, decimal, itertools, json, os, pathlib, re, shutil, statistics, subprocess, tempfile, time

ap = argparse.ArgumentParser()
ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
ap.add_argument("--model", default="openai/gpt-5.6-terra"); ap.add_argument("--repeat", type=int, default=1)
ap.add_argument("--parallel", type=int, default=2); ap.add_argument("--no-skill", action="store_true"); ap.add_argument("--only", type=int, nargs="*")
a = ap.parse_args()
if a.repeat < 1 or a.parallel < 1: ap.error("--repeat and --parallel must be 1 or more")
binp = pathlib.Path(a.bin).resolve(); demo = pathlib.Path(a.demo).resolve()
skill = pathlib.Path(__file__).resolve().parents[2] / "skills" / "ograph"
if not (demo / "sparql-questions.json").is_file(): ap.error(f"{demo} has no sparql-questions.json")
questions = json.loads((demo / "sparql-questions.json").read_text())

work = pathlib.Path(tempfile.mkdtemp(prefix="ograph-sparql-agent-"))
atexit.register(shutil.rmtree, work, True)
ws = work / "demo"
shutil.copytree(demo, ws, ignore=shutil.ignore_patterns("sparql-questions.json", "*-labels.json", "questions*.txt", "generate.py"))
bindir = work / "bin"; bindir.mkdir(); (bindir / "ograph").symlink_to(binp)
env = {**os.environ, "PATH": f"{bindir}{os.pathsep}{os.environ['PATH']}"}

def rows(q):
    try:
        p = subprocess.run([str(binp), "--root", str(ws), "--json", "sparql", q], capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired:
        return None, "query ran over 60 s"
    if p.returncode == 1: return [], None
    if p.returncode != 0: return None, " ".join(p.stderr.split())[:300] or f"exit {p.returncode}"
    answer = json.loads(p.stdout)
    return (answer["rows"], None) if "rows" in answer else (None, f"a {answer['kind']} answer, not SELECT rows")

# A node and its name are the same answer: `id:app/ledger-db` counts as `ledger-db`, `id:inc/inc-5377` as `INC-5377`.
names, err = rows("SELECT ?s ?n WHERE { { ?s ops:name ?n } UNION { ?s inc:id ?n } }")
if err: raise SystemExit(f"{demo} is not an ograph workspace: {err}")
NAMES = dict(names)
def cell(c):
    # --json prints a double as a typed literal, an integer or decimal bare, and an unbound
    # variable as null. Other typed literals, xsd:float among them, compare as text.
    m = isinstance(c, str) and re.fullmatch(r'"(.*)"\^\^<http://www\.w3\.org/2001/XMLSchema#double>', c)
    if not m: return NAMES.get(c, c)
    try: x = float(m[1]) + 0.0  # a double's own range; + 0.0 turns -0 into 0
    except ValueError: return c
    return format(decimal.Decimal(repr(x)).normalize(), "f")
def named(rs): return [[cell(c) for c in r] for r in rs]

def same(got, gold):
    got, gold = named(got), named(gold)
    want = {tuple(r) for r in gold}
    if not got: return False
    return any({tuple(r[i] for i in cols) for r in got} == want for cols in itertools.permutations(range(len(got[0])), len(gold[0])))

PROMPT = """Answer this question about the ograph workspace in the current directory by writing SPARQL and running it with `ograph sparql`. Use only `ograph sparql`, not the other ograph commands. Run as many queries as you need.

Question: {q}

End your reply with the one SPARQL query whose result answers the question, in a ```sparql block."""

def ask(q, n):
    for attempt in range(3):
        # An empty reply, a failed exit or a hang is the provider (rate limit, outage), not the agent.
        try:
            # stdin closed: pi -p waits for piped input otherwise.
            p = subprocess.run(["pi", "-p", "--no-session", "--no-extensions", "--no-context-files", "--no-prompt-templates",
                                "--no-skills", *([] if a.no_skill else ["--skill", str(skill)]), "--tools", "bash,read", "--model", a.model,
                                PROMPT.format(q=q["q"])], cwd=ws, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=300)
            if p.returncode == 0 and p.stdout.strip(): break
            why = f"exit {p.returncode}: {(p.stderr or p.stdout).strip()[-200:]}"
        except subprocess.TimeoutExpired:
            why = "no reply within 300 s"
        if attempt < 2: time.sleep(30 * (attempt + 1))
    else:
        return None, f"pi failed 3 times, last: {why}"
    blocks = re.findall(r"```sparql\s*\n(.*?)```", p.stdout, re.S)
    if not blocks: return False, "no ```sparql block in the reply"
    got, err = rows(blocks[-1])
    if err: return False, f"final query failed: {err}"
    ok = same(got, gold[n])
    return ok, "" if ok else f"got {sorted(map(tuple, got), key=str)[:6]} want {sorted(map(tuple, gold[n]), key=str)[:6]}"

def timed(item):
    n, q = item
    t = time.time()
    return (n, *ask(q, n), time.time() - t)

todo = [(n, q) for n, q in enumerate(questions, 1) if not a.only or n in a.only]
if not todo: raise SystemExit(f"no question matches --only; there are {len(questions)}")
gold = {}
for n, q in todo:
    gold[n], err = rows(q["gold"])
    if err or not gold[n]: raise SystemExit(f"Q{n} reference query: {err or 'no rows; harvest the demo first (cd examples/demo && ograph harvest)'}")
print(f"model {a.model}, {len(todo)} questions x {a.repeat}, " + ("no skill" if a.no_skill else "skill skills/ograph"))
results, times = {}, []
with concurrent.futures.ThreadPoolExecutor(a.parallel) as pool:
    for n, ok, why, secs in pool.map(timed, [t for t in todo for _ in range(a.repeat)]):
        results.setdefault(n, []).append(ok); times.append(secs)
        q, verdict = questions[n - 1], "ERROR" if ok is None else "PASS" if ok else "FAIL"
        print(f"Q{n:<2} {q['kind']:<6} {verdict} {secs:5.1f} s  {q['q'][:70]}" + (f"\n       {why}" if why else ""), flush=True)
for kind in ("recipe", "novel", "all"):
    runs = [ok for n, oks in results.items() if kind == "all" or questions[n - 1]["kind"] == kind for ok in oks]
    scored = [ok for ok in runs if ok is not None]
    unscored = len(runs) - len(scored)
    print(f"{kind}: {sum(scored)}/{len(scored)} correct ({sum(scored) / max(len(scored), 1):.2f})"
          + (f"; {unscored} not scored: pi failed" if unscored else ""))
print(f"median time per run: {statistics.median(times):.1f} s")
