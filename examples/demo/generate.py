#!/usr/bin/env python3
"""Deterministic demo workspace for ograph: a small company's catalog, telemetry exports, past
incidents with mitigations, chat threads with gold labels (threads-labels.json, read only by the
eval), and news. Run from this directory: python3 generate.py"""
import json, math, random, subprocess, pathlib
R = random.Random(7)
T0 = 1795300000  # 2026-11-21 ~19:46 UTC, seconds
APPS = {
    "web-frontend": ["checkout", "search-api", "auth-service"],
    "checkout": ["payments-api", "inventory", "auth-service", "orders-api"],
    "payments-api": ["ledger-db", "auth-service"],
    "orders-api": ["ledger-db", "notifications"],
    "search-api": ["elasticsearch"],
    "inventory": ["ledger-db"],
    "auth-service": [], "ledger-db": [], "notifications": [], "elasticsearch": [],
}
docs = []
for a, deps in APPS.items():
    d = f"apiVersion: backstage.io/v1alpha1\nkind: Component\nmetadata:\n  name: {a}\nspec:\n  type: service\n  owner: team-{a.split('-')[0]}\n"
    if deps:
        d += "  dependsOn:\n" + "".join(f"    - component:{x}\n" for x in deps)
    docs.append(d)
pathlib.Path("catalog").mkdir(exist_ok=True)
pathlib.Path("catalog/catalog-info.yaml").write_text("---\n".join(docs))
if not pathlib.Path("catalog/.git").exists():
    subprocess.run(["git", "init", "-q"], cwd="catalog")
subprocess.run(["git", "add", "-A"], cwd="catalog")
subprocess.run(["git", "-c", "user.name=demo", "-c", "user.email=demo@example.invalid", "commit", "-qm", "catalog", "--allow-empty"], cwd="catalog")
own = [
    {"app": "web-frontend", "process": "online sales"}, {"app": "checkout", "process": "online sales"},
    {"app": "payments-api", "process": "card payments"}, {"app": "ledger-db", "process": "card payments"},
    {"app": "orders-api", "process": "order fulfilment"}, {"app": "notifications", "process": "customer notifications"},
    {"app": "payments-api", "vendor": "CardRail"}, {"app": "notifications", "vendor": "MailRelay"},
    {"app": "elasticsearch", "vendor": "SearchCloud"}, {"app": "payments-api", "alias": "payments"},
]
w = lambda name, rows: pathlib.Path(name).write_text("".join(json.dumps(r) + "\n" for r in rows))
w("ownership.jsonl", own)
NOW = T0 + 86400
incidents = [
    {"id": "INC-4120", "title": "Card payments failing", "service": "payments-api", "opened": T0 - 20 * 86400, "severity": "SEV2", "status": "resolved", "resolved": T0 - 20 * 86400 + 3600, "mitigation": "raised ledger-db connection pool to 200"},
    {"id": "INC-4388", "title": "Checkout errors after release", "service": "checkout", "opened": T0 - 12 * 86400, "severity": "SEV2", "status": "resolved", "resolved": T0 - 12 * 86400 + 1800, "mitigation": "rolled back checkout v4.1.0"},
    {"id": "INC-4501", "title": "Search timeouts", "service": "search-api", "opened": T0 - 5 * 86400, "severity": "SEV3", "status": "resolved", "resolved": T0 - 5 * 86400 + 7200, "mitigation": "replaced failed elasticsearch data nodes"},
    {"id": "INC-5377", "title": "Checkout latency and payment errors", "service": "checkout", "opened": NOW, "severity": "SEV2", "status": "open"},
]
pathlib.Path("incidents.json").write_text(json.dumps(incidents, indent=1))
deploys = [
    {"service": "checkout", "version": "4.1.0", "at": T0 - 12 * 86400 - 120},
    {"service": "payments-api", "version": "2.3.1", "at": NOW - 240},
    {"service": "search-api", "version": "1.9.0", "at": NOW - 3 * 3600},
]
w("deployments.jsonl", deploys)
w("config.jsonl", [{"service": "ledger-db", "key": "max_connections", "at": NOW - 480}, {"service": "auth-service", "key": "fast_auth", "at": T0 - 2 * 86400}])
alerts = {"alerts": [
    {"labels": {"alertname": "LedgerPoolSaturated", "service": "ledger-db", "severity": "critical"}, "startsAt": NOW - 420},
    {"labels": {"alertname": "PaymentsErrorRate", "service": "payments-api", "severity": "critical"}, "startsAt": NOW - 180},
    {"labels": {"alertname": "CheckoutLatencyP99", "service": "checkout", "severity": "warning"}, "startsAt": NOW - 60},
    {"labels": {"alertname": "PaymentsErrorRate", "service": "payments-api", "severity": "critical"}, "startsAt": T0 - 20 * 86400 - 200},
    {"labels": {"alertname": "LedgerPoolSaturated", "service": "ledger-db", "severity": "critical"}, "startsAt": T0 - 20 * 86400 - 400},
    {"labels": {"alertname": "SearchTimeouts", "service": "search-api", "severity": "warning"}, "startsAt": T0 - 5 * 86400 - 300},
    # Old alerts that name services by the aliases the runbooks state (pay-gw, finder).
    {"labels": {"alertname": "PayGwQueueDepth", "service": "pay-gw", "severity": "warning"}, "startsAt": T0 - 8 * 86400},
    {"labels": {"alertname": "FinderSlowQueries", "service": "finder", "severity": "warning"}, "startsAt": T0 - 9 * 86400},
]}
pathlib.Path("alerts.json").write_text(json.dumps(alerts))
# Runbooks: three aliases stated outright, and decoys that must not become aliases.
pathlib.Path("runbooks").mkdir(exist_ok=True)
pathlib.Path("runbooks/payments.md").write_text("""# payments-api runbook

payments-api, also known as pay-gw, authorizes card payments through CardRail.
It reads balances from ledger-db (ledger-pg) and authenticates callers with auth-service.
ledger-db (postgres) runs on the managed database tier; the primary database (ledger-db) is shared with orders-api.

## History
Past incidents: payments-api (INC-4120) and checkout (INC-4388). The rounding change shipped in payments-api (v2.3.1).
Some teams say orders-api is also called checkout, because it was split out of the old checkout backend.
payments-api is also called by web-frontend; ledger-db (read-only) replicas serve the monthly reports.
""")
pathlib.Path("runbooks/platform.md").write_text("""# Platform notes

search-api (aka finder) answers product search from elasticsearch.
notifications sends email through MailRelay.
web-frontend talks to checkout (see the payments runbook).
""")
pathlib.Path("runbooks-labels.json").write_text(json.dumps({"aliases": {"payments-api": ["pay-gw"], "ledger-db": ["ledger-pg"], "search-api": ["finder"]},
    "decoys": ["postgres", "ledger-db", "INC-4120", "INC-4388", "v2.3.1", "checkout", "by", "read-only"]}, indent=1))
lines = []
for s in range(NOW - 3600, NOW + 120, 2):
    lines.append({"ts": s, "service": "payments-api", "message": f"charge ok id={R.randint(1000,9999)} took {R.randint(20,80)}ms"})
    if s >= NOW - 400 and R.random() < 0.8:
        lines.append({"ts": s, "service": "payments-api", "message": f"ERROR ledger-db connection pool exhausted after 3000ms waiting=" + str(R.randint(10, 99))})
    if s >= NOW - 200 and R.random() < 0.5:
        lines.append({"ts": s, "service": "checkout", "message": f"payment call failed: upstream timeout order={R.randint(1,99999)}"})
w("logs.jsonl", lines)
cols = ["time", "ledger-db_connections", "ledger-db_cpu", "payments-api_latency-p99", "checkout_latency-p99", "search-api_cpu"]
rows = [",".join(cols)]
for s in range(NOW - 3600, NOW + 120, 10):
    bad = s >= NOW - 480
    rows.append(",".join(str(x) for x in [s, (195 if bad else 60) + R.gauss(0, 4), 30 + R.gauss(0, 3), (900 if s >= NOW - 300 else 120) + R.gauss(0, 15), (1500 if s >= NOW - 150 else 200) + R.gauss(0, 20), 40 + R.gauss(0, 4)]))
pathlib.Path("metrics.csv").write_text("\n".join(rows) + "\n")
THREADS = [
    ("payments-api latency spiked right after the 2.3.1 deploy, but traces show the ledger-db connection pool exhausted on every request since 13:50, before the deploy. Raised pool size to 200, latency recovered. INC-4120", "CAPACITY"),
    ("checkout 500s started 2 min after we rolled out checkout v4.1.0, NullPointerException in the new PromoService code path, rolled back, errors gone. INC-4388", "DEPLOYMENT"),
    ("someone flipped feature flag fast_auth=true in the prod config of auth-service at 09:12, p99 jumped from 80ms to 2s, flipped back, fixed", "CONFIG"),
    ("orders-api pods OOMKilled during the traffic peak, HPA maxed out at 40 replicas, bumped max to 80 and the memory limit, stable now", "CAPACITY"),
    ("does anyone know who owns the ledger-db terraform module? need to request a quota change next week", "UNKNOWN"),
    ("search-api timeouts again; the elasticsearch cluster shows red status, 2 data nodes lost disks. Infra replacing nodes. No deploys today. INC-4501", "DEPENDENCY"),
    ("notifications are delayed because MailRelay is returning 503s for every send since 10:00; our queue and workers look healthy", "DEPENDENCY"),
    ("TLS handshake failures between web-frontend and checkout after the load balancer certificate expired at midnight, renewed it", "NETWORK"),
    ("the inventory migration dropped the reserved_qty column default, so new rows have nulls and checkout shows wrong stock", "DATA"),
    ("we saw a burst of login attempts from one ASN against auth-service using leaked credentials, blocked the range and forced resets", "SECURITY"),
    ("web-frontend DNS lookups for checkout failing intermittently from one zone, resolvers there were overloaded, moved to the other resolver pool", "NETWORK"),
    ("ledger-db disk filled up at 03:00 because WAL archiving stopped, cleared space and restarted archiving", "CAPACITY"),
    ("payments-api started returning 400s after we changed the currency rounding setting in its config from 2 to 0 decimals, reverted the setting", "CONFIG"),
    ("rolled out search-api 1.9.0 and the new query parser rejects quoted phrases, rolling back to 1.8.4", "DEPLOYMENT"),
    ("CardRail authorization API latency went from 200ms to 4s for all merchants, their status page confirms an incident, nothing wrong on our side", "DEPENDENCY"),
    ("a bad batch file from the partner left duplicate order rows in orders-api, wrote a dedupe script and reconciled", "DATA"),
]
msgs = []
for i, (text, _) in enumerate(THREADS):
    ts = T0 - (len(THREADS) - i) * 3600
    msgs.append({"ts": f"{ts}.000100", "channel": "ops", "user": "U1", "text": text})
pathlib.Path("discussions").mkdir(exist_ok=True)
pathlib.Path("discussions/ops.json").write_text(json.dumps(msgs, indent=1))
pathlib.Path("threads-labels.json").write_text(json.dumps({f"{T0 - (len(THREADS) - i) * 3600}.000100": lab for i, (_, lab) in enumerate(THREADS)}, indent=1))
# Multi-message threads with operator actions, labeled (kind, app, message number) in
# actions-labels.json, read only by the eval. Some threads only propose, ask or plan: they
# have no action. The last two follow past incidents, so `similar` lists what was done.
ACTION_THREADS = [
    (["checkout 5xx after the 4.2.0 deploy", "should we roll back checkout?", "rolled back checkout to 4.1.9, errors dropping"], [("ROLLBACK", "checkout", 3)]),
    (["payments-api pods stuck in a crash loop", "restarted payments-api pods, they came back healthy"], [("RESTART", "payments-api", 2)]),
    (["orders-api queue is growing", "let's scale orders-api to 20 replicas", "nah, it drained on its own"], []),
    (["ledger-db primary is unresponsive in us-east-1a", "failed over ledger-db to the replica in us-east-1b", "writes are flowing again"], [("FAILOVER", "ledger-db", 2)]),
    (["auth-service p99 jumped after enabling new_token_cache", "turned off the new_token_cache flag on auth-service", "latency back to normal"], [("FLAG", "auth-service", 2)]),
    (["search-api timing out on long queries", "raised the search-api request timeout from 2s to 5s in its config"], [("CONFIG", "search-api", 2)]),
    (["inventory has a fix for the stock bug ready", "deployed inventory 3.4.1 with the fix", "stock numbers look right now"], [("DEPLOY", "inventory", 2)]),
    (["notifications are failing to send", "can someone restart notifications?", "I'll do it after the standup"], []),
    (["checkout memory keeps climbing", "bounced checkout on all hosts", "memory flat again, will keep an eye on it"], [("RESTART", "checkout", 2)]),
    (["web-frontend is slow from europe", "maybe we should fail over web-frontend to eu-west?", "no, it's the CDN, not us"], []),
    (["elasticsearch cluster yellow", "added two data nodes to elasticsearch", "cluster green"], [("SCALE", "elasticsearch", 2)]),
    (["payments-api error rate up after config push", "reverted the payments-api retry setting to 3", "errors gone"], [("CONFIG", "payments-api", 2)]),
    (["orders-api 4.0 broke the export job", "we did not roll back orders-api, the job owners patched it instead"], []),
    (["ledger-db connections maxed", "raised max_connections on ledger-db to 400", "ok", "also restarted payments-api to drop the stale pool"], [("CONFIG", "ledger-db", 2), ("RESTART", "payments-api", 4)]),
    (["planning: next week we will fail over ledger-db to test the runbook"], []),
    (["checkout canary looks bad", "rolled back the checkout canary", "full fleet was never touched"], [("ROLLBACK", "checkout", 2)]),
    (["auth-service login errors", "flipped fast_login off for auth-service", "still errors", "rolled back auth-service to 2.8.0", "fixed"], [("FLAG", "auth-service", 2), ("ROLLBACK", "auth-service", 4)]),
    (["search-api is at 90% cpu", "scaled search-api from 6 to 12 instances"], [("SCALE", "search-api", 2)]),
    (["inventory sync is stuck", "if it doesn't recover in 10 min we restart inventory", "it recovered"], []),
    (["notifications backlog", "scaled notifications workers to 30", "backlog cleared in 15 min"], [("SCALE", "notifications", 2)]),
    (["web-frontend shows a blank page for some users", "deployed web-frontend 9.1.2 hotfix", "blank pages gone"], [("DEPLOY", "web-frontend", 2)]),
    (["checkout and payments-api both erroring", "should we restart checkout or payments-api first?", "restarted payments-api first", "checkout recovered by itself"], [("RESTART", "payments-api", 3)]),
    (["ledger-db replication lag at 20 minutes", "we could fail over ledger-db but the replica is behind too", "waiting it out"], []),
    (["orders-api throwing NPEs after 5.2.0", "rolled back orders-api to 5.1.3", "NPEs gone"], [("ROLLBACK", "orders-api", 2)]),
    (["auth-service rate limit too low for the sale", "bumped the auth-service rate limit to 2000 rps in config"], [("CONFIG", "auth-service", 2)]),
    (["elasticsearch master node down", "restarted the elasticsearch master", "cluster rejoined"], [("RESTART", "elasticsearch", 2)]),
    (["payments-api latency high", "is it worth scaling payments-api?", "not yet, let's watch it"], []),
    (["web-frontend us-east is down", "moved web-frontend traffic to us-west", "users are back"], [("FAILOVER", "web-frontend", 2)]),
    (["inventory feature flag reserve_v2 causing double reservations", "disabled reserve_v2 for inventory", "no more doubles"], [("FLAG", "inventory", 2)]),
    (["INC-4388 checkout 500s after v4.1.0", "rolled back checkout to 4.0.9", "errors gone, closing"], [("ROLLBACK", "checkout", 2)]),
    (["INC-4120 payments failing, ledger-db pool exhausted", "raised the ledger-db connection pool to 200", "payments recovering"], [("CONFIG", "ledger-db", 2)]),
]
TA = T0 - 30 * 86400
roots = [TA + i * 7200 for i in range(len(ACTION_THREADS) - 2)] + [T0 - 12 * 86400 + 300, T0 - 20 * 86400 + 300]
amsgs, alabels = [], {}
for root, (texts, acts) in zip(roots, ACTION_THREADS):
    ts = [f"{root + k * 120}.000200" for k in range(len(texts))]
    amsgs += [{"ts": t, "thread_ts": ts[0], "channel": "incidents", "user": f"U{k + 2}", "text": x} for k, (t, x) in enumerate(zip(ts, texts))]
    alabels[ts[0]] = [{"kind": k, "app": a, "ts": ts[m - 1]} for k, a, m in acts]
pathlib.Path("chat-actions").mkdir(exist_ok=True)
pathlib.Path("chat-actions/actions.json").write_text(json.dumps(amsgs, indent=1))
pathlib.Path("actions-labels.json").write_text(json.dumps(alabels, indent=1))
news = [
    {"at": NOW - 7200, "title": "CardRail reports elevated authorization latency", "summary": "CardRail says authorizations are slow for some merchants in North America", "mentions": ["CardRail"]},
    {"at": NOW - 86400, "title": "MailRelay deprecates v1 send API", "summary": "MailRelay will turn off the v1 send API in 60 days", "mentions": ["MailRelay"]},
    {"at": NOW - 3600, "title": "Chip supply news", "summary": "Unrelated hardware news", "mentions": ["SomeChipCo"]},
]
w("news.jsonl", news)
print("demo data written; incident INC-5377 opened at", NOW)
