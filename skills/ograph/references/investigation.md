# Investigating an incident

The four questions ograph answers natively, in the order an investigation uses them.

## 1. What happened before it

```bash
ograph before INC-5377
```

```
id:inc/inc-5377 on checkout at 2026-11-22T22:26:40Z; scope: checkout(0) auth-service(1) inventory(1) orders-api(1) payments-api(1) ledger-db(2) notifications(2)
  -  1.0 min  checkout         ALERT           sev 0.50  id:ev/alert-checkout-checkoutlatencyp99-1795386340000
  -  3.0 min  payments-api     ALERT           sev 1.00  id:ev/alert-payments-api-paymentserrorrate-1795386220000
  -  4.0 min  payments-api     DEPLOYMENT      sev 0.25  id:ev/deploy-payments-api-2.3.1-1795386160000
  -  7.0 min  ledger-db        ALERT           sev 1.00  id:ev/alert-ledger-db-ledgerpoolsaturated-1795385980000
  -  8.0 min  ledger-db        CONFIG_CHANGE   sev 0.25  id:ev/config-ledger-db-max_connections-1795385920000
```

- `scope` lists the incident's application (distance 0) and what it depends on, by hops.
- `sev` is a 0 to 1 score from the source's label: SEV1 1.00, SEV2 0.75, SEV3 0.50, SEV4 0.25
  (`why` shows the label). Deployments and config changes are SEV4 by default: their weight
  comes from timing, not severity.
- The earliest strong events on the deepest dependency are usually the root, the later ones on
  callers are symptoms. Changes (DEPLOYMENT, CONFIG_CHANGE) just before a symptom are leads.
- An episode that began before the window and was still under way counts too.
- Nothing found? `--window 30m`, then `--window 2h`. Still nothing: say so.
- `--json` adds `source_record` per event (`alerts.json#3`, `metrics.csv:347`), the raw record
  to open in the source system.

## 2. Which application is the likely cause

```bash
ograph chains INC-5377
```

```
  1. ledger-db              score 0.944
     id:chain/inc-5377-a7370ff7d4358525 0.94: ledger-db:ALERT -> payments-api:LOG_ERROR -> payments-api:ALERT -> checkout:ALERT -> incident
  2. payments-api           score 0.930
     id:chain/inc-5377-f1ece3ced5be33c7 0.93: payments-api:ALERT -> checkout:ALERT -> incident
```

- One line per candidate application, best first, with its best chain: events linked along
  dependency edges, in time order, ending at the incident.
- Scores follow Netflix's published rule: known causal pairs carry 0.9, others
  `time_proximity * 0.6 + severity * 0.4`; chains under 0.6 are dropped.
- `--rank propagation` ranks by how an anomaly spread to callers. When both rankings agree,
  say so; when they differ, report both.
- The chain id is stable across re-harvests. Use it for verdicts.

## 3. Has this happened before

```bash
ograph similar INC-5377
```

```
  0.599  id:inc/inc-4120 on payments-api at 2026-11-01T22:26:40Z  causes: [ledger-db]  mitigations: [raised ledger-db connection pool to 200]
```

- Similarity compares the signal pattern around each incident's onset.
- `causes` appear only for past incidents with an accepted verdict. An empty list means nobody
  confirmed one yet, not that there was none.
- A mitigation that worked on a similar incident is the strongest fix to suggest.

## 4. What does it affect

```bash
ograph blast ledger-db
```

```
ledger-db is depended on by 5 applications
  1  inventory
  1  orders-api
  1  payments-api
  2  checkout
  3  web-frontend
  process card payments: via payments-api, ledger-db
  process online sales: via checkout, web-frontend
  process order fulfilment: via orders-api
  open incidents: id:inc/inc-5377
```

Distance is in dependency hops. Business processes come from the ownership source.

## Vendor news

```bash
ograph news --since 7d
```

Each item names the vendor, the applications that use it and their business processes.
`--since` counts back from now. News is context, not correlation: an item matters to an
incident when its applications are in the incident's `scope` and it came shortly before the
first symptom. Say so as a lead to check, not as a cause.

## What to report

1. The timeline, newest first, minutes before the incident.
2. The top candidates with score and chain id, called hypotheses.
3. The similar past incident, its confirmed cause and the mitigation that worked.
4. The blast radius: applications, business processes, other open incidents.
5. The source records a person should open.
6. What the person must confirm before a verdict.

If the harvest quarantined facts, check them (`references/data-quality.md`) before saying an
expected event is absent: a misspelled service name is quarantined, not lost.
