# ograph

ograph builds an ontology-driven observability knowledge graph and queries it from the command line, in the way Netflix's "Ontology-Driven Observability" describes. It works the same way on a laptop and on a corporate network.

- **Ingest.** It reads your catalog, deployments, config changes, alerts, incidents, logs, metrics, traces, chat threads and news.
- **The gateway.** Every proposed fact passes a validation gateway against the ontology before it is stored.
- **The store.** Facts go into a pure-Rust quad store on redb.
- **Questions.** It answers four investigation questions on native indexes.
- **SPARQL and agents.** Everything else is SPARQL, available to people and to agents through pi or MCP.

```sh
ograph init
ograph harvest                     # one named graph per source file, then correlation
ograph before INC-5377             # 1. which recent events could explain this incident
ograph chains INC-5377             #    ranked root-cause candidates with their evidence chains
ograph blast ledger-db             # 2. which services and business processes this affects
ograph similar INC-5377            # 3. past incidents like this one, their causes and mitigations
ograph news                        # 4. news touching the vendors our applications use
ograph why id:inc/inc-4120         # provenance of every fact about a node
ograph verdict id:chain/inc-4120-2 accept --evidence PM-4120 --by oncall
ograph sparql 'SELECT ?app WHERE { ?app ops:dependsOn+ id:app\/ledger-db }'
ograph lab                         # one HTML page over the whole graph
```

## How it works

- **Raw telemetry stays in its own systems.** ograph keeps only episodes, such as "a burst of this log template" or "this metric left its band", with a pointer back to the first raw record. It does not keep every line, point or span. On one RCAEval case that is 463 quads instead of 3.9 million, and it ranks root causes better (0.87 against 0.63 top-1 for what SPARQL alone can express; see `docs/EVIDENCE.md`).
- **The gateway owns every write.** Enrichers propose facts. The gateway checks:
  - IRIs, and catalog membership with fuzzy matching;
  - closed vocabularies, model confidence and evidence;
  - incremental SHACL on the nodes the batch touches.

  Rejected facts are kept in a quarantine graph with their reason.
- **Each source file is one named graph.** Re-harvesting writes only the difference.
- **The questions run on native indexes.** A per-application time index answers "what happened before", and the dependency index answers "what does this affect". SPARQL, through Oxigraph's `spareval`, covers everything else.
- **Correlation follows Netflix's published rule.** Known causal pairs carry a 0.9 prior. Other pairs score `time_proximity * 0.6 + severity * 0.4`, and chains under 0.6 are discarded. The window is 30 minutes. A chain is a hypothesis until someone records a verdict.
- **A model proposes, and data confirms.** A model-proposed dependency needs a catalog or trace edge behind it. A thread label needs a quote from the thread, or it becomes UNKNOWN. Aliases come only from phrases that state them outright.
- **Models run through `pi`.** ograph holds no credentials. Answers are cached by content hash, so a re-harvest makes no model calls.

## Results

| Check | Result |
|---|---|
| Root cause on RCAEval RE2-OB (90 labeled failures), held-out reps | AC@1 0.87, AC@3 0.95, Avg@5 0.94. The published numbers are TORAI 0.83/0.95/0.93, CIRCA 0.67/0.89/0.85 and BARO 0.67/0.82/0.80 |
| Similar past incident shares the root cause | 0.92 top-1 (59 of 60 held out), 0.94 top-3, against about 0.20 by chance |
| SPARQL correctness against Oxigraph in memory | 0 mismatches over 41 queries on 3.5 M quads |
| W2, the 10 minutes before an incident | 0.003 ms native, 0.17 ms SPARQL, 0.10 ms Oxigraph |
| W3, blast radius of the hub application | 15 ms native, 18 ms SPARQL, 137 ms Oxigraph |
| SHACL Core, W3C test suite | 98 of 98 |
| Gateway, 16 kinds of bad write plus a broken model | 0 invalid facts committed, every rejection quarantined with a reason |
| Model-proposed dependencies | Committed only when a catalog or trace edge backs them. Co-occurring events were tested as support and rejected: 0.22 precision against a 0.14 base rate |
| Thread classifier on 16 labeled threads | 15, 16 and 15 correct in three runs, up from 13. It quotes the cause or answers UNKNOWN |
| Aliases from runbooks | 3 of 3 stated aliases, 0 of 6 decoys |
| Misspelled names in SPARQL | Rejected with the nearest declared name; 0 false rejections over every stored predicate and class |
| Build | Pure Rust: no `cc`, `cmake` or `bindgen`, and a clean CLI build takes 8 s |

Every number comes from a script in `scripts/eval/` or a binary in `ograph-bench/`, with its output in `docs/evidence/`. `docs/EVIDENCE.md` explains each one, and `docs/PLAN.md` records the decisions.

## Layout

| Path | What it holds |
|---|---|
| `ograph-core/` | The store, SPARQL adapter, ontology, SHACL, gateway, enrichers, correlation, queries, serve and lab |
| `ograph-cli/` | The `ograph` binary |
| `ograph-bench/` | `store_bench`, `gateway_eval`, `confirm_eval`, `shacl_case`, `purekg_bench` and `profile_query` |
| `ontology/` | `ograph.ttl` (classes, properties, closed vocabularies with definitions) and `shapes.ttl` |
| `scripts/eval/` | The RCAEval export and scoring, the similar-incident eval, the model, alias and coverage gates, and the SHACL suite runner |
| `scripts/agent/` | The pi extension, the agent skill, the installer and the contract test |
| `examples/demo/` | A deterministic demo company for the four questions |
