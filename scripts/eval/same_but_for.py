#!/usr/bin/env python3
"""Says whether two JSON files are equal once the named keys are removed at every depth: the
check that an answer gained fields and changed nothing else.

Usage: same_but_for.py OLD.json NEW.json KEY [KEY...]
"""
import json, sys

def strip(v, keys):
    if isinstance(v, dict):
        return {k: strip(x, keys) for k, x in v.items() if k not in keys}
    if isinstance(v, list):
        return [strip(x, keys) for x in v]
    return v

old, new, keys = sys.argv[1], sys.argv[2], set(sys.argv[3:])
a, b = json.load(open(old)), json.load(open(new))
same = a == strip(b, keys)
print(f"{'identical' if same else 'DIFFERENT'} but for {', '.join(sorted(keys))}: {new.rsplit('/', 1)[-1]}")
sys.exit(0 if same else 1)
