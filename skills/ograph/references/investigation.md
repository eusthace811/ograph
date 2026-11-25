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
       0.99  ledger-db:ALERT -> payments-api:LOG_ERROR  (time 0.99, severity 1.00, catalog dependency)
       0.93  payments-api:LOG_ERROR -> payments-api:ALERT  (time 0.88, severity 1.00, same application)
       0.96  payments-api:ALERT -> checkout:ALERT  (time 0.93, severity 1.00, catalog dependency)
       0.90  checkout:ALERT -> incident  (known pair, same application)
       weakest link: checkout:ALERT -> incident at 0.90
  2. payments-api           score 0.930
     id:chain/inc-5377-f1ece3ced5be33c7 0.93: payments-api:ALERT -> checkout:ALERT -> incident
       0.96  payments-api:ALERT -> checkout:ALERT  (time 0.93, severity 1.00, catalog dependency)
       0.90  checkout:ALERT -> incident  (known pair, same application)
       weakest link: checkout:ALERT -> incident at 0.90
```

- One line per candidate application, best first, with its best chain: events linked along
  dependency edges, in time order, ending at the incident.
- Scores follow Netflix's published rule: known causal pairs carry 0.9, others
  `time_proximity * 0.6 + severity * 0.4`; chains under 0.6 are dropped.
- Under each chain, one line per link: its score and why. A chain's confidence is the
  geometric mean of its links. Check the weakest link first: a low time part means a long gap,
  and `dependency seen only in traces` or `no direct dependency` means the catalog does not
  state the edge the link crosses.
- `--rank propagation` ranks by how an anomaly spread to callers. When both rankings agree,
  say so; when they differ, report both.
- A line `app:SIGNAL was accepted as the cause of id:inc/... (similarity 0.97)` under an
  application means people accepted that same application and signal as the cause of a
  similar past incident. Report it as support for the candidate, with the incident it names.
- The chain id is stable across re-harvests. Use it for verdicts.

When rank 1 is not what it was earlier, such as at the last update or before a harvest, ask why:

```bash
ograph chains INC-5377 --top 3 --since 2026-11-25T11:53:57.945Z
```

```
since 2026-11-25T11:53:57.945Z: the ranking moved; source files changed: 1
  then  1. ledger-db 0.944  2. payments-api 0.930  3. checkout 0.900
  now   1. payments-api 0.930  2. ledger-db 0.909  3. checkout 0.900
  g:harvest/alerts/alerts.json [alerts]: taking it back alone changes the ranking now, and it alone changes the ranking then
```

- Each named file was tested by ranking again, so report it as the reason: "rank 1 moved from
  ledger-db to payments-api after alerts.json changed". Read what changed in it with
  `ograph changes --since T`; here it lost the LedgerPoolSaturated alert before the incident.
- A file that changed but is not named did not move the ranking on its own. Do not cite it.
- `the changes move it together` means no single file explains the move. Say so; do not pick one.

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
- A line `ruled out: app:SIGNAL (evidence)` under a past incident is a root people rejected
  there. It says what that incident was not. It does not rule the application out here: on
  RCAEval, 9 of the 11 top-ranked roots that matched an earlier rejection were the true cause
  (docs/EVIDENCE.md).

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

## 5. Where in the code

Run this when the workspace has a `code` source (`.ograph/config.toml`), after `chains`.

```bash
ograph code INC-1
```

```
id:inc/inc-1 on checkout at 2026-11-23T13:52:00Z
lines behind the events before it:
  checkout         EXCEPTION       shop/services/checkout/client.py:6  TimeoutError: pool exhausted
  checkout         EXCEPTION       shop/services/payments/pool.py:11  TimeoutError: pool exhausted
  checkout         LOG_ERROR       shop/services/checkout/client.py:8  payment call failed: upstream timeout <*>
  payments         LOG_ERROR       shop/services/payments/pool.py:11  ledger-db connection pool exhausted after <*> <*>
commits before it:
  - 12.0 min  payments         98aecc6b2ee7  payments: shrink ledger pool to 20 to save memory  changed shop/services/payments/pool.py:11
files its description is about (unsure):
    9.08  shop/services/checkout/client.py:8
    8.84  shop/logs/app.log:3
    7.38  shop/docs/postmortems/2026-01-ledger-pool.md:1
    6.88  shop/services/payments/pool.py:11
    5.66  shop/trace.txt:5
past write-ups:
   18.15  shop/docs/postmortems/2026-01-ledger-pool.md  (card, connection, exhausted, failing, ledger-db, payments, pool)
    0.74  shop/docs/postmortems/2026-03-search-disk.md  (out)
```

- The first two sections come from the graph: the statement that writes each log burst, the
  frames of each exception's stack trace, and the commits in the correlation window.
- A commit followed by `changed <path:line>` changed the file behind one of those events. It is
  the strongest lead to report: give its revision, file and line. It is still a hypothesis.
- `first added N days before by <revision>` under a log line names the commit that first put
  that message in the code, however long ago. A message added days before its first burst is a
  lead the commits of the window cannot show. It needs `ogrep index --history` in the code
  repository; without it `degraded` says so.
- `deployments before it` lists a deployment with the files changed since the version the
  service ran before, when both versions are tags or commit ids of the code repository.
  `changed <path:line>` means the same as for a commit.
- The last two sections come from ogrep, which ranks files and past write-ups against the
  incident's title and its top chain. `(unsure)` means the first file does not stand clear of
  the rest. Read a write-up before you suggest its fix.
- `--json` gives `locations` (a log line may carry `introduced`), `commits` and `deployments`
  (with `touches`), `files`, `files_confidence`, `writeups` and `degraded`. A reason under `degraded` means ogrep could not answer for a
  repository: say so, do not report the empty list as "nothing found".
- Commits outside the correlation window are not listed. `suspects --since 7d` reaches further back.
- `ogrep index .` in the code repository makes these lookups fast. ograph does not build that
  index itself.

From a located line, hand over to ogrep in the code repository (paths there are relative to
it, so drop the repository's directory from the `path:line`):

```bash
ogrep --related-tests services/payments/pool.py .   # the tests to run, best first, with why
ogrep --callers -w acquire .                        # the functions that call it; -w keeps longer names out
```

- `--related-tests` ranks tests named after the file, tests that use its definitions and tests
  changed with it. Run the first ones before and after a fix.
- `--callers` needs the function's name, which the line or `ogrep --cards PATH` shows. Callers
  say who else a change to that function reaches, below the level of services that `blast` covers.

## When telemetry says little

```bash
ograph suspects INC-5377                 # the incident's application, commits since the day before
ograph suspects checkout,payments --since 3d
```

```
  1. payments               score 2.73  distance 1  explains checkout
     98aecc6b2ee7  2026-11-23T13:40:00Z  payments: shrink ledger pool to 20 to save memory  (shop/services/payments/pool.py)
  2. checkout               score 1.00  distance 0  explains checkout
```

- Run it when `before` and `chains` find nothing or little. It needs no telemetry: ogrep ranks
  the failing applications and what they depend on by distance and by recent commits.
- Other incidents opened within the correlation window count as failing too, so one upstream
  cause that explains several of them ranks higher.
- With a `code` source, `chains` prints the same list when it ranks no application.
- Say that these suspects come from the dependency graph and recent commits, not from telemetry.

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
5. With a `code` source: the commit, file and line behind the top candidate, if `code` found one.
6. The source records a person should open.
7. What the person must confirm before a verdict.

If the harvest quarantined facts, check them (`references/data-quality.md`) before saying an
expected event is absent: a misspelled service name is quarantined, not lost.
