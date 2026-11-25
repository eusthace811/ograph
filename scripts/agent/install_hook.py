#!/usr/bin/env python3
"""Adds ograph's session-start note to a Claude Code settings file (default
~/.claude/settings.json): a SessionStart hook that runs `ograph status`, which prints one line
when the graph is stale or incidents wait for a verdict and nothing otherwise. Running it again
changes nothing.

Usage: install_hook.py [SETTINGS_JSON]"""
import json, pathlib, sys

COMMAND = "ograph status 2>/dev/null || true"
path = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else pathlib.Path.home() / ".claude" / "settings.json")
text = path.read_text() if path.exists() else ""
settings = json.loads(text) if text.strip() else {}
starts = settings.setdefault("hooks", {}).setdefault("SessionStart", [])
if any(h.get("command") == COMMAND for group in starts for h in group.get("hooks", [])):
    print(f"the ograph session-start hook is already in {path}")
else:
    starts.append({"hooks": [{"type": "command", "command": COMMAND}]})
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(settings, indent=2) + "\n")
    print(f"added the ograph session-start hook to {path}")
