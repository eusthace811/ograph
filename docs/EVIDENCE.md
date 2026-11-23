# ograph: evidence per phase

Every number here comes from a script or binary in this repository, and its raw output is in `docs/evidence/`. `scripts/eval/run_all.sh` reruns every gate.

Machine: Apple silicon with 12 cores and 32 GiB of RAM. The store is on an external disk. Measured on 2026-11-22 and 23.

## Phase 1: scaffold and build

| Check | Result |
|---|---|
| C toolchain in `cargo tree -e normal,build --target all` | No `cc`, `cmake` or `bindgen`. The only `-sys` crate is `windows-sys` (pure Rust declarations, no C build) |
| Clean release build of the CLI | 8 s wall. The binary is 10 MB |
| Oxigraph crates | `oxrdf` 0.3.5, `oxttl` 0.2.5, `spargebra` 0.4.8, `spareval` 0.2.9 and `sparesults` 0.3.5, pinned exactly. `spareval` is touched only in `ograph-core/src/sparql.rs` |

A Windows MSVC build was not run, because this machine has no Windows toolchain and I did not change the installed Rust toolchains.

## Phase 2: store (`ograph-bench store_bench`, `store-x1.txt`, `store-x10.txt`)

The store changed in these ways from the spike:

- **Inline ids.** Canonical dateTime, integer and boolean values live inside the id.
- **Hashed dictionary.** A sip128-keyed dictionary stores each term once.
- **Union default graph.** The default graph is a reference-counted union of the named graphs, so SPARQL never sees a triple twice.
- **Fewer orderings.** Two graph-qualified orderings replace six.
- **Time indexes.** It adds a time index and a per-application time index.
- **Diff replace.** Graph replace writes only the difference.

| | 1x (3.52 M quads) | 10x (35.2 M quads) |
|---|---|---|
| Correctness | 0 mismatches over 41 queries against Oxigraph in memory and the native paths | 0 mismatches, SPARQL against native |
| Load | 14.4 s, 244 K quads/s (4 M-quad transactions) | 401 s, 88 K quads/s |
| On disk after compaction | 658 MiB, 196 bytes/quad | 8.6 GiB, 256 bytes/quad |
| Open | 3 ms | 7 ms |
| W1 (incidents on one team's apps in one region), SPARQL / native / Oxigraph | 10.4 / 7.6 / 6.4 ms | 185 / 159 ms for 10.7x the rows |
| W2 (events in the 10 minutes before an incident), SPARQL / native / Oxigraph | 0.16 / 0.003 / 0.10 ms | 0.21 / 0.003 ms |
| W3 (blast radius of the hub), SPARQL / native / Oxigraph | 17.7 / 15.3 / 136.5 ms | 213 / 189 ms for 10x the rows |
| Re-harvest of one day's graph, unchanged | 60 ms, 0 writes | 9.6 s, 0 writes |

**W2 does not grow with history.** At 10x, W2 runs in the same time as at 1x, because the per-application time index answers it. W1 and W3 grow with the number of rows they return.

**Optimizations, each measured before and after:**

| Change | Before | After |
|---|---|---|
| Point lookup for fully bound patterns (W1 SPARQL) | 12.4 ms | 9.3 ms |
| Two graph orderings instead of six (disk, 1x) | 2,176 MiB | 869 MiB |
| Union keys sorted per ordering, then larger transactions (1x load) | 18.7 s | 14.4 s; compacted size 857 to 658 MiB |
| Same changes, 10x load | Not finished after 43 min, with 98% of the time in `pwrite` | 401 s |
| Graph replace as a diff, unchanged day (1x) | 4.6 s | 60 ms |

Non-durable commits were tried and rejected. They did not speed up the 1x load (20.3 s), and they doubled the loose file, because redb cannot reuse pages freed by non-durable commits until a durable one.

**Harvest speed.** Three changes in `enrich/` made harvest about 25% faster with identical sorted quads on every RCAEval case and the demo:
- Drain compares word ids instead of strings, and indexes templates by length.
- Log and trace lines are read into a fixed list of fields through a serde visitor, with the full parse as a fallback.
- Baseline medians use selection instead of a full sort.

Interleaved, sequential runs on 12 cases (3 each) give a median of 0.228 s before and 0.180 s after; the merged 90-case harvest goes from 14.8 s to 10.1 s (6 runs each). Root-cause and similar-incident scores are unchanged. The rest of single-case time is trace parsing and 7 to 8 durable redb commits. Product ids such as `L9ECAV7KIM` escaped the masking rule, and so did ids after a `key=`. Masking upper-case ids with a digit, and judging the value after `=`, takes recommendationservice in one case from 9,113 distinct masked lines to 6, and cartservice from 57 to 4. Stored quads on the checked cases, RCAEval scores and similar-incident scores are unchanged; timing differences were inside the noise of a loaded machine.

## Phase 3: ontology, SHACL and gateway

| Check | Result |
|---|---|
| SHACL Core, W3C suite (`shacl-core.txt`) | 98 of 98 on `sh:conforms` and result count. rudof's measured 92 came from a harness that ignored `sht:dataGraph`; with a fixed harness it would likely match |
| `ograph ontology check` | 0 problems. Every IRI the code uses is declared |
| Gateway gate, 17 kinds of bad write plus 43 good events (`gateway.txt`) | 0 bad facts committed, every kind quarantined with its reason, 43 of 43 good events committed (3 of them misspelled names that must still resolve), full audit clean, and a re-submit writes 1 quad (the harvest time) |

The rudof `shacl` crate was measured and rejected:

- **Dependencies.** About 238 crates, including a C build through `ring`, plus tokio and reqwest.
- **Store access.** No way to read our store without a copy.
- **Focus nodes.** No focus-node validation.

The `reasonable` crate was measured: 35 crates, and 1 ms to close a 220-triple ontology. The gateway needs only the subclass closure, so we compute that ourselves.

The gate found two real gaps, and both are fixed:

- **Partial nodes.** When one fact of a node was rejected, the rest of the node still committed. A violation that no single fact caused now rejects the node.
- **Wrong domain in the ontology.** The audit found an ontology bug: `harv:reason` had too narrow a domain.

## Phase 4: deterministic enrichers and the four questions (`examples/demo`)

| Question | Command | Result on the demo |
|---|---|---|
| 1. Which recent events could explain this incident? | `ograph before INC-5377`, `ograph chains INC-5377` | 9 events across the scope. Top cause ledger-db, with the chain `ledger-db:ALERT -> payments-api:LOG_ERROR -> payments-api:ALERT -> incident` (the designed cause) |
| 2. Which services and business processes are affected? | `ograph blast ledger-db` | 5 dependents by distance. Processes affected: card payments, online sales and order fulfilment |
| 3. Which past incidents resemble this one, and which mitigations worked? | `ograph similar INC-5377` | INC-4120: cause ledger-db, mitigation "raised ledger-db connection pool to 200" |
| 4. Which news relates to our vendors? | `ograph news` | CardRail latency, reaching payments-api and card payments. MailRelay deprecation, reaching notifications. An unrelated item was dropped |

## Phase 5: correlation and investigation memory on RCAEval RE2-OB

RCAEval RE2-OB (MIT) has 90 labeled failures: 5 services × 6 fault types × 3 repetitions, with metrics, logs and traces for 30 of them.

- **The export hides the label.** `scripts/eval/rcaeval_export.py` writes every case under an opaque id, because the case's folder name holds the answer.
- **The incident.** Each incident opens on `frontend` 300 s after the injection, the way an alert-driven incident would.
- **Detection.** ograph is never told the injection time. Its detectors use trailing baselines only.
- **Splits.** Rep 1 tunes; reps 2 and 3 are held out.

| Method (`rcaeval-final.txt`) | All 90: AC@1 / AC@3 / Avg@5 | Held-out 60 |
|---|---|---|
| **ograph, default (chain ranking, 30-minute window)** | **0.93 / 0.99 / 0.98** | **0.93 / 0.98 / 0.98** |
| ograph, propagation ranking | 0.83 / 0.94 / 0.94 | 0.87 / 0.97 / 0.96 |
| z-max with an oracle window (told the injection time) | 0.84 / 0.97 / 0.94 | 0.82 / 0.97 / 0.94 |
| What a pure graph can rank with SPARQL 1.1 aggregates | 0.63 / 0.94 / 0.89 | 0.68 / 0.97 / 0.90 |
| Published TORAI (metrics, logs and traces) | 0.83 / 0.95 / 0.93 | |
| Published CIRCA (metrics) | 0.67 / 0.89 / 0.85 | |
| Published BARO (metrics) | 0.67 / 0.82 / 0.80 | |
| Random | about 0.09 / 0.27 | |

The published baselines come from the TORAI paper's Table 2, averaged over all 90 cases. The RCAEval README flags its BARO row for RE2-OB as possibly copied from the Train Ticket table.

**How the defaults were chosen (`rcaeval-lag.txt`, `rcaeval-grid.txt`):**

- **Scale floor.** The first run scored 0.21 AC@1 because of sparse series. `redis_diskio` has a median and MAD of 0, so one spike gave z = 10^10. Two changes fixed it: a scale floor from the baseline's own range, and counting only rises. A drop in CPU is a symptom.
- **The known-pair prior.** I first read Netflix's prior as a multiplier on every link. The post says known pairs "carry a high prior weight" and other pairs are scored on proximity and severity. Implemented that way, chain ranking went from 0.09 (with strength capped at 1) to 0.80.
- **The window.** It is 30 minutes, not Netflix's 10. With a 10-minute window, chain ranking falls to 0.11 AC@1 when the incident opens 600 s after the fault. With 30 minutes it scores:

  | Incident lag | AC@1 |
  |---|---|
  | 120 s | 0.87 |
  | 300 s | 0.87 |
  | 600 s | 0.79 |
  | 900 s | 0.78 |
- **Proximity runs from the end of an episode (`rcaeval_diag.py`).** Strength saturates, so 87 of 90 winning roots had severity 1.0 and time proximity decided every ranking. Proximity was measured from an episode's start, so a one-point blip seconds before the incident beat a fault that began 290 s earlier and never stopped. Metric episodes now record `evt:until`, the last anomalous point, and a link's proximity is measured from it, capped at the next event or the incident. Equal confidences then prefer the chain that reaches more applications, then the earlier onset. The detector also waits for 150 s of baseline history before judging a point, which removes warm-up events at the start of each series (any value from 60 to 300 s gives the same scores). Misses fell from 12 to 6: loss went from 0.60 to 0.80 AC@1 and delay from 0.80 to 0.87, and stored events fell 31%. The misses were diagnosed on all 90 cases before the rule was written, so the held-out split is less independent here than for the weights; the tuning split did prefer this ordering over the reverse (0.90 against 0.87). Of the 6 left, 3 are same-second ties decided by name, 2 have the true service firing after the winner, and 1 is pre-fault noise. A "callee explains its caller" penalty and a raw-z tie-break were tried and rejected on held-out reps.
- **The lag table predates that change.** A replica of the ranking scored about 0.92 at 120, 600 and 900 s of lag after it, but the table above was not rerun.
- **The weight grid did not generalize.** Tuning propagation weights on rep 1 (206 combinations) gave no held-out gain, so the shipped weights are not tuned to labels.
- **The graph is worth about 5 points.** Ranking with the dependency edges ignored scored 0.79 AC@1, against 0.84 with them, measured with propagation ranking.
- **Signals.** Measured with propagation ranking, metrics alone scored 0.86 and all signals 0.84. On this dataset the log and trace episodes (146 events) added a little noise.

**The gateway caught a correlator bug.** Anomaly strength was not capped at 1, so chain confidences exceeded 1. The SHACL shape `corr:confidence <= 1` quarantined 75 chains. That exposed the bug, and it is fixed.

**Investigation memory (`similar-eval.txt`).** All 90 cases went into one store, with each incident's true chain accepted as a verdict. Then `similar` was run for each incident over the incidents before it:

- **Top-1.** The most similar past incident's accepted cause is the true root cause for 0.92 (82 of 89), against about 0.20 by chance. On the held-out reps 2-3 it is 59 of 60; on the tuning rep 1 it is 23 of 29.
- **Top-3.** It is among the top three for 0.94.
- **The signature sums strengths.** It used to keep the maximum strength per application. Strength saturates at 1, so the root application and its callers looked alike, and 0.74 (66 of 89) was the result. Summing over a 900 s window fixed 16 of 23 misses. The window was picked on rep 1, averaged over simulated gaps of 120 to 900 s between fault and incident, so it does not fit the benchmark's fixed 300 s gap. Four of the 7 remaining misses have no earlier incident with the same root cause, so 85 of 89 is the ceiling. Delay faults went from 15 to 13 hits. Each service and fault repeats three times in RE2-OB, so a stricter check removes every past incident with the same service and fault: the old signature gets 62 of 85 and the summed one 79 of 85, so the gain is not near-duplicate matching.

## Phase 6: model enrichers (`llm-gate.txt`)

Models run through pi on its existing login, `openai/gpt-6-luna` at $0.10 in and $0.50 out per million tokens. The prompt is generated from the ontology's vocabularies and definitions.

| Check | Result |
|---|---|
| Accuracy on 16 labeled threads | 15 of 16, then 16 and 15 in two repeat runs with no cache. The first prompt scored 13 of 16 in one run. See Phase 9 for what changed |
| Replay | A re-harvest made 0 model calls and wrote 1 quad (the harvest time) |
| Broken model (`scripts/eval/bad_model.py`) | Exercises non-JSON, out-of-vocabulary, invented-app, low-confidence and invented-call answers, plus a fabricated quote. All were quarantined, dropped or marked degraded. 0 invalid facts were committed, and the audit is clean |
| Correct rejections with the real model | The model named the vendors CardRail and MailRelay as applications. The catalog gate refused to create them |

## Phase 7: serve, agents and lab

| Check | Result |
|---|---|
| JSON-RPC over TCP, address in `.ograph/serve.json` | 0.1 to 1.7 ms per call |
| MCP over stdio | `initialize`, `tools/list` and `tools/call` work, with 8 tools |
| pi extension contract (`scripts/agent/test_pi.mjs`, `pi-contract.txt`) | Passes: tools registered, real calls, read-only SPARQL enforced, three misspelled names rejected with the right suggestion, reconnect after shutdown |
| A real pi agent run (`agent-run.txt`) | 22.7 s. It named ledger-db pool saturation with its chain, labeled as a hypothesis, the affected applications and processes, and the past mitigation |
| `ograph lab` | A 22 KB self-contained HTML page |

Phase 8, the AGE sink, is skipped: the plan ships it only when a team asks.

## Phase 9: ideas from NVIDIA auto-ontology

[NVIDIA/auto-ontology](https://github.com/NVIDIA/auto-ontology) builds a semantic layer over SQL. It is not an RDF system, so we ported five of its ideas instead of its code. Each idea has its own gate.

### Model proposes, telemetry confirms (`confirm.txt`, `gateway.txt`)

A model may now propose `ops:dependsOn` from a chat thread, with a quote that holds both application names. The gateway commits the edge only when the same edge exists from the catalog or from traces. Otherwise it quarantines the edge as "unconfirmed".

We also tested co-occurring events as a second kind of support, and rejected it. `confirm_eval` proposed all 9,900 ordered application pairs from the 90 RCAEval cases. The catalog declares 1,350 of them.

| Support rule | Precision | Recall |
|---|---|---|
| A trace edge only | 1.000 | 0.200 |
| Events co-occur within a minute, in 3 or more minutes | 0.182 | 0.067 |
| A trace edge, or co-occurrence in 3 or more minutes | 0.453 | 0.251 |
| A trace edge, or co-occurrence in 5 or more minutes | 0.885 | 0.205 |

The base rate is 0.136, so co-occurrence barely beats chance. During an incident, every service misbehaves in the same minutes. At 5 or more minutes, co-occurrence adds 7 real edges and 36 invented ones over traces alone. (Before the detector's warm-up rule removed noise events, the 3-minute row scored 0.221; the conclusion did not change.)

| Check | Result |
|---|---|
| New gateway kind: a confident model edge with no backing edge | Quarantined in all 3 cases. A model edge that the catalog declares is committed |
| Real model on the demo threads | It proposed 1 to 2 calls per run. Every committed call is declared in the catalog. It also proposed `payments-api -> ledger-db` with a quote that does not name payments-api, and code dropped it |
| Near-twin names | `inventory-service-uk` scored 0.98 against the catalog's `inventory-service-eu` and resolved to it silently. A fuzzy match now also needs a 0.03 margin over any other entity and must not differ by one whole unlike word. The new kind is quarantined 3 of 3; misspellings such as `inventry-service-eu` still resolve. RCAEval scores, the demo and the alias gate are unchanged |
| Determinism | A fuzzy catalog match with tied scores picked its winner in hash order, so a re-submit could rewrite quarantine reasons. Ties now break by IRI. Re-submits wrote 1 quad in 5 of 5 runs |

### SPARQL name check (`pi-contract.txt`)

A query that uses a class or property the ontology does not declare now fails, with the nearest declared name. Before, such a query returned no rows, which looked like an answer. Examples: `ops:dependOn` suggests `ops:dependsOn`, and `inc:severty` suggests `evt:severity`.

| Check | Result |
|---|---|
| False rejections | 0 of the 50 predicates and 16 classes stored in the demo graph |
| Agent contract | Three misspelled names were rejected through `serve --mcp` with the right suggestion. A correct query still answers |
| Found during the work | The check rejected `evt:source` in a query written for the alias eval. The property is `evt:sourceRecord` |

### Ontology coverage against competency questions (`coverage.txt`)

`ograph ontology coverage QUESTIONS` asks the model which declared name represents each concept in each question. Code keeps a mapping only when the name is declared and the words are in the question. Code also drops a "missing" concept that is a catalog application name. What is left goes into proposed Turtle for a person to review.

The set is 21 questions in `examples/demo/questions.txt`. We labeled 13 of them as covered and 8 as needing a concept the ontology lacks: on-call, SLO, revenue, cluster, feature flag, customer, error budget and runbook. We wrote these labels, so treat this set as a smoke test, not a benchmark.

| Check | First prompt | Shipped prompt |
|---|---|---|
| Gap questions reported with the expected gap | 8 of 8 | 8 of 8 |
| Covered questions reported covered | 9 of 13 | 11 of 13 |
| Proposals merged into the ontology | | 17 additions, 0 breaking in `ograph ontology diff` |

Three changes took covered questions from 9 to 11:

- **Provenance classes in the prompt.** We had left them out.
- **Catalog names dropped.** Code drops a gap that is a catalog name.
- **Time words.** The prompt says that time words compare `evt:at`.

The two remaining false gaps are "fails" and "facts". The gate requires every gap to be found, with at most two false gaps.

### Exact-quote aliases (`alias.txt`)

A new `docs` source reads runbooks. It takes an alias only from a phrase that states one outright, such as "payments-api, also known as pay-gw", "search-api (aka finder)" or "ledger-db (ledger-pg)". The gateway resolves the first name against the catalog, and it quarantines an alias that already names another application.

| Check | Result |
|---|---|
| Aliases committed from the demo runbooks | 3 of 3 expected, 0 unexpected |
| Decoys | 0 of 6 committed. "ledger-db (postgres)", "checkout (INC-4388)" and a version string matched no phrase. "the primary database (ledger-db)" and "orders-api is also called checkout" were quarantined |
| Alerts that use an alias name | 12 facts quarantined without runbooks. With them, both alerts resolve to payments-api and search-api |
| Root-cause chains for INC-5377 | Unchanged |

We first also let the model find aliases, with the same quote check. It found no alias that the phrases missed, and it added a false one, `ledger-db -> postgres`, whose quote passed. That path was removed, so no model reads runbooks.

In RCAEval, `frontend-external` stays quarantined as a near match for `frontend`, because no text states it is an alias. The Kubernetes Service selector does state it. Reading that selector belongs in `ogrep --entities`.

### An explicit "unknown" (`llm-gate.txt`)

The classifier must now quote the words that state a cause, or answer UNKNOWN. A label in the vocabulary that comes without a verifiable quote becomes UNKNOWN in code. A label outside the vocabulary still goes to the gateway, so the quarantine keeps a record of that failure.

Accuracy went from 13 of 16 to 15, 16 and 15 of 16 in three runs. The question thread is now UNKNOWN. The model answered UNKNOWN itself, with no quote, so the prompt made this fix. The code-level abstain never fired on the real model. On the broken model it turned every unquoted label into UNKNOWN. One network thread still flips between NETWORK and CAPACITY.

## Why not a pure knowledge graph (`purekg.txt`, `rcaeval-final.txt`)

A pure knowledge graph puts every metric point, log line and span in the graph as a node, so SPARQL can answer everything. I loaded one RCAEval case both ways: 24 minutes of an 11-service system, 96 MiB of raw exports.

| | Pure knowledge graph | ograph (episodes plus pointers) |
|---|---|---|
| Quads | 3,924,039 | 368 (10,000x fewer) |
| Store | 1,088 MiB | 0.30 MiB |
| Ingest | 15.8 s | 0.4 s for the whole harvest, including detection |
| "What happened in the 10 minutes before?" | 279,168 raw records, 5.5 MiB of text, in 398 ms | 11 episodes, 2.2 KiB, in 0.05 ms |
| Root cause on 90 cases | 0.63 AC@1 with the best statistic SPARQL 1.1 can express: a mean shift in units of standard deviation | 0.93 AC@1 |
| A day of this small system | 235 M quads, 64 GiB | 0.02 M quads |

These are the reasons, each measured above:

- **Size.** Raw telemetry is about 10,000 times larger as triples. At 196 to 256 bytes per quad, one day of an 11-service demo is 64 GiB. A real estate would multiply that by its service count.
- **Agents.** The answer an agent needs is the 11 episodes, not the 279,168 records. 5.5 MiB of text does not fit a model's context, and the episodes fit in 2.2 KiB.
- **SPARQL cannot express the detector.** SPARQL 1.1 has no median, MAD, window functions or online baseline. The best ranking it can express scores 0.63 AC@1, against ograph's 0.93.
- **Netflix made the same choice.** AutoRCA "queried live" the raw systems and queried only the graph's harvested facts with SPARQL (post, page 12). ograph keeps a pointer to the first raw record of each episode (`evt:sourceRecord`, or the evidence in `ograph why`) for drill-down.

The graph is still where the value is. Dependency edges add about 5 points of top-1 accuracy over ignoring them. Investigation memory over verdicts reaches 0.92 top-1. Provenance, quarantine and closed vocabularies let both people and agents trust what they read.
