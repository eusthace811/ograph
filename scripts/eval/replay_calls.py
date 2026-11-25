#!/usr/bin/env python3
"""Recover the full output of every tool call in agent_rca_eval.py transcripts.

agent_rca_eval.py keeps each tool output only up to 3000 characters in its transcripts. The
agent saw all of it: an `ograph_chains` answer runs to 10 KB, and the application that misled
the agent is often past the cut. This script re-executes each run's calls, in order, against a
fresh copy of the same case staged the way the harness stages it (agent_rca_eval.stage):

  ograph_*  tools/call on `ograph serve --mcp` in the workspace, the server the mcp arm talked to
  bash      the same command under bash in the workspace, `ograph` on PATH for skill and not raw,
            with the original run's paths mapped onto the replay's
  read      the file, numbered the way Claude Code's Read numbers it

Every replayed output is checked against what the transcript kept: `exact` when the kept text
(after masking harvest timestamps and run paths) starts the replayed output, `close` when the
first 200 masked characters appear in the kept region, `mismatch` otherwise. Where the replay
does not match, or the agent saw a preview only (Claude Code moves an output over ~25 KB to a
file and shows its first 2 KB), the kept text stands in for the output.

The binary must be the one the run used, or one with the same ograph-core and ograph-cli.

Usage:
  replay_calls.py --runs docs/evidence/agent-rca-haiku --bin target/release/ograph \\
      --cases /abs/data/rcaeval-cases [--demo examples/demo] --work /private/tmp/.../replay \\
      --out /private/tmp/.../replay.jsonl [--only KEYFILE] [--wrong-only]

Writes one JSON line per run: {"key", "arm", "case", "run", "root", "label", "calls": [{"tool",
"args", "kept", "truncated", "output", "source"}]}, `source` one of exact, close, mismatch,
preview, kept (no replay), error. Runs already in --out are kept."""
import argparse, collections, json, os, pathlib, re, shutil, subprocess, sys, tarfile, tempfile

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import agent_rca_eval as rca  # noqa: E402

KEEP = 3000
CUT = re.compile(r"^\.\.\. \[(\d+) more characters\]$", re.M)
START = re.compile(r"^\$ ([A-Za-z_]+) (\{.*\})$")
RUN_WS = re.compile(r"(/[\w./@+-]+?)/runs/[0-9a-f]{10}/ops")


def transcripts_dir(runs, work):
    """The transcripts directory of a run directory, unpacking transcripts.tar.gz once."""
    d = runs / "transcripts"
    if d.is_dir(): return d
    tgz = runs / "transcripts.tar.gz"
    out = work / "transcripts" / runs.name
    if not (out / "transcripts").is_dir():
        out.mkdir(parents=True, exist_ok=True)
        with tarfile.open(tgz) as t: t.extractall(out, filter="data")
    return out / "transcripts"


def transcript_path(tdir, row):
    return tdir / row["arm"] / f"{row['case'].replace('/', '_')}-{row['run']}.txt"


def parse(text):
    """A transcript as its calls in order. Each call has its tool, its args, and `region`: the text
    between its batch of calls and the next start, which holds its output (and the outputs of the
    other calls in the same batch, and any reply text the agent wrote before its next call)."""
    lines = text.split("\n")
    calls, batch, region = [], [], []
    def flush():
        body = "\n".join(region)
        for i, c in enumerate(batch): c["region"], c["nth"] = body, i
        batch.clear(); region.clear()
    pending = False
    for line in lines[1:]:
        m = START.match(line)
        if m:
            if not pending: flush()
            try: args = json.loads(m[2])
            except ValueError: args = {"raw": m[2]}
            c = {"tool": m[1], "args": args}
            calls.append(c); batch.append(c); pending = True
        else:
            if line.startswith("init {") and not calls: continue
            pending = False
            region.append(line)
    flush()
    return calls


def kept_output(region, nth=0):
    """The kept text of the `nth` output in a region, by the transcript's own layout: an output
    ends at its cut marker, or at the first blank line (the layout puts one after each)."""
    rest = region.lstrip("\n")
    for _ in range(nth + 1):
        m = CUT.search(rest)
        blank = rest.find("\n\n")
        if m and (blank < 0 or m.start() <= blank + 1 or "<persisted-output>" in rest[:blank]):
            out, rest, truncated = rest[:m.end()], rest[m.end():].lstrip("\n"), True
        elif "<persisted-output>" in rest[:200] and "</persisted-output>" in rest:
            end = rest.index("</persisted-output>") + len("</persisted-output>")
            out, rest, truncated = rest[:end], rest[end:].lstrip("\n"), False
        else:
            end = blank if blank >= 0 else len(rest)
            out, rest, truncated = rest[:end], rest[end:].lstrip("\n"), False
    return out, truncated


MASKS = [(re.compile(r'"harvested_at":"[^"]*"'), '"harvested_at":""'), (re.compile(r"/[\w./@+-]+?/runs/[0-9a-f]{10}/ops"), "<ws>"),
         (re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z(?=\",\"source\")"), "<t>")]


def mask(s):
    for rx, to in MASKS: s = rx.sub(to, s)
    return s


def agreement(kept, region, full):
    k, f = mask(CUT.sub("", kept).rstrip()), mask(full)
    if k and f.startswith(k[:KEEP - 50]): return "exact"
    head = f[:200].strip()
    if head and head in mask(region): return "close"
    return "mismatch"


class Mcp:
    """`ograph serve --mcp` in one workspace, one JSON-RPC line per request."""
    def __init__(self, binp, ws, env):
        self.p = subprocess.Popen([str(binp), "serve", "--mcp"], cwd=ws, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.n = 0
        self.ask("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "replay", "version": "1"}})

    def ask(self, method, params):
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n"); self.p.stdin.flush()
        return json.loads(self.p.stdout.readline())

    def call(self, name, args):
        r = self.ask("tools/call", {"name": name, "arguments": args})
        if "error" in r: return json.dumps(r["error"])
        return "".join(c.get("text", "") for c in r["result"].get("content", []))

    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=30)


def read_file(path, args):
    p = pathlib.Path(path)
    if not p.is_file(): return f"File does not exist: {path}"
    lines = p.read_text(errors="replace").split("\n")
    start = int(args.get("offset") or 1); limit = int(args.get("limit") or 2000)
    return "\n".join(f"{i}\t{l}" for i, l in enumerate(lines[start - 1:start - 1 + limit], start))


def replay_run(ctx, case, arm, calls, map_from):
    """Re-execute a run's calls in order in a fresh workspace; returns one output (or None) per call."""
    ws = rca.workspace(ctx, case, rca.ARMS[arm])
    first = ctx.bin.parent if arm != "raw" else ctx.nobin
    env = ctx.env(first)
    env["HOME"] = str(ws.parent)
    swap = lambda s: s.replace(map_from + "/runs/", "\0").replace(map_from + "/bin", str(ctx.bin.parent)).replace(map_from + "/skills", str(ctx.skill.parent)) if map_from else s
    outs, mcp = [], None
    try:
        for c in calls:
            tool, args = c["tool"], c["args"]
            try:
                if tool.startswith("ograph_"):
                    mcp = mcp or Mcp(ctx.bin, ws, env)
                    outs.append(mcp.call(tool[len("ograph_"):], args))
                elif tool == "bash":
                    cmd = re.sub(r"\0[0-9a-f]{10}/ops", str(ws), swap(args.get("command", "")))
                    p = subprocess.run(["bash", "-c", cmd], cwd=ws, env=env, capture_output=True, text=True, timeout=180, errors="replace")
                    out = (p.stdout + ("\n" + p.stderr if p.stderr else "")).rstrip("\n")
                    outs.append(out)
                elif tool == "read":
                    path = re.sub(r"\0[0-9a-f]{10}/ops", str(ws), swap(args.get("file_path", "")))
                    outs.append(read_file(path, args))
                else: outs.append(None)
            except Exception as e:
                outs.append(None); c["replay_error"] = f"{type(e).__name__}: {e}"[:200]
    finally:
        if mcp: mcp.close()
        shutil.rmtree(ws.parent, ignore_errors=True)
    return outs


def case_of(row, cases_dir, demo):
    suite, cid, inc = row["case"].split("/")
    if suite == "demo": return rca.Case("demo", "demo", inc, row["label"], demo)
    return rca.Case(suite, cid, inc, row["label"], cases_dir / cid)


def recover(ctx, row, text, cases_dir, demo, do_replay=True):
    calls = parse(text)
    m = RUN_WS.search(text)
    full = [None] * len(calls)
    if do_replay and ctx is not None:
        full = replay_run(ctx, case_of(row, cases_dir, demo), row["arm"], calls, m[1] if m else "")
    out = []
    for c, f in zip(calls, full):
        kept, truncated = kept_output(c["region"], c["nth"])
        if "<persisted-output>" in kept: source, output = "preview", kept
        elif f is None: source, output = ("error" if c.get("replay_error") else "kept"), CUT.sub("", kept).rstrip()
        else:
            source = agreement(kept, c["region"], f)
            output = f if source != "mismatch" else CUT.sub("", kept).rstrip()
        out.append({"tool": c["tool"], "args": c["args"], "kept": kept, "truncated": truncated, "output": output, "source": source,
                    **({"error": c["replay_error"]} if c.get("replay_error") else {})})
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--runs", required=True, help="a directory with runs.jsonl and transcripts/ or transcripts.tar.gz")
    ap.add_argument("--bin"); ap.add_argument("--cases"); ap.add_argument("--demo")
    ap.add_argument("--work", required=True); ap.add_argument("--out", required=True)
    ap.add_argument("--only", help="a file of run keys `arm case run`, one per line: replay only those")
    ap.add_argument("--wrong-only", action="store_true", help="only runs that named a wrong application")
    ap.add_argument("--no-replay", action="store_true", help="keep the transcript text only")
    a = ap.parse_args(argv)
    runs, work = pathlib.Path(a.runs).resolve(), pathlib.Path(a.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    tdir = transcripts_dir(runs, work)
    rows = [json.loads(l) for l in (runs / "runs.jsonl").read_text().splitlines()]
    rows = [r for r in rows if r["arm"] != "chains" and transcript_path(tdir, r).exists()]
    if a.wrong_only: rows = [r for r in rows if r["status"] == "fail" and r["root"]]
    if a.only:
        want = {tuple(l.split()[:3]) for l in pathlib.Path(a.only).read_text().splitlines() if l.strip() and not l.startswith("#")}
        rows = [r for r in rows if (r["arm"], r["case"], str(r["run"])) in want]
    out = pathlib.Path(a.out)
    done = {json.loads(l)["key"] for l in out.read_text().splitlines()} if out.exists() else set()
    ctx = None
    if not a.no_replay:
        if not (a.bin and (a.cases or a.demo)): ap.error("--bin and --cases or --demo are needed to replay")
        ctx = rca.Ctx(argparse.Namespace(work=str(work / "ws"), bin=a.bin, cases=a.cases, guidance=None)); ctx.setup()
    cases_dir = pathlib.Path(a.cases).resolve() if a.cases else None
    demo = pathlib.Path(a.demo).resolve() if a.demo else None
    with out.open("a") as f:
        for r in rows:
            key = f"{r['arm']} {r['case']} {r['run']}"
            if key in done: continue
            if r["case"].startswith("demo/") and not demo or not r["case"].startswith("demo/") and not cases_dir:
                calls = recover(None, r, transcript_path(tdir, r).read_text(), cases_dir, demo, False)
            else:
                calls = recover(ctx, r, transcript_path(tdir, r).read_text(), cases_dir, demo, not a.no_replay)
            f.write(json.dumps({"key": key, "arm": r["arm"], "case": r["case"], "run": r["run"], "root": r["root"], "label": r["label"], "status": r["status"], "calls": calls}) + "\n"); f.flush()
            srcs = collections.Counter(c["source"] for c in calls)
            print(f"{key}: {len(calls)} calls, " + ", ".join(f"{k} {v}" for k, v in srcs.most_common()), file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
