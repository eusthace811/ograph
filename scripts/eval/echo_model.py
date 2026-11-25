#!/usr/bin/env python3
"""A free, deterministic model for the cache-key check of catalog_slice_gate.py. It logs the
thread and the known-applications list it was sent, one JSON line per call to the file named by
its argument, and answers with nothing to commit."""
import json, sys
text = sys.stdin.read().rsplit("The thread:\n", 1)[-1]
thread, _, known = text.partition("\n\nKnown applications")
apps = [l[2:].split(" (")[0] for l in known.splitlines() if l.startswith("- ")]
with open(sys.argv[1], "a") as f:
    f.write(json.dumps({"thread": thread.strip(), "known": apps}) + "\n")
print(json.dumps({"category": "UNKNOWN", "cause_quote": None, "sentiment": "calm", "apps": [], "calls": [], "confidence": 0.9}))
