#!/usr/bin/env python3
"""A deliberately broken model for the gateway gate: answers from a fixed rotation of failure
modes, chosen by a hash of the input so runs are repeatable. Every JSON answer also claims calls
between applications: one with a quote that is not in the thread, and, when the thread names two
applications, the reverse of the order they appear in, quoted from the thread itself."""
import hashlib, json, re, sys
text = sys.stdin.read().rsplit("The thread:\n", 1)[-1].strip()
apps = ["web-frontend", "checkout", "payments-api", "orders-api", "search-api", "inventory", "auth-service", "ledger-db", "notifications", "elasticsearch"]
named = sorted((m.start(), a) for a in apps for m in [re.search(re.escape(a), text)] if m)
calls = [{"from": "checkout", "to": "ledger-db", "quote": "checkout reads ledger-db directly"}]
if len(named) >= 2:
    calls.append({"from": named[1][1], "to": named[0][1], "quote": text})
# Salt 0 spreads the 16 demo threads over all six failure modes.
k = int(hashlib.sha256(("0" + text).encode()).hexdigest(), 16) % 6
answers = [
    {"category": "OUTAGE", "sentiment": "calm", "apps": [], "confidence": 0.9},
    {"category": "CAPACITY", "sentiment": "furious", "apps": ["checkout"], "confidence": 0.9},
    {"category": "DATA", "sentiment": "calm", "apps": ["made-up-service"], "confidence": 0.9},
    {"category": "NETWORK", "sentiment": "urgent", "apps": ["checkout"], "confidence": 0.2},
    {"category": "DEPENDENCY", "sentiment": "concerned", "apps": ["web-frontend"], "confidence": 0.8},
]
print("Sure! The category is probably capacity." if k == 0 else json.dumps({**answers[k - 1], "calls": calls}))
