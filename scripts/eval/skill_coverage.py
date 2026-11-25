#!/usr/bin/env python3
"""Does the ograph skill cover what an agent can use, and do its examples run?

1. Coverage. Every subcommand and flag of the real binary's --help, every positional value it
   lists (`ontology check|diff|coverage`), every config key and table, source kind and source
   option in docs/REFERENCE.md, and the agent-facing items below that no --help lists, against
   the text of skills/ograph (SKILL.md and references/*.md). A flag counts as mentioned when one
   line or paragraph names both its command and the flag.
2. ogrep integrations. Each user-visible behavior the ogrep-backed commits added, with the
   pattern the skill must match to cover it.
3. Examples. Every `ograph ...` line in a ```bash block runs, in block order, in a fresh harvested
   copy of examples/demo per skill file. The other lines of a block run first as its setup
   (`cp`, `printf`). A line must exit 0, or the code its comment states (`exit 1`, `(exit 1)`).
   A block with `ograph serve` is not run: it does not exit.

Exclusions are fixed below, each with its reason, and were written before the first run.

Usage: skill_coverage.py --bin target/release/ograph [--rev REV] [--demo examples/demo] [--no-run]
  --rev reads skills/ograph and docs/REFERENCE.md from a git revision instead of the tree.
Exit 0 when nothing is unmentioned and every example exits as documented."""
import argparse, datetime, os, pathlib, re, shlex, shutil, subprocess, sys, tempfile

REPO = pathlib.Path(__file__).resolve().parents[2]

EXCLUDED = {
    "help": "clap's own subcommand",
    "global --help": "clap's own flag",
    "global --version": "clap's own flag",
    **{f"config [detect] {k}": "detector threshold: it changes which events exist, a person tunes it against docs/EVIDENCE.md, and the skill points at docs/REFERENCE.md for the list"
       for k in ("metric_z", "metric_baseline_s", "metric_min_run", "metric_min_history_s", "log_burst_factor", "log_min_count", "trace_error_rate", "trace_slow_factor", "two_sided")},
    **{f"config [llm.extract] {k}": "how the model is called, set once by a person; the skill says what the table enables"
       for k in ("command", "model", "parallel", "timeout_s")},
}

# What no --help lists but an agent reaches: (item, pattern over the skill text).
EXTRA = [
    ("./x install-hook", r"\./x install-hook"),
    ("MCP resource ograph://incidents", r"ograph://incidents"),
    ("MCP resource ograph://ontology", r"ograph://ontology"),
    ("MCP resource template ograph://incident/{id}", r"ograph://incident/\{id\}"),
    ("MCP prompt investigate", r"`investigate \{incident\}`"),
    ("MCP tools resolve and cite_check", r"\bcite_check\b"),
    ("chains field scores", r"`scores`"),
    ("chains field weakest", r"`weakest`"),
    ("chains field edge and its values", r"`edge`[^\n]*`trace`"),
    ("chains field precedent", r"`precedent`"),
    ("similar field ruled_out", r"ruled.out"),
    ("fault classes ([incidents] classify, inc:faultClass)", r"classify = true"),
    ("verdict history (rca:supersededBy, why lists it)", r"supersededBy"),
    ("[verdict] evidence policy", r"require_evidence"),
    ("validate focus counts per shape", r"(?s)ograph validate.{0,400}focus"),
    ("ontology check lints (warnings)", r"(?s)ontology check.{0,1200}warning"),
    ("status --json fields", r"`unjudged`"),
    ("history_days", r"history_days"),
    ("lab panels", r"(?s)ograph lab.{0,600}neighbourhood"),
    ("operator actions (on by default, actions = false, OPERATOR_ACTION)", r"actions = false"),
    ("[incidents] merge policy", r"\[incidents\][^\n]*merge|merge = \{"),
]

# The user-visible behaviors the ogrep-backed commits added (c7f2e06 ... 57184ae on main).
OGREP = [
    ("c9abd75", "`ograph code INC`: lines behind the events, commits, files, write-ups", r"ograph code INC"),
    ("c9abd75", "`code --top` limits files and write-ups", r"(?s)ograph code[^\n]*--top|`--top` limits"),
    ("c9abd75", "`code` lists `degraded` when ogrep cannot answer for a repository", r"`degraded`"),
    ("c9abd75", "`code` in serve/MCP", r"`code \{incident, top\?\}`"),
    ("10eeba2", "`ograph suspects INC|APP[,APP] --since`", r"ograph suspects [a-z]+,[a-z]+ --since"),
    ("10eeba2", "`chains` falls back to suspects when nothing ranks (code source)", r"chains`? prints the same list"),
    ("10eeba2", "other incidents in the window count as failing for suspects", r"Other incidents opened within"),
    ("a2dacb8", "`code` source kind, its `since` option", r"kind = \"code\"[^\n]*since"),
    ("a2dacb8", "`errors` source kind, EXCEPTION events with stack-frame lines", r"kind = \"errors\""),
    ("a2dacb8", "COMMIT and EXCEPTION signals", r"COMMIT, EXCEPTION"),
    ("a2dacb8", "`ogrep index .` makes lookups fast, ograph does not build it", r"ogrep index \."),
    ("c7f2e06/7fac5b9", "commit-to-file prior: added then dropped; the skill must not offer code_pair_prior", None),
    ("b7471ca", "ogrep calls of a harvest run side by side: speed only, no flag or output", None),
    ("fa935b8", "catalog takes calls ogrep infers from configuration; `inferred = false`", r"inferred = false"),
    ("fa935b8", "aliases from a Kubernetes Service in front of a workload (ogrep 0.3+)", r"Kubernetes Service"),
    ("fa935b8", "LOG_NEW and `[detect] log_new_after_s`", r"log_new_after_s"),
    ("fa935b8", "`code` names the commit that first added a message; needs `ogrep index --history`", r"ogrep index --history"),
    ("fa935b8", "deployment files between two revisions; `code` lists `deployments` with `touches`", r"deployments before it"),
    ("fa935b8", "`suspects` for an incident stops at its open time", r"(?s)suspects.{0,300}(?:until it opened|stops at)"),
    ("c975ff8", "with ogrep 0.2.0, deployments keep no files and the harvest line says DEGRADED", r"(?s)(?:0\.3 or newer|newer than 0\.2\.0).{0,200}DEGRADED|DEGRADED.{0,200}(?:0\.3 or newer|newer than 0\.2\.0)"),
    ("fa935b8", "with ogrep 0.2.0, suspects counts later commits too and a note says so", r"0\.2\.0[^\n]*(?:later commits|note)|(?:later commits|note)[^\n]*0\.2\.0"),
    ("57184ae", "ogrep hand-over from a located line: --related-tests, --callers", r"ogrep --related-tests"),
    ("a2dacb8", "`OGRAPH_OGREP` points at another ogrep binary", r"OGRAPH_OGREP"),
    ("b7471ca/serve", "serve re-harvests when a code repository's .git/logs/HEAD changes", r"\.git/logs/HEAD"),
]


def source(rev, rel):
    if not rev: return (REPO / rel).read_text()
    return subprocess.run(["git", "-C", str(REPO), "show", f"{rev}:{rel}"], capture_output=True, text=True, check=True).stdout


def skill_files(rev):
    if not rev:
        root = REPO / "skills" / "ograph"
        return {str(p.relative_to(root)): p.read_text() for p in [root / "SKILL.md", *sorted((root / "references").glob("*.md"))]}
    names = subprocess.run(["git", "-C", str(REPO), "ls-tree", "-r", "--name-only", rev, "skills/ograph"], capture_output=True, text=True, check=True).stdout.split()
    names = sorted((n for n in names if n.endswith(".md")), key=lambda n: (n != "skills/ograph/SKILL.md", n))
    return {n.removeprefix("skills/ograph/"): source(rev, n) for n in names}


def binary_items(bin):
    """(item, command, flag) for every subcommand, flag and listed positional value."""
    top = subprocess.run([bin, "--help"], capture_output=True, text=True).stdout
    cmds = re.findall(r"^  ([a-z][\w-]*)\s{2,}", top.split("Commands:")[1].split("Options:")[0], re.M)
    items = [(c, c, None) for c in cmds]
    items += [(f"global {f}", None, f) for f in re.findall(r"^\s+(?:-\w, )?(--[\w-]+)", top.split("Options:")[1], re.M)]
    for c in cmds:
        if c == "help": continue
        h = subprocess.run([bin, c, "--help"], capture_output=True, text=True).stdout
        for f in re.findall(r"^\s+(?:-\w, )?(--[\w-]+)", h.split("Options:")[1], re.M):
            if f not in ("--root", "--json", "--help"): items.append((f"{c} {f}", c, f))
        for vals in re.findall(r"\[possible values: ([^\]]+)\]", h.split("Options:")[0]):
            items += [(f"{c} {v.strip()}", c, v.strip()) for v in vals.split(",")]
    return items


def reference_items(ref):
    conf = ref.split("## Configuration")[1].split("```toml")[1].split("```")[0]
    items, table = [], None
    for line in conf.splitlines():
        if m := re.match(r"^\[\[?([\w.]+)\]\]?", line):
            table = m[1]
            if table != "source": items.append((f"config [{table}]", rf"\[{re.escape(table)}\]"))
        elif (m := re.match(r"^(\w+)\s*=", line)) and table != "source":
            key = m[1]
            items.append((f"config [{table}] {key}" if table else f"config {key}", rf"\b{key}\b"))
    formats = ref.split("## Input formats")[1].split("\n## ")[0]
    for kind in re.findall(r"^\| `(\w+)` \|", formats, re.M)[1:]:
        items.append((f"source kind {kind}", rf'kind = "{kind}"|`{kind}` source|\b{kind}\b source'))
    for opt in sorted(set(re.findall(r"Optional `(\w+)", formats))):
        items.append((f"source option {opt}", rf"\b{opt} = "))
    return items


def paragraphs(text):
    """Each prose paragraph and each code line on its own, so a flag counts for a command only
    when one of them names both."""
    out = []
    for k, part in enumerate(text.split("```")):
        out += part.splitlines() if k % 2 else part.split("\n\n")
    return out


def mentioned_flag(paras, cmd, flag):
    if cmd is None: return any(re.search(rf"(?<![\w-]){re.escape(flag)}\b", p) for p in paras)
    name = rf"(?:ograph(?:\s+--json)?\s+|`|\b){re.escape(cmd)}\b"
    if flag.startswith("--"):
        return any(re.search(name, p) and re.search(rf"(?<![\w-]){re.escape(flag)}\b", p) for p in paras)
    return any(re.search(rf"ograph(?:\s+--json)?\s+{re.escape(cmd)}\b[^\n#]*\s{re.escape(flag)}\b", p) for p in paras)


def coverage(bin, files, ref):
    text = "\n\n".join(files.values())
    paras = paragraphs(text)
    rows = []
    for item, cmd, flag in binary_items(bin):
        if flag is None: ok = bool(re.search(rf"\bograph(?:\s+--json)?\s+{re.escape(cmd)}\b", text))
        else: ok = mentioned_flag(paras, cmd, flag)
        rows.append((item, ok))
    rows += [(item, bool(re.search(pat, text))) for item, pat in reference_items(ref)]
    rows += [(item, bool(re.search(pat, text))) for item, pat in EXTRA]
    return rows


def blocks(text):
    """The lines of every ```bash block, a line continued while its quotes are open."""
    for k, part in enumerate(text.split("```")):
        if k % 2 == 0 or not part.startswith("bash"): continue
        lines, cur = [], ""
        for raw in part.splitlines()[1:]:
            cur = f"{cur}\n{raw}" if cur else raw
            try: shlex.split(cur, comments=True)
            except ValueError: continue
            if cur.strip(): lines.append(cur)
            cur = ""
        yield lines


def expected_exit(line):
    lex = shlex.shlex(line, posix=True); lex.whitespace_split = True; lex.commenters = ""
    words, comment = [], ""
    try:
        for tok in lex:
            if tok.startswith("#") and not comment: comment = "#"
            if comment: comment += " " + tok
            else: words.append(tok)
    except ValueError: pass
    if re.search(NOT_RUN, comment): return words, None
    m = re.search(r"\(exit (\d)\)\s*$", comment)
    return words, int(m[1]) if m else 0


# A line whose comment says one of these needs what examples/demo does not have.
NOT_RUN = r"needs a model|in a workspace with a code source"
# The demo store's history starts when the script harvests it, so a time before that is an
# error (docs/REFERENCE.md). Such a line runs again with the time set to now, which the history
# covers, and must then exit 0 or 1.
YOUNG = re.compile(r"the history of this store starts at (\S+) ")

ANSWER = """The strongest candidate for INC-5377 is ledger-db, id:chain/inc-5377-a7370ff7d4358525 (rank 1),
a hypothesis until someone confirms it. It reaches payments-api and checkout.
"""


def run_examples(bin, files, demo, work):
    env = dict(os.environ, PATH=f"{pathlib.Path(bin).resolve().parent}{os.pathsep}{os.environ['PATH']}")
    base = work / "base"
    shutil.copytree(demo, base, symlinks=True, ignore=shutil.ignore_patterns("store.redb", "store.lock", "harvested", "lab.html"))
    h = subprocess.run([bin, "harvest"], cwd=base, capture_output=True, text=True, env=env)
    if h.returncode != 0: raise SystemExit(f"harvest of the demo failed: {h.stderr[-300:]}")
    (base / "answer.md").write_text(ANSWER)
    results = []
    for name, text in files.items():
        ws = work / name.replace("/", "_")
        subprocess.run(["cp", "-cRp" if sys.platform == "darwin" else "-Rp", str(base), str(ws)], check=True)
        for lines in blocks(text):
            if any(re.match(r"\s*ograph serve\b", l) for l in lines):
                results += [(name, l.strip(), None, None, "not run: serve does not exit") for l in lines if l.lstrip().startswith("ograph ")]
                continue
            for line in lines:
                stripped = line.lstrip()
                if stripped.startswith("#"): continue
                if not stripped.startswith("ograph "):
                    subprocess.run(["sh", "-c", line], cwd=ws, capture_output=True, env=env, timeout=60); continue
                words, want = expected_exit(stripped)
                if want is None:
                    results.append((name, stripped.split("\n")[0][:110], None, None, "not run: its comment says it " + re.search(NOT_RUN, stripped)[0])); continue
                p = subprocess.run([bin, *words[1:]], cwd=ws, capture_output=True, text=True, env=env, timeout=300)
                note = (p.stderr.strip().splitlines() or [""])[-1][:160]
                if p.returncode == 2 and (m := YOUNG.search(p.stderr)) and want == 0:
                    now = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=1)).strftime("%Y-%m-%dT%H:%M:%SZ")
                    at = [now if w0 in ("--since", "--known-at") else w for w0, w in zip(["", *words], words)][1:]
                    p = subprocess.run([bin, *at], cwd=ws, capture_output=True, text=True, env=env, timeout=300)
                    note = f"store younger than the example's time; at the time now it exits {p.returncode}" + (f": {p.stderr.strip().splitlines()[-1][:120]}" if p.returncode > 1 else "")
                    if p.returncode == 1: p.returncode = 0
                results.append((name, stripped.split("\n")[0][:110], want, p.returncode, note))
    return results


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True); ap.add_argument("--rev"); ap.add_argument("--demo", default=str(REPO / "examples" / "demo"))
    ap.add_argument("--no-run", action="store_true")
    a = ap.parse_args(argv)
    bin = str(pathlib.Path(a.bin).resolve())
    files, ref = skill_files(a.rev), source(a.rev, "docs/REFERENCE.md")
    print(f"skill: skills/ograph at {a.rev or 'the working tree'} ({', '.join(files)}); binary {bin}")

    rows = coverage(bin, files, ref)
    missing = [i for i, ok in rows if not ok and i not in EXCLUDED]
    print(f"\n1. Coverage: {sum(ok for _, ok in rows)} of {len(rows)} items mentioned; {len(missing)} unmentioned and not excluded")
    for item in missing: print(f"  MISSING {item}")
    for item, why in EXCLUDED.items():
        print(f"  excluded {item}: {why}")

    text = "\n\n".join(files.values())
    print("\n2. ogrep integrations (commit, behavior, covered)")
    og_missing = 0
    for commit, what, pat in OGREP:
        if pat is None:
            state = "n/a (nothing to document)" if "code_pair_prior" not in text else "WRONG: still mentions code_pair_prior"
            og_missing += state.startswith("WRONG")
        else:
            state = "yes" if re.search(pat, text) else "NO"
            og_missing += state == "NO"
        print(f"  {commit:<15} {state:<26} {what}")

    bad = []
    if not a.no_run:
        demo = pathlib.Path(a.demo).resolve()
        if not (demo / "catalog").exists(): raise SystemExit(f"{demo}/catalog is missing: run python3 generate.py in {demo} first")
        with tempfile.TemporaryDirectory(prefix="skillcov-") as d:
            results = run_examples(bin, files, demo, pathlib.Path(d))
        ran = [r for r in results if r[2] is not None]
        bad = [r for r in ran if r[2] != r[3]]
        print(f"\n3. Examples: {len(ran)} run, {len(bad)} not exiting as documented, {len(results) - len(ran)} not run")
        for name, line, want, got, err in results:
            mark = "not run" if want is None else ("ok" if want == got else "FAIL")
            print(f"  {mark:<7} {name}: {line}" + ("" if want is None else f"  (exit {got}, documented {want})") + (f"  {err}" if mark != "ok" or "younger" in err else ""))
    ok = not missing and not og_missing and not bad
    print(f"\nGATE 0 unmentioned items, 0 uncovered ogrep behaviors, 0 failing examples: {'PASS' if ok else 'FAIL'} "
          f"({len(missing)} unmentioned, {og_missing} uncovered, {len(bad)} failing)")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
