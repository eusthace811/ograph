#!/usr/bin/env python3
"""Agent-written SPARQL on examples/demo/sparql-questions.json.

An agent (headless pi with the skills/ograph skill, bash and read) answers each question by
writing SPARQL. It ends its reply with its final query; that query is run and its rows must
equal the rows of the question's reference query, as a set, on some choice and order of its
columns (extra columns are allowed, extra or missing rows are not). "recipe" questions are close
to a recipe in skills/ograph/references/sparql.md; "novel" ones are not.

Usage: sparql_agent_eval.py --bin target/release/ograph --demo examples/demo [--model M] [--repeat N] [--no-skill]
--no-skill runs the same agent without the skill, to measure what the skill adds."""
import argparse, concurrent.futures, itertools, json, os, pathlib, re, shutil, statistics, subprocess, tempfile, time

ap = argparse.ArgumentParser()
ap.add_argument("--bin", required=True); ap.add_argument("--demo", required=True)
ap.add_argument("--model", default="openai/gpt-5.6-terra"); ap.add_argument("--repeat", type=int, default=1)
ap.add_argument("--parallel", type=int, default=2); ap.add_argument("--no-skill", action="store_true"); ap.add_argument("--only", type=int, nargs="*")
a = ap.parse_args()
binp = pathlib.Path(a.bin).resolve(); demo = pathlib.Path(a.demo).resolve()
skill = pathlib.Path(__file__).resolve().parents[2] / "skills" / "ograph"
questions = json.loads((demo / "sparql-questions.json").read_text())

work = pathlib.Path(tempfile.mkdtemp(prefix="ograph-sparql-agent-"))
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
NAMES = dict(rows("SELECT ?s ?n WHERE { { ?s ops:name ?n } UNION { ?s inc:id ?n } }")[0])
def named(rs): return [[NAMES.get(c, c) for c in r] for r in rs]

def same(got, gold):
    got, gold = named(got), named(gold)
    want = {tuple(r) for r in gold}
    if not got: return not want
    k = len(gold[0]) if gold else 1
    return any({tuple(r[i] for i in cols) for r in got} == want for cols in itertools.permutations(range(len(got[0])), k))

PROMPT = """Answer this question about the ograph workspace in the current directory by writing SPARQL and running it with `ograph sparql`. Use only `ograph sparql`, not the other ograph commands. Run as many queries as you need.

Question: {q}

End your reply with the one SPARQL query whose result answers the question, in a ```sparql block."""

def ask(item):
    n, q = item
    t = time.time()
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
        time.sleep(30 * (attempt + 1))
    else:
        return n, None, f"pi failed 3 times, last: {why}", time.time() - t
    blocks = re.findall(r"```sparql\s*\n(.*?)```", p.stdout, re.S)
    if not blocks: return n, False, "no ```sparql block in the reply", time.time() - t
    got, err = rows(blocks[-1])
    if err: return n, False, f"final query failed: {err}", time.time() - t
    ok = same(got, gold[n])
    return n, ok, "" if ok else f"got {sorted(map(tuple, got), key=str)[:6]} want {sorted(map(tuple, gold[n]), key=str)[:6]}", time.time() - t

todo = [(n, q) for n, q in enumerate(questions, 1) if not a.only or n in a.only]
gold = {}
for n, q in todo:
    gold[n], err = rows(q["gold"])
    if err: raise SystemExit(f"Q{n} reference query failed: {err}")
print(f"model {a.model}, {len(todo)} questions x {a.repeat}, " + ("no skill" if a.no_skill else "skill skills/ograph"))
results, times = {}, []
with concurrent.futures.ThreadPoolExecutor(a.parallel) as pool:
    for n, ok, why, secs in pool.map(ask, [t for t in todo for _ in range(a.repeat)]):
        results.setdefault(n, []).append(ok); times.append(secs)
        print(f"Q{n:<2} {questions[n - 1]['kind']:<6} {'ERROR' if ok is None else 'PASS' if ok else 'FAIL'} {secs:5.1f} s  {questions[n - 1]['q'][:70]}" + (f"\n       {why}" if why else ""), flush=True)
for kind in ("recipe", "novel", "all"):
    runs = [ok for n, oks in results.items() if kind == "all" or questions[n - 1]["kind"] == kind for ok in oks]
    scored = [ok for ok in runs if ok is not None]
    print(f"{kind}: {sum(scored)}/{len(scored)} correct ({sum(scored) / max(len(scored), 1):.2f})" + (f"; {len(runs) - len(scored)} not scored: pi failed" if len(scored) < len(runs) else ""))
print(f"median time per run: {statistics.median(times):.1f} s")
shutil.rmtree(work)
