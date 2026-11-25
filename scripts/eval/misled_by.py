#!/usr/bin/env python3
"""Where did a wrong root-cause answer come from? For each run of agent_rca_eval.py that named a
wrong application, find the first tool call whose output presented that application as a
candidate cause, and the signal it showed.

Input: the JSON lines replay_calls.py writes (every call of a run with its full output, recovered
past the transcripts' 3000-character cut). An output presents application X as a candidate when
it attaches a fault signal to X:

  before     an event on X (signal and severity)                    MCP JSON, --json, or text rows
  chains     X among the ranked candidates (rank, root signal)
  suspects   X among the ranked suspects (rank, score)
  similar    a past incident whose accepted cause is X
  why        facts of an event on X
  sparql     a result row naming X next to an event type
  own        a line of the agent's own grep, awk or Python output naming X with a fault word
             (error, timeout, anomaly, spike, deviation, onset, z ...), or, when the command
             itself filtered for errors, naming X with a count
  read       a raw file line from X with a fault word

Not candidates: catalog, ownership and dependency listings, `blast` and `resolve`, a scope list,
CSV headers, the skill's text, and `cite_check` (it echoes the agent's own draft). Several ograph
commands in one Bash call: the tool is the command whose part of the output carries the signal.
The signal reported is the strongest one on X in that output.

Usage:
  misled_by.py --replayed replay.jsonl [--labels docs/evidence/misled-by-labels.txt]
      [--suite rcaeval] [--arms mcp,skill,raw] [--out FILE]

--labels compares the tool (and the call) against hand labels and prints the agreement."""
import argparse, collections, json, pathlib, re, sys

SIGNAL_TYPES = r"METRIC_ANOMALY|LOG_ERROR|LOG_NEW|TRACE_SLOW|TRACE_ERROR|ALERT|DEPLOYMENT|CONFIG_CHANGE|INCIDENT|Alert|Deployment|ConfigChange|MetricAnomaly|LogError|TraceSlow|TraceError"
FAULT = re.compile(r"error|exception|fail|timeout|timed out|refused|unavailable|panic|oom|crash|anomal|spike|jump|onset|deviation|departure|baseline|z[-_ ]?score|\bz ?=|\bsev\b|severity|saturat|exhaust|slow|alert|firing|deploy|rollout|degrad|outlier|5\d\d\b", re.I)
ERROR_FILTER = re.compile(r"error|exception|fail|timeout|refused|unavailable|panic|warn", re.I)
NOT_CANDIDATE = {"blast", "resolve", "cite_check", "verdict", "news", "code", "uses"}
CLI_BEFORE = r"^\s*-\s+[\d.]+\s+min\s+{x}\s+([A-Z_]+)\s+sev\s+([\d.]+)"
CLI_RANK = r"^\s*(\d+)\.\s+{x}\s+score\s+([\d.]+)(.*)$"
CATALOG_LINE = re.compile(r"^\s*(?:-\s*component:|name:|owner:|dependsOn|system:|\{\"app\": \"[^\"]+\", \"(?:process|vendor|alias)\")")


def name_rx(app):
    """X as a word, also as the prefix of a metric column (redis_diskio) or an IRI tail (app/redis)."""
    return re.compile(r"(?<![A-Za-z0-9-])" + re.escape(app) + r"(?![A-Za-z0-9-])")


def sev_of(o):
    for k in ("severity", "score", "confidence"):
        if isinstance(o.get(k), (int, float)): return float(o[k])
    return 0.0


def from_json(doc, app, tool_hint):
    """Hits in one parsed JSON document: [(tool, strength, signal)]."""
    hits = []
    is_app = lambda v: isinstance(v, str) and (v == app or v.endswith("/app/" + app))
    if isinstance(doc, dict) and isinstance(doc.get("apps"), list):  # chains
        for i, a in enumerate(doc["apps"], 1):
            if isinstance(a, dict) and is_app(a.get("app")):
                root = ((a.get("chain") or {}).get("root") or {})
                kind = root.get("kind", "")
                hits.append(("chains", float(a.get("score") or 0), f"chain rank {i} score {float(a.get('score') or 0):.2f}" + (f", root {kind} severity {root.get('severity')}" if kind else "")))
        return hits
    if isinstance(doc, dict) and isinstance(doc.get("events"), list):  # before
        for e in doc["events"]:
            if isinstance(e, dict) and is_app(e.get("app")) and e.get("signal"):
                hits.append(("before", sev_of(e), f"{e['signal']} severity {e.get('severity')}"))
        return hits
    if isinstance(doc, list) and doc and all(isinstance(x, dict) and "rank" in x and "app" in x for x in doc):  # suspects
        for x in doc:
            if is_app(x.get("app")): hits.append(("suspects", float(x.get("score") or 0), f"suspect rank {x['rank']} score {x.get('score')}"))
        return hits
    if isinstance(doc, list) and doc and all(isinstance(x, dict) and ("incident" in x or "id" in x) for x in doc):  # similar
        for x in doc:
            text = json.dumps(x)
            if name_rx(app).search(text): hits.append(("similar", float(x.get("score") or 0), f"past incident {x.get('incident') or x.get('id')}"))
        return hits
    if isinstance(doc, dict) and "facts" in doc and "node" in doc:  # why
        node = str(doc.get("node", ""))
        on = any(f.get("predicate") == "evt:on" and is_app(str(f.get("object", "")).replace("id:app/", "/app/")) for f in doc["facts"] if isinstance(f, dict))
        if on and "/ev/" in node.replace("id:ev/", "/ev/"): hits.append(("why", 0.0, f"facts of event {node}"))
        return hits
    rows = doc.get("rows") if isinstance(doc, dict) else None
    if isinstance(rows, list):  # sparql
        for r in rows:
            text = json.dumps(r)
            if name_rx(app).search(text) and re.search(SIGNAL_TYPES + r"|/ev/", text): hits.append(("sparql", 0.0, "query row " + text[:80]))
        return hits
    return hits


def from_json_text(s, app):
    """Hits in ograph JSON cut short (`| head -c`): its flat event objects and ranked entries."""
    hits, a = [], re.escape(app)
    for o in re.findall(r"\{[^{}]*\}", s):
        if re.search(rf'"app":"{a}"', o) and (m := re.search(r'"signal":"([A-Z_]+)"', o)):
            sev = re.search(r'"severity":([\d.]+)', o)
            hits.append(("before", float(sev[1]) if sev else 0.0, f"{m[1]} severity {sev[1] if sev else '?'}"))
    ranked = re.findall(r'\{"app":"([^"]+)","score":([\d.]+)', s)
    for i, (x, score) in enumerate(ranked, 1):
        if x == app: hits.append(("chains", float(score), f"chain rank {i} score {float(score):.2f}"))
    return hits


def from_text(text, app, tool, command):
    """Hits in text output, line by line; JSON lines are parsed as documents."""
    rx = name_rx(app)
    hits = []
    before_rx, rank_rx = re.compile(CLI_BEFORE.format(x=re.escape(app))), re.compile(CLI_RANK.format(x=re.escape(app)))
    errors_cmd = bool(ERROR_FILTER.search(command or ""))
    for line in text.split("\n"):
        if not rx.search(line): continue
        s = line.strip()
        if s[:1] in "[{":
            try: doc = json.loads(s)
            except ValueError: doc = None
            if doc is not None and not (isinstance(doc, dict) and "message" in doc):
                hits += from_json(doc, app, tool); continue
            if doc is None and re.match(r'^[\[{]"(?:incident|app|rank|apps|events)"', s):
                hits += from_json_text(s, app); continue
        if m := before_rx.search(line):
            hits.append(("before", float(m[2]), f"{m[1]} severity {m[2]}")); continue
        if m := rank_rx.search(line):
            kind = "suspects" if re.search(r"distance|explains", m[3]) else "chains"
            hits.append((kind, float(m[2]), f"{'suspect' if kind == 'suspects' else 'chain'} rank {m[1]} score {m[2]}")); continue
        if re.search(rf"^\s*id:chain/\S+\s+[\d.]+:\s+{re.escape(app)}:", line):
            continue  # the chain row under a rank line already counted
        if re.search(r"evt:on\s+id:app/" + re.escape(app) + r"\b", line):
            hits.append(("why", 0.0, "facts of an event on it")); continue
        if CATALOG_LINE.search(line) or "scope:" in line or line.count(",") > 8 and "time" in line.split(",")[0]:
            continue
        if re.search(r"is depended on by|dependents|depends on", line): continue
        if tool == "read" and re.search(r"/skills?/|SKILL\.md", command or ""): continue
        own = "read" if tool == "read" else "own"
        if m := re.search(r'"message":\s*"([^"]*)"', line):
            if FAULT.search(m[1]) and not re.search(r"\b(?:ok|success|succeeded)\b", m[1], re.I): hits.append((own, 0.0, "log line: " + m[1][:70]))
            continue
        col = re.search(re.escape(app) + r"_[\w.-]+", line)
        what = f"metric figures for {col[0]}" if col else "fault words"
        if FAULT.search(line.replace(app, "")):
            hits.append((own, 0.0, f"{what}: " + s[:90])); continue
        if errors_cmd and re.search(r"\d", line.replace(app, "")) and re.search(rf"['\"]?{re.escape(app)}[\w.-]*['\"]?\s*[,:)\s]\s*\d|\d+\s*[,(']+\s*['\"]?{re.escape(app)}", line):
            hits.append((own, 0.0, (f"{what}" if col else "count after an error filter") + ": " + s[:80]))
    return hits


def attribute(run):
    """(call number, tool, signal) of the first call presenting the wrong app, or None."""
    app = run["root"]
    for n, c in enumerate(run["calls"], 1):
        tool = c["tool"].removeprefix("ograph_")
        if tool in NOT_CANDIDATE: continue
        out = c.get("output") or ""
        if not name_rx(app).search(out): continue
        command = c["args"].get("command") or c["args"].get("file_path") or c["args"].get("query") or ""
        hits = []
        if c["tool"].startswith("ograph_"):
            try: hits = from_json(json.loads(out), app, tool)
            except ValueError: hits = []
            hits = [(tool, *h[1:]) for h in hits]
        else:
            hits = from_text(out, app, tool, command)
        if hits:
            first_tool = hits[0][0]
            mine = [h for h in hits if h[0] == first_tool]
            best = max(mine, key=lambda h: h[1])
            return n, first_tool, best[2]
    return None


def load_labels(path):
    labels = {}
    for line in pathlib.Path(path).read_text().splitlines():
        m = re.match(r"^(mcp|skill|raw)\s+(\S+)\s+(\d+)\s+(\d+)\s+(\w+)\s+(.*)$", line)
        if m: labels[f"{m[1]} {m[2]} {m[3]}"] = (int(m[4]), m[5], m[6])
    return labels


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--replayed", required=True, action="append", help="replay_calls.py output; repeatable")
    ap.add_argument("--labels"); ap.add_argument("--suite", help="only cases of this suite (rcaeval, rcaeval-miss, demo, ...)")
    ap.add_argument("--arms", default="mcp,skill,raw"); ap.add_argument("--out")
    a = ap.parse_args(argv)
    runs = [json.loads(l) for f in a.replayed for l in pathlib.Path(f).read_text().splitlines() if l.strip()]
    runs = [r for r in runs if r["status"] == "fail" and r["root"] and r["arm"] in a.arms.split(",")]
    if a.suite: runs = [r for r in runs if r["case"].split("/")[0] == a.suite]
    lines = []
    say = lambda s="": lines.append(s)
    results = {r["key"]: attribute(r) for r in runs}
    if a.labels:
        labels = load_labels(a.labels)
        shared = [k for k in labels if k in results]
        tool_ok = sum(results[k] is not None and results[k][1] == labels[k][1] for k in shared)
        call_ok = sum(results[k] is not None and results[k][0] == labels[k][0] and results[k][1] == labels[k][1] for k in shared)
        say(f"agreement with {a.labels}: tool {tool_ok}/{len(shared)} ({tool_ok / max(len(shared), 1):.2f}), tool and call {call_ok}/{len(shared)}")
        for k in shared:
            got = results[k]
            mark = "ok " if got and got[1] == labels[k][1] else "DIFF"
            say(f"  {mark} {k}: label call {labels[k][0]} {labels[k][1]}; script " + (f"call {got[0]} {got[1]}: {got[2]}" if got else "none"))
        say()
    by_arm = collections.defaultdict(list)
    for r in runs: by_arm[r["arm"]].append(r)
    for arm in [x for x in a.arms.split(",") if by_arm[x]]:
        rs = by_arm[arm]
        tools = collections.Counter(results[r["key"]][1] if results[r["key"]] else "none" for r in rs)
        say(f"{arm}: {len(rs)} wrong runs; first saw the wrong application in " + ", ".join(f"{t} {n}" for t, n in tools.most_common()))
        sig = collections.Counter()
        for r in rs:
            got = results[r["key"]]
            if not got: continue
            s = re.sub(r"rank (\d+) score [\d.]+", r"rank \1", got[2])
            s = re.sub(r"(severity) ([\d.]+)", lambda m: f"{m[1]} {'1.0' if float(m[2]) >= 0.995 else '0.5-0.99' if float(m[2]) >= 0.5 else '<0.5'}", s)
            s = re.sub(r"^(metric figures|fault words|log line|count after an error filter)\b.*", r"\1", s)
            sig[(got[1], s)] += 1
        for (t, s), n in sig.most_common(6):
            say(f"  {n:>3}  {t:<9} {s}")
        apps = collections.Counter(r["root"] for r in rs)
        say("  wrong applications: " + ", ".join(f"{k} x{v}" for k, v in apps.most_common()))
    report = "\n".join(lines)
    print(report)
    if a.out:
        pathlib.Path(a.out).write_text(json.dumps({k: v for k, v in results.items()}, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
