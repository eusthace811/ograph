#!/usr/bin/env python3
"""A model command for `[llm.extract]` that asks headless Claude Code instead of pi, for gates
that harvest with a model. ograph writes the prompt on stdin and reads the answer on stdout, as
with pi; Claude Code owns the login, so ograph still holds no credentials.

The call is isolated as pi's `--no-tools --no-session --no-extensions --no-skills
--no-context-files --thinking off` is: no tools, no user or project settings (so no hooks), no
memory, no MCP servers, no skills or slash commands, no session saved, thinking off, and a
neutral working directory so no CLAUDE.md is found. Each call's total_cost_usd is appended to
--cost-log when given.

Usage in config.toml:
  [llm.extract]
  command = ["python3", "scripts/eval/claude_model.py", "--model", "claude-haiku-5-5", "--cost-log", "/tmp/cost.jsonl"]
  model = "claude:claude-haiku-5-5"
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time

ap = argparse.ArgumentParser()
ap.add_argument("--model", default="claude-haiku-5-5")
ap.add_argument("--cost-log")
ap.add_argument("--budget", default="0.50", help="--max-budget-usd for one call, a safety cap")
a = ap.parse_args()

claude = shutil.which("claude") or os.path.expanduser("~/.local/bin/claude")
env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE") and k != "AI_AGENT"}
cmd = [claude, "-p", "--output-format", "json", "--model", a.model, "--tools", "", "--setting-sources", "",
       "--settings", json.dumps({"autoMemoryEnabled": False, "alwaysThinkingEnabled": False}), "--strict-mcp-config",
       "--disable-slash-commands", "--no-session-persistence", "--max-budget-usd", a.budget]
with tempfile.TemporaryDirectory(prefix="model-") as cwd:
    p = subprocess.run(cmd, input=sys.stdin.read(), capture_output=True, text=True, cwd=cwd, env=env)
try:
    r = json.loads(p.stdout)
except ValueError:
    sys.exit(f"claude exit {p.returncode}: {(p.stderr or p.stdout).strip()[-300:]}")
if a.cost_log:
    with open(a.cost_log, "a") as f:
        f.write(json.dumps({"t": round(time.time(), 1), "cost": r.get("total_cost_usd", 0), "models": sorted(r.get("modelUsage") or {}),
                            "error": bool(r.get("is_error"))}) + "\n")
if p.returncode != 0 or r.get("is_error"):
    sys.exit(f"claude failed ({r.get('subtype')}): {str(r.get('result') or p.stderr).strip()[-300:]}")
print(r.get("result", ""))
