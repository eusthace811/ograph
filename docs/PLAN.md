# ograph: decision and plan

`ograph` is a working name. It is an ogrep-style Rust CLI that builds the observability knowledge graph described in Netflix's "Ontology-Driven Observability" post (October 2026, saved in this folder as a PDF). Every number below comes from a run in `spikes/` or from a cited file, unless it is marked as an estimate.

## Decision

Import Oxigraph's RDF and SPARQL crates at a pinned commit. Do not import its store or its CLI. Build our own quad store on redb, and build the CLI from ogrep's design.

This copies Netflix's own choice, QuipuDB, an "RDF graph database, a distributed quad store based on redb" (Netflix post, page 5).

| Layer | Source | Why |
|---|---|---|
| RDF terms, parsers, serializers | Oxigraph: `oxrdf`, `oxsdatatypes`, `oxttl`, `oxrdfio` | MIT or Apache-2.0, pure Rust, standalone crates |
| SPARQL parse, optimize, evaluate, results | Oxigraph: `spargebra`, `sparopt`, `spareval`, `sparesults` | `spareval::QueryableDataset` runs SPARQL over any store that implements three methods. Measured in `spikes/redb-quadstore` |
| Storage | Ours, on redb | Pure Rust, Windows in CI. Netflix built on it too |
| SHACL validation | Ours, SHACL Core over a graph trait (`ograph-core/src/shacl.rs`) | Changed after measurement. rudof `shacl` 0.3 pulls in about 238 crates, a C build through `ring`, tokio and reqwest. It cannot read our store without a copy, and it has no focus-node validation. Ours passes 98 of 98 W3C core tests and validates only the nodes a batch touches |
| OWL reasoning | Ours: the subclass closure at ontology load | Changed. The gateway needs only the class closure plus domain and range, which takes about 30 lines. `reasonable` is fast (1 ms) but adds 35 crates, including rio 0.7 |
| CLI, server, agent skill, lab, build scripts | ogrep | Our own MIT code, already proven on Windows and behind Artifactory |
| Optional sink | Apache AGE, as events in a Postgres table plus the graph | For teams that already run Postgres. Measured in `spikes/server-graphs` |

### Why not fork Oxigraph whole, the way ogrep forked tgrep

- **Its storage is closed to extension.** Storage is a private enum with no trait (`lib/oxigraph/src/storage/mod.rs:55-64`). The maintainer closed a storage-trait PR (#1458) and says pluggable backends are not on his roadmap (#199).
- **RocksDB is costly to build.** It needs a C++20 compiler and libclang on every build machine. A clean build takes 479 s with RocksDB and 87 s without (oxigraph#2027).
- **One person maintains it.** He merged all 178 PRs of the last six months, and every crate is 0.x. Pin exact versions, vendor the source, and keep `QueryableDataset` behind one adapter module of ours.

### Why not the other candidates

| Candidate | Verdict | Deciding evidence |
|---|---|---|
| HelixDB | Rejected | v3 is a 723K-line rewrite on a forked SlateDB that compiles C. It has no RDF, SHACL or named graphs. `crates/metrics` still declares AGPL-3.0, and embedded mode sends telemetry by default |
| open-ontologies | Source of ideas | One author, 100K lines, git-patched Oxigraph plus RocksDB. Port its ideas: a three-valued SHACL verdict ("skipped constraint means no verdict"), the closed-vocabulary check for LLM output, ontology `plan` and `diff`, and bitemporal named graphs |
| TrustGraph | Source of ideas | Python platform on Pulsar and Cassandra. Its extraction design is worth copying: show the LLM only the relevant slice of the ontology, let code mint IRIs, reject on domain and range. Its validator drops rejected facts silently, so we record every rejection |
| Apache AGE | Optional sink, not the engine | On the same data the hybrid answers W2 in 0.28 ms warm and 5.3 ms on a new connection, against 0.10 ms in process on redb. Cypher variable-length paths never finished on the hub app, and Cypher writes have open lost-write bugs (#2491, #2494, #2485, #2620) |
| Apache HugeGraph | Rejected | Slower than AGE on every workload. It needs a 2 GB heap and 10 s to start, has had no release since 2025-11, and has an open data-loss bug (#2258) |
| Neo4j | Not prototyped | The pasted review proposed a Neo4j prototype. It is a JVM server with no RDF or SHACL, and AGE already covers the "server graph database" option with a better license fit |

## Measured results

The same seeded dataset was used throughout: 20,000 applications, 100,000 incidents, a dependency DAG and about 1,000,000 events, 3,520,089 quads in all. Every engine returned identical sorted rows for every query. Times are warm medians.

| Query | redb + spareval | redb native path | Oxigraph in memory | AGE hybrid | HugeGraph |
|---|---|---|---|---|---|
| W1: incidents on Bedrock apps in us-east-1 (1,938 rows) | 4.09 ms with decode cache | 2.06 ms | 3.48 ms | 3.6 to 5.1 ms | 6.4 to 12.5 ms |
| W2: events in the 10 minutes before an incident, on the app and its dependencies | 0.229 ms | 0.103 ms | 0.094 ms | 0.28 to 0.33 ms | 0.80 to 1.47 ms |
| W3: blast radius of the hub app (19,999 rows) | 14.89 ms | 12.81 ms | 135 ms | 28 ms (SQL CTE) | 61 to 64 ms |
| Open or start | 3.3 ms | | reload every run | 120 to 361 ms | 9.75 s |

The spike also showed three things the real store must change:

1. **Inline small values in term ids.** Store dateTime values, numbers and short strings inside the id, as Oxigraph does. 49% of W2's SPARQL time went to decoding dateTime values for the FILTER.
2. **Add native time-window and traversal paths.** The trait only exposes quad patterns, so SPARQL cannot reach a time index. The SPARQL W2 path reads every event an app ever had, and the gap grows with how much history you keep. Correlation and blast radius run natively in Rust, and SPARQL stays for ad hoc questions.
3. **Shrink the dictionary.** It took 262 of the 544 MiB because every term is stored twice.

The Umesh Malik post reported 42 s for W1 on 360,000 triples in rdflib. The same query takes 4 ms here at ten times the size. RDF is not slow, as the pasted review argued.

## What goes in the graph

Netflix keeps raw metrics, logs and traces in their own systems. AutoRCA "queried live" those systems and queries the graph "with SPARQL over the shared ontology" (post, page 12). The graph holds harvested facts only. That bounds its size.

| In the graph | Stays outside, referenced by id |
|---|---|
| Applications, teams, owners, regions (catalog-gated) | Raw log lines |
| Dependencies, with declared or inferred provenance | Metric time series |
| Deployments, config changes, commits, experiments | Individual spans |
| Alert firings, incidents, their lifecycle and roles | Chat messages beyond the thread summary |
| Error patterns (log templates), metric anomalies, slow and failed traces | |
| Discussion threads with closed-vocabulary classification | |
| Correlation chains and hypotheses, with evidence and verdicts | |
| Harvest records: source, time, extractor version | |

The ontology starts from Netflix's twelve namespaces: SLACK, OPS, ALERT, METRIC, LOG, TIMELINE, INCIDENT, RCA, EXPERIMENT, E2E, CORRELATION and HARVEST. The first version models only what the four benchmark questions need.

## Core data shapes

```rust
// The only way anything enters the graph.
struct ProposedFact {
	subject: EntityRef,
	predicate: Predicate,          // a closed enum generated from the ontology
	object: Object,
	evidence: Vec<EvidenceRef>,    // a source record id, a file:line, or a query
	method: Extraction,
}

enum Extraction {
	Rule { extractor: ExtractorId },
	Catalog { catalog: CatalogId },
	Llm { model: String, prompt_hash: Hash, confidence: Confidence },
	Human { who: String },
}

enum EntityRef {
	Known(Iri),                    // resolved against an authoritative catalog
	Provisional { kind: Class, name: String },  // external orgs and news subjects
}

enum Verdict { Proposed, Accepted, Rejected, Superseded { by: Iri } }
```

- **Every LLM fact carries a confidence.** `Llm` cannot exist without one, so the type rules out an unscored LLM fact.
- **Applications need a catalog match.** A predicate whose range is `ops:Application` accepts only `EntityRef::Known`.
- **Each harvest batch writes one named graph.** That graph records the batch's source, time and extractor version. Re-running a harvest replaces its graph, so harvests are idempotent.
- **Chains and hypotheses are nodes, as in Netflix's example.** For example `:chain-1 corr:rootCause :deploy-1.4.2 ; corr:leadsTo :INC-5377 ; corr:confidence 0.92`. The store indexes named graphs and nodes well. It indexes RDF 1.2 reifiers only when the triple is fully bound (`spareval/src/eval.rs:2812`).

## The validation gateway

Enrichers never write to the store. They emit `ProposedFact` batches, and the gateway checks each one before commit:

1. Every entity has a type, and every IRI follows one naming scheme.
2. Applications and teams resolve against their catalog. Fuzzy matches below a threshold go to quarantine. They are not created.
3. The predicate's domain and range match, using the `reasonable` closure of the ontology.
4. Closed vocabularies (`sh:in`) hold, such as severity, thread category and sentiment.
5. Evidence references point at records that exist.
6. Incremental SHACL runs on the focus nodes the batch touches. Neither rudof nor shifty does this, so we build the focus-node selection.

A rejected fact goes to a `quarantine` named graph with its reason, never into a log line. `ograph validate` runs full SHACL over the whole graph as an audit.

## Enrichers

Enrichers run in a fixed order, and each one reads what earlier ones wrote. That is Netflix's "load, add, save" sequence. External calls are best effort. A failed source marks its harvest as degraded and does not block the others. Netflix's five layers set the order:

1. **Deterministic extraction.** Regexes find trace ids, incident references and user mentions.
2. **Catalog validation.** Names are fuzzy-matched and then checked against the catalog.
3. **API enrichment.** User ids resolve to identity, team and on-call rotation.
4. **LLM classification.** Through pi, the model returns JSON drawn from closed vocabularies (see "LLM access through pi"). Outputs are cached by content hash (ogrep's `FactStore` design), so a re-harvest gives the same triples without calling the model again.
5. **Probabilistic correlation.**

The first sources read files and exports, which keeps runs offline and repeatable:

| Source | Input | Reuse |
|---|---|---|
| Catalog and dependencies | Backstage `catalog-info.yaml`, Compose, Kubernetes, Prometheus rules | `ogrep --entities --json` already extracts these with declared or inferred provenance |
| Logs | Log files | Drain templates, `--new-since` and `--template-to-code` from ogrep, plus the `drain3` crate |
| Traces, metrics, logs | OTLP JSON | `opentelemetry-proto` with `with-serde` |
| Incidents and alerts | PagerDuty, incident.io or ServiceNow exports, Alertmanager | New |
| Deployments and commits | Git tags, CI or CD exports, `ogrep --timeline` | Partly in ogrep |
| Discussions | Slack or Teams JSON exports | New, with the LLM step |
| Past verdicts | ogrep's decision journal (`ogrep-journal.jsonl`) | Format already defined in ogrep's `docs/REFERENCE.md` |

ogrep stays the code specialist, the "Code" box in Netflix's AutoRCA diagram. ograph calls it through `--json` and keeps no copy of its search.

## Correlation

Start with Netflix's published settings as defaults (post, page 9 figure):

- 13 signal types.
- Known causal pairs with a 0.9 prior: DEPLOYMENT to LOG_ERROR, CONFIG_CHANGE to METRIC_ANOMALY, ALERT to INCIDENT, TRACE_FAILED to INCIDENT.
- A score of `time_proximity × 0.6 + severity × 0.4`.
- A 10-minute window and a minimum confidence of 0.6.

All of these are configuration, not code, because a settlement failure can surface hours after its cause. The engine walks candidate chains depth-first over the native time index. It stores each chain with the records it used and writes nothing as a conclusion. The RCAEval gate below tunes these values.

## LLM access through pi

ograph holds no LLM credentials and contains no provider code. Every model call goes through `pi`, which already owns authentication. At home that is a Codex login. On a corporate network it is a pi provider extension that talks to the corporate LLM gateway. ograph itself does not change between the two.

The model has two jobs, and each uses pi differently.

**Extraction runs during harvest.** ograph runs a configured command with the prompt on standard input and reads JSON from standard output:

```toml
[llm.extract]
command = ["pi", "-p", "--no-tools", "--no-session", "--no-extensions",
           "--no-skills", "--no-context-files", "--offline", "--thinking", "off"]
parallel = 4
```

- **Prompts come from the ontology.** The closed vocabularies in the prompt are generated from the ontology's `sh:in` lists, so the prompt and the gate cannot disagree.
- **Every output passes the validation gate.** Output that is not JSON, or has a value outside a list, goes to quarantine.
- **Outputs are cached.** The cache key is the input's content hash, the model and the prompt hash. A re-harvest makes no new calls.
- **A failed call degrades the harvest without blocking it.**
- **Little text reaches the model.** Only discussion threads and incident write-ups go to it, not logs or metrics.

Measured on 2026-11-22 on the development machine, this command with pi's default model returned valid closed-vocabulary JSON in 3.0 s. In the same test the model labeled a dependency failure as DEPLOYMENT. That is why outputs carry a confidence and humans record verdicts.

**Investigation runs in a pi session.** ograph ships a pi extension, built like ogrep's `scripts/agent/pi-extension.ts`, plus a skill. The extension exposes `before`, `blast`, `chains`, `similar`, `why`, `sparql` and `verdict` as tools backed by `ograph serve`. The same MCP server serves any other MCP client. The agent reasons on whatever model pi is logged into, and ograph supplies the evidence.

We rejected two alternatives:

- **ograph calling provider APIs directly.** That means credentials and per-provider code in the CLI.
- **ograph reading pi's tokens.** That means reading `~/.pi/agent/auth.json` or calling `pi auth print-bearer-token`, which couples ograph to pi's storage. The Codex login also rejects some models, such as `gpt-5.4-mini` (measured).

## The CLI

The surface follows ogrep: subcommands, flags, `--json`, exit codes 0, 1 and 2, and a server found through a file in the index directory.

```sh
ograph init                         # ontology, shapes and config into .ograph/
ograph harvest                      # run the enrichers in order, one named graph per batch
ograph serve                        # keep the store open, re-harvest on change, JSON-RPC over TCP
ograph before INC-5377 --window 30m # events before an incident on its app and dependencies
ograph blast api-gateway            # dependents and affected business processes, by distance
ograph chains INC-5377              # correlation chains with their evidence
ograph similar INC-5377             # past incidents, their accepted causes and mitigations
ograph why <fact>                   # provenance: harvest, source record, extractor, verdicts
ograph verdict <hypothesis> accept --evidence <ref>
ograph sparql 'SELECT ...'          # ad hoc queries for agents and people
ograph validate                     # full SHACL audit
ograph export --turtle              # the per-harvest archive Netflix found useful for debugging
ograph ontology check | diff        # ontology as code
ograph-lab                          # local web page over the graph, like ogrep-lab
```

It also ships a pi extension, an agent skill and an MCP adapter, packaged the way ogrep's are.

## Evidence gates

Each phase ends with a check that a reviewer can rerun, the same way ogrep's `scripts/eval` does.

| Gate | Data | Pass condition |
|---|---|---|
| Correctness | The spike generator | Identical rows to Oxigraph in memory on every query |
| Speed | The spike generator, scaled to 10x | W1 to W3 within the redb native numbers above, plus the four questions from the pasted review |
| SHACL conformance | W3C data-shapes suite | A score that may never fall, compared with pyshacl |
| Gateway | Generated bad writes plus LLM outputs replayed from cache | Zero invalid facts committed, and every rejection quarantined with a reason |
| RCA accuracy | RCAEval (MIT, 735 labeled failures with metrics, logs and traces) plus OpenTelemetry demo injections from ogrep's `suspects_eval.py` | Top-1 and top-3 root-cause accuracy reported against RCAEval's own baselines. This is the number Netflix's post never published |
| Build | Windows MSVC, macOS and Linux, through `./x doctor` and `./x vendor` | No `cc`, `cmake`, `bindgen` or `-sys` crate in `cargo tree -e normal,build --target all` |

## Phases

1. **Scaffold.** Copy ogrep's workspace layout (`ograph-core`, `ograph-cli`, `ograph-lab`), `Makefile`, `x`, CI and the vendoring support. Vendor the Oxigraph crates at the pinned versions the spike used. Turn the spike benchmark into `scripts/eval/store_bench`.
2. **Store.** Build the redb quad store with inlined ids, the six graph-qualified orderings plus default-graph orderings, the event time index and the reverse dependency index. Exit on the correctness and speed gates.
3. **Ontology and gateway.** Write the first ontology and SHACL shapes. Add the `reasonable` closure, the catalog, the `ProposedFact` path and quarantine. Exit on the gateway and SHACL gates.
4. **Deterministic enrichers.** Catalog, dependencies, deployments, incidents, alerts and log templates, mostly from files ogrep already parses. Exit when the four review questions answer on the OpenTelemetry demo.
5. **Correlation and investigation memory.** Add chains, `verdict` and `similar`. Exit on the RCAEval gate.
6. **LLM enrichers.** Discussion classification with a cached, closed-vocabulary output. Exit on the gateway gate with replayed outputs.
7. **Serve, agents and lab.** Add the server, the agent skill, the MCP adapter and the lab page.
8. **Optional AGE sink.** Ship it only if a team asks for it.

Graph algorithms (HelixDB's Apache-licensed `graph-algorithms` crate is a candidate), multi-agent orchestration and automated remediation wait until phase 5's gate passes.

## Status (2026-11-23)

Phases 1 to 7 and Phase 9 (ideas from NVIDIA auto-ontology) are built. Each one passed its gate, and the gates are in `docs/EVIDENCE.md`. Phase 8, the AGE sink, waits until a team asks for it.

Decisions the evidence changed:

- **The correlation window is 30 minutes, not 10.**
  - With Netflix's rule, chain mode keeps 0.79 top-1 when the incident opens 10 minutes after the fault.
  - With a 10-minute window it drops to 0.11.
  - Propagation ranking is kept as an option for teams that need a short window.
- **The known-pair prior replaces the formula; it does not multiply it.**
  - That is what the post says, and it scored best.
  - My first reading, a penalty multiplier, threw away most chains.
- **The default extraction model is `openai/gpt-6-luna` through pi.**
  - It is the cheapest, and it matched larger models on the 8-thread test.
  - On the demo's 16 labeled threads it classified 13 correctly. After the Phase 9 prompt it classifies 15 or 16.
- **Phase 9 ported five ideas from NVIDIA auto-ontology, and the evidence cut two of the planned parts.**
  - Co-occurring events do not confirm a model-proposed dependency. They scored 0.18 precision on RCAEval against a 0.14 base rate. Only catalog and trace edges count.
  - No model reads runbooks for aliases. With a quote check it still added a false alias and found no true one the phrases missed.

## Open decisions

- **The name.** `ograph` is a placeholder.
- **Default time windows per domain.** Settlement and news correlations need longer windows, set from historical incidents.
- **Distribution.** QuipuDB is distributed. ograph starts as one store per team, with `serve` for sharing. Distribution waits until a measured need appears.

## Sources

- Netflix TechBlog, "Ontology-Driven Observability", 2026-10-16 (PDF in this folder).
- Umesh Malik, "Build an Observability Knowledge Graph", 2026-10-19.
- `spikes/redb-quadstore/README.md` and `spikes/server-graphs/README.md`: the measurements above.
- NVIDIA/auto-ontology, default branch, read 2026-11-23.
- Oxigraph at `9da8d30`; HelixDB at `3bf4e545`; open-ontologies at `c2b64e1`; TrustGraph at `7774be0`; apache/age and apache/hugegraph default branches, read 2026-11-22.
