---
name: ograph
description: Investigate production incidents with ograph, an ontology-driven observability knowledge graph. Use when asked why an incident happened, what changed before it, what a failing service affects, whether it happened before and how it was fixed, which vendor news touches our services, or for any question about services, dependencies, deployments, alerts, incidents and their evidence, in a directory that has (or should have) an .ograph/ workspace.
allowed-tools: Bash(ograph:*)
---

# Incident investigation with ograph

ograph keeps a knowledge graph of the service catalog, dependencies, business processes,
vendors, deployments, config changes, commits, alerts, incidents, log and metric anomalies, slow
or failed traces, exceptions, chat threads and news. Raw logs, metrics and traces stay in their own systems;
every event points back to its first raw record (`source_record`).

## Quick start

ograph needs `ogrep` on `PATH` (`ogrep --version`) to read the service catalog. Without it the
catalog harvest line says `DEGRADED: ogrep not runnable` and every fact about a service is
quarantined, so every answer comes back empty. Fix that first.

```bash
ograph harvest    # commands search upward for .ograph/; the last line counts quarantined facts
```

## Commands

### Investigate

```bash
ograph before INC-5377                 # default window 10m, 4 dependency hops
ograph before INC-5377 --window 45m    # widen when nothing shows up; --depth 6 for more hops
ograph chains INC-5377                 # top 5 applications, each with its best chain
ograph chains INC-5377 --top 10
ograph chains INC-5377 --rank propagation   # second opinion: anomaly spread, not chain score
ograph chains INC-5377 --window 60m --set depth=6   # what a wider correlation would rank (stores nothing)
ograph blast ledger-db                 # name, alias or IRI of an application; --depth limits the hops
ograph similar INC-5377 --top 3
ograph code INC-5377 --top 3           # lines, commits, files and write-ups behind it; nothing without a code source (exit 1)
ograph suspects INC-5377               # likely causes from dependencies and recent commits, no telemetry needed
ograph suspects checkout,payments --since 3d   # the same for failing applications
ograph news                            # all news; --since 24h, 7d counts back from now
```

### Explain

```bash
ograph why INC-5377                    # every fact: graph, source, extractor, time, model provenance
ograph why id:app/ledger-db            # also lists quarantined proposals about the node
ograph why id:ev/alert-ledger-db-ledgerpoolsaturated-1795385980000
ograph changes --since 2d              # facts the sources added, removed or changed since then
ograph chains INC-5377 --known-at 2026-11-20T09:00:00Z   # the answer the store gave then; also on before, blast, sparql
```

`--known-at` and `--since` take a dateTime with a zone or how long ago (`90m`, `2d`). The time is
when ograph recorded a fact, not when it became true. A view holds only what a harvest recorded
and the history still keeps; standard error says how far back that goes.

### Check names and answers

```bash
ograph resolve chekout ledger-service    # names to catalog applications, at most 5, by the harvest's rules
ograph cite-check answer.md              # every chain, incident, application and IRI an answer cites
ograph status                            # a line when the graph is stale or chains wait for a verdict
```

`./x install-hook` in the ograph repository runs `ograph status` at the start of each Claude Code
session (references/data-quality.md).

### Record (only after a person confirms)

```bash
# accept or reject a chain; use the chain id that `chains` printed
ograph verdict id:chain/inc-5377-a7370ff7d4358525 accept --evidence PM-5377 --by oncall
ograph verdict id:chain/inc-5377-f1ece3ced5be33c7 reject --evidence "deploy was a no-op" --by oncall
ograph replay --set window_s=600       # before a settings change: which past verdicts it would flip; exits 1 if any
```

### Query anything else

```bash
# the ontology's prefixes, id: and g: are predeclared (references/sparql.md lists them)
ograph sparql 'SELECT ?app WHERE { ?a ops:dependsOn+ id:app\/ledger-db ; ops:name ?app }'
ograph sparql 'SELECT ?id ?status WHERE { ?i a inc:Incident ; inc:id ?id ; inc:status ?status }' --limit 20
```

### Maintain

```bash
ograph init                 # create .ograph/ (config.toml, ontology.ttl, shapes.ttl)
ograph harvest              # all sources, then correlation; unchanged files write nothing
ograph harvest --only alerts   # one kind, then re-correlate
ograph validate             # full SHACL audit, expect "0 violations"
ograph stats                # store size, graphs, triples
ograph lab --out lab.html   # one HTML page for a person: causes, timeline, neighbourhood, evidence
ograph export --out all.trig
ograph compact              # drop history older than history_days, shrink the file
```

## Output

Every command takes `--json` (put it before or after the subcommand) for machine-readable output.
Prefer it when you need fields such as `source_record`, chain ids or scores.

```bash
ograph --json before INC-5377
ograph --json chains INC-5377
ograph --json sparql 'SELECT ?id WHERE { ?i a inc:Incident ; inc:id ?id }'
```

Exit codes: `0` found something, `1` ran fine but found nothing (an empty answer, not an error),
`2` error (the message on stderr says what to fix, such as an unknown incident or a misspelled
ontology name with the nearest declared one).

## Naming things

Any command that takes a node accepts:

- an incident id: `INC-5377`
- an application name or alias: `ledger-db`, `pay-gw`
- a prefixed name: `id:inc/inc-5377`, `id:app/ledger-db`, `id:chain/inc-5377-a7370ff7d4358525`
- a full IRI: `https://ograph.dev/id/app/ledger-db`

Inside SPARQL, escape a `/` in a prefixed name, and the dots of a version (`id:app\/ledger-db`,
`id:ev\/deploy-payments-api-2\.3\.1-1795386160000`), or use the full IRI
(`<https://ograph.dev/id/app/ledger-db>`). A parse error names the escaped form.

## Rules for investigating

1. Start with `before`, then `chains`. Read the signal types (DEPLOYMENT, CONFIG_CHANGE, ALERT,
   LOG_ERROR, LOG_NEW, METRIC_ANOMALY, TRACE_FAILED, TRACE_SLOW, COMMIT, EXCEPTION) and the
   order they happened in. LOG_NEW is the first line or two of an error message the service
   had not written before: weak alone, a strong lead right after a change. OPERATOR_ACTION is
   something a person said in chat they did (a rollback, a restart); chains never start from
   one, and its time is when the message was posted. An event marked `chronic` belongs to a
   series that kept firing before the incident: background, not the start of the fault. An
   application that `explains` others in `chains` (when `own_degradation` is on) ranks above
   them: their anomalies followed its own.
2. A chain is a hypothesis. Say "the strongest candidate is" with its score and evidence, never
   "the cause is", unless a verdict accepted it.
3. Check `similar` before proposing a fix: a mitigation that worked before is the best lead.
4. Run `blast` on the suspected cause to say who else is affected.
5. Run `why` on any fact you cite, and name its source record so a person can open it.
6. Record a verdict only when a person confirmed the cause. Ask once for their name and a ticket
   or postmortem id if they gave none; if they have none, quote their confirmation as the
   evidence and use their role (`--by oncall`).
7. An empty answer (exit 1) is information: say what you looked for and widen the window or the
   question; do not invent events.
8. When the workspace has a `code` source, run `code` after `chains`: it names the lines behind
   the events and the commits that changed them. Report a commit under `touches` as a lead with
   its revision, file and line. It is still a hypothesis. From a located line, ogrep gives the
   next step in the code repository: `ogrep --related-tests PATH .` for the tests to run and
   `ogrep --callers -w NAME .` for the functions that call it (references/investigation.md).
9. When `before` and `chains` find little, run `suspects`. Say that its list comes from the
   dependency graph and recent commits, not from telemetry.
10. Before you name an application no answer showed you, `resolve` it. `ambiguous` (a sibling
   such as `-canary`, or a tie) and `unresolved` are not applications: say so.
11. Run `cite-check` on your answer before you give it. Fix or drop every `stale` or `unknown`
   citation; a stale chain means the graph changed since you read it.
12. The graph keeps episodes and pointers, not raw detail. For a config change's old and new
   value, a log line or a metric curve, open the event's `source_record` (`--json`) in its file.
13. When rank 1 differs from an earlier answer, run `ograph chains INC --since T`. Report only the
   files it names.
14. When a source turns out wrong, `ograph uses FILE[#n]` lists the facts, chains and verdicts
   that rest on it; check those verdicts again.
15. `chains` and `before` skip dependencies the catalog dates after the incident opened and list
   them as `withheld`; use `blast APP --at INC` for the blast radius as it was then.
16. If chat names apps by short names and `disc:about` misses them, see `known_apps` in
   references/sources-and-harvest.md.

Report what [references/investigation.md](references/investigation.md) lists under "What to report".

## Specific tasks

* **Investigating an incident end to end** [references/investigation.md](references/investigation.md)
* **Recording and reading verdicts** [references/verdicts.md](references/verdicts.md)
* **SPARQL: classes, properties, recipes, pitfalls** [references/sparql.md](references/sparql.md)
* **Setting up sources and harvesting** [references/sources-and-harvest.md](references/sources-and-harvest.md)
* **Quarantine, degraded harvests and data quality** [references/data-quality.md](references/data-quality.md)
* **Serving the graph to agents (JSON-RPC, MCP, pi)** [references/serving.md](references/serving.md)
* **Changing the ontology safely** [references/ontology.md](references/ontology.md)
