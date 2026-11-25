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
# Threads that name applications the way people do (a short name, an alias, a word of the
# name) and threads whose words only look like an application (git checkout, an inventory of
# hosts, a vendor). apps-labels.json lists, per thread root, the catalog applications each
# thread above and here refers to. Read only by scripts/eval/catalog_slice_gate.py; no demo
# source reads chat-mentions.
MENTION_THREADS = [
    (["auth is throwing 401s for every login since the 14:00 deploy", "rolled auth back, logins work again"], ["auth-service"]),
    (["frontend is serving a blank page in safari, looks like the new bundle"], ["web-frontend"]),
    (["notif service queue is 40k deep, emails are going out an hour late"], ["notifications"]),
    (["ES heap at 95%, GC pauses of 8s on every data node"], ["elasticsearch"]),
    (["pay gw returning 502s to checkout since 11:20"], ["payments-api", "checkout"]),
    (["finder p99 is 3s, the query cache got disabled in the last release"], ["search-api"]),
    (["ledger-pg replication lag is 15 minutes"], ["ledger-db"]),
    (["the ledger database ran out of connections again", "orders and payments are both erroring because of it"], ["ledger-db", "orders-api", "payments-api"]),
    (["inventory svc reports negative stock for 200 SKUs after the migration"], ["inventory"]),
    (["git checkout of the release branch fails on the build agents, the CI cache is corrupt"], []),
    (["doing an inventory of our EC2 hosts before the audit, please tag yours by friday"], []),
    (["CardRail is down for all merchants per their status page, nothing on our side"], []),
    (["can't search the wiki, Confluence is down for everyone"], []),
    (["PagerDuty notifications were not reaching phones for an hour, their incident, resolved now"], []),
    (["the payments service retries every bank call 5 times, which tripled our load on CardRail"], ["payments-api"]),
    (["orders backlog cleared after we doubled the workers"], ["orders-api"]),
    (["checkout svc and auth svc both leak connections after the java upgrade"], ["checkout", "auth-service"]),
    (["the frontend can't reach checkout, DNS still resolves to the old IP"], ["web-frontend", "checkout"]),
    (["search pods crashlooping since the config push", "elastic itself is fine, it's the search-api config"], ["search-api", "elasticsearch"]),
    (["frontend team offsite next week, no deploys from them until monday"], []),
    (["mail sends failing, MailRelay answers 503, notifications keeps retrying"], ["notifications"]),
    (["rolled back pay-gw to 2.2.9, card auths are going through again"], ["payments-api"]),
    (["stock reservations double counted in inventory", "turned the reserve_v2 flag off"], ["inventory"]),
    (["the postgres primary behind the ledger is at 100% disk"], ["ledger-db"]),
    (["web-frontend errors spiked; it's the CDN, the vendor confirmed"], ["web-frontend"]),
    (["elasticsearch upgrade to 8.12 done, finder reindex running overnight"], ["elasticsearch", "search-api"]),
    (["the orders API returns 504 whenever the export job runs"], ["orders-api"]),
    (["please search the runbooks before paging, half of tonight's pages were known issues"], []),
    (["auth-svc certificate expires tomorrow, renewing it tonight"], ["auth-service"]),
    (["payments team asks who approved the checkout of the old ledger schema branch"], []),
]
# Held-out set B, written after the first slice failed on the set above and before the second
# was built or run. Same mix: short names, aliases, words of a name, and words that only look
# like one.
HELDOUT_THREADS = [
    (["orders svc 5xx since the db failover", "ledger looks fine now, orders still erroring"], ["orders-api", "ledger-db"]),
    (["web front end can't load product images, CDN cache purge pending"], ["web-frontend"]),
    (["notification service stopped sending push since midnight"], ["notifications"]),
    (["elastic cluster red after the node replacement"], ["elasticsearch"]),
    (["paygw timeouts to the bank, card auths failing"], ["payments-api"]),
    (["finder returns empty results for every query since 9am"], ["search-api"]),
    (["auth latency doubled after the token cache change", "auth team rolled it back"], ["auth-service"]),
    (["checkout svc OOM on 3 of 8 hosts"], ["checkout"]),
    (["ledger-pg vacuum is blocking writes"], ["ledger-db"]),
    (["inventory svc can't reach the ledger, connection refused"], ["inventory", "ledger-db"]),
    (["the payments service and the checkout service disagree on totals for EUR orders"], ["payments-api", "checkout"]),
    (["search latency is up, the search service is waiting on elastic"], ["search-api", "elasticsearch"]),
    (["frontend 404s on /cart after the router change"], ["web-frontend"]),
    (["notif backlog is 12k and climbing"], ["notifications"]),
    (["orders API rejects every request with 401", "auth svc rotated its signing key an hour ago"], ["orders-api", "auth-service"]),
    (["the ledger db is at 90% storage"], ["ledger-db"]),
    (["pay gw p99 at 2s, ledger reads are slow"], ["payments-api", "ledger-db"]),
    (["ES snapshot job failed overnight, disk full on the repo bucket"], ["elasticsearch"]),
    (["auth is fine, the problem was the corporate VPN"], ["auth-service"]),
    (["inventory counts are off by one after the reserve change"], ["inventory"]),
    (["please checkout the new on-call handbook before your shift"], []),
    (["the order of the deploy steps in the wiki is wrong, fixing it"], []),
    (["Okta is down, nobody can log in to Grafana"], []),
    (["search the incident channel before opening a new ticket"], []),
    (["finance asked for an inventory of our cloud spend by team"], []),
    (["front end guild meetup is moved to thursday"], []),
    (["MailRelay changed their API keys, the vendor emailed us"], []),
    (["slack notifications for deploys are duplicated, the bot posts twice"], []),
    (["the auth token for the build bot expired, CI is red"], []),
    (["payment of the AWS invoice is late, finance is on it"], []),
]
OPS_APPS = [["payments-api", "ledger-db"], ["checkout"], ["auth-service"], ["orders-api"], ["ledger-db"], ["search-api", "elasticsearch"], ["notifications"], ["web-frontend", "checkout"], ["inventory", "checkout"], ["auth-service"], ["web-frontend", "checkout"], ["ledger-db"], ["payments-api"], ["search-api"], [], ["orders-api"]]
ACTION_APPS = [["checkout"], ["payments-api"], ["orders-api"], ["ledger-db"], ["auth-service"], ["search-api"], ["inventory"], ["notifications"], ["checkout"], ["web-frontend"], ["elasticsearch"], ["payments-api"], ["orders-api"], ["ledger-db", "payments-api"], ["ledger-db"], ["checkout"], ["auth-service"], ["search-api"], ["inventory"], ["notifications"], ["web-frontend"], ["checkout", "payments-api"], ["ledger-db"], ["orders-api"], ["auth-service"], ["elasticsearch"], ["payments-api"], ["web-frontend"], ["inventory"], ["checkout"], ["payments-api", "ledger-db"]]
TM = T0 - 40 * 86400
mmsgs = []
apps_labels = {"ops": {f"{T0 - (len(THREADS) - i) * 3600}.000100": a for i, a in enumerate(OPS_APPS)}, "incidents": {f"{r}.000200": a for r, a in zip(roots, ACTION_APPS)}, "mentions": {}}
for i, (texts, apps) in enumerate(MENTION_THREADS):
    ts = [f"{TM + i * 3600 + k * 60}.000300" for k in range(len(texts))]
    mmsgs += [{"ts": t, "thread_ts": ts[0], "channel": "mentions", "user": f"U{k + 2}", "text": x} for k, (t, x) in enumerate(zip(ts, texts))]
    apps_labels["mentions"][ts[0]] = apps
for i, (texts, apps) in enumerate(HELDOUT_THREADS):
    ts = [f"{TM - 86400 + i * 1800 + k * 60}.000400" for k in range(len(texts))]
    mmsgs += [{"ts": t, "thread_ts": ts[0], "channel": "heldout", "user": f"U{k + 2}", "text": x} for k, (t, x) in enumerate(zip(ts, texts))]
    apps_labels.setdefault("heldout", {})[ts[0]] = apps
pathlib.Path("chat-mentions").mkdir(exist_ok=True)
pathlib.Path("chat-mentions/mentions.json").write_text(json.dumps(mmsgs, indent=1))
pathlib.Path("apps-labels.json").write_text(json.dumps(apps_labels, indent=1))
news = [
    {"at": NOW - 7200, "title": "CardRail reports elevated authorization latency", "summary": "CardRail says authorizations are slow for some merchants in North America", "mentions": ["CardRail"]},
    {"at": NOW - 86400, "title": "MailRelay deprecates v1 send API", "summary": "MailRelay will turn off the v1 send API in 60 days", "mentions": ["MailRelay"]},
    {"at": NOW - 3600, "title": "Chip supply news", "summary": "Unrelated hardware news", "mentions": ["SomeChipCo"]},
]
w("news.jsonl", news)
print("demo data written; incident INC-5377 opened at", NOW)
