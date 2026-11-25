#!/usr/bin/env python3
"""A deliberately broken model for the gateway gate: answers from a fixed rotation of failure
modes, chosen by a hash of the input so runs are repeatable. Every JSON answer also claims calls
between applications: one with a quote that is not in the thread, and, when the thread names two
applications, the reverse of the order they appear in, quoted from the thread itself. It also
reports actions that the checks must refuse."""
import hashlib, json, re, sys
text = sys.stdin.read().rsplit("The thread:\n", 1)[-1].split("\n\nKnown applications", 1)[0].strip()
apps = ["web-frontend", "checkout", "payments-api", "orders-api", "search-api", "inventory", "auth-service", "ledger-db", "notifications", "elasticsearch"]
named = sorted((m.start(), a) for a in apps for m in [re.search(re.escape(a), text)] if m)
calls = [{"from": "checkout", "to": "ledger-db", "quote": "checkout reads ledger-db directly"}]
if len(named) >= 2:
    calls.append({"from": named[1][1], "to": named[0][1], "quote": re.sub(r"(?m)^\[m\d+\] ", "", text)})
# Every answer also reports actions that must not be committed: words that are not in the
# message, a kind outside the vocabulary, an application the thread does not name, a message
# that does not exist, a question as if it were done, and a symptom quoted as a restart.
msgs = re.findall(r"^\[m(\d+)\] (.*)$", text, re.M) or [("1", text)]
first = named[0][1] if named else "checkout"
m1 = msgs[0][1]
actions = [
    {"kind": "ROLLBACK", "app": first, "message": 1, "quote": "rolled back everything"},
    {"kind": "REBOOT", "app": first, "message": 1, "quote": m1},
    {"kind": "RESTART", "app": "made-up-service", "message": 1, "quote": m1},
    {"kind": "SCALE", "app": first, "message": 99, "quote": m1},
    {"kind": "RESTART", "app": first, "message": 1, "quote": m1.split(",")[0]},
] + [{"kind": "ROLLBACK", "app": first, "message": int(n), "quote": t} for n, t in msgs if t.endswith("?")][:1]
# Salt 0 spreads the demo threads over all six failure modes.
k = int(hashlib.sha256(("0" + text).encode()).hexdigest(), 16) % 6
answers = [
    {"category": "OUTAGE", "sentiment": "calm", "apps": [], "confidence": 0.9},
    {"category": "CAPACITY", "sentiment": "furious", "apps": ["checkout"], "confidence": 0.9},
    {"category": "DATA", "sentiment": "calm", "apps": ["made-up-service"], "confidence": 0.9},
    {"category": "NETWORK", "sentiment": "urgent", "apps": ["checkout"], "confidence": 0.2},
    {"category": "DEPENDENCY", "sentiment": "concerned", "apps": ["web-frontend"], "confidence": 0.8},
]
print("Sure! The category is probably capacity." if k == 0 else json.dumps({**answers[k - 1], "calls": calls, "actions": actions}))
