# ograph: evidence per phase

Every number here comes from a script or binary in this repository, and its raw output is in `docs/evidence/`. `scripts/eval/run_all.sh` reruns every gate. It does not regenerate the one-off studies (`rcaeval-lag.txt`, `rcaeval-grid.txt`, `agent-run.txt`), the 10x store run, or the repeated uncached model runs of `scripts/eval/repeat_gates.py`; each section names its command.

Machine: Apple silicon with 12 cores and 32 GiB of RAM. The store is on an external disk. Measured on 2026-11-22 and 23.

## Phase 1: scaffold and build

| Check | Result |
|---|---|
| C toolchain in `cargo tree -e normal,build --target all` | No `cc`, `cmake` or `bindgen`. The only `-sys` crate is `windows-sys` (pure Rust declarations, no C build) |
| Clean release build of the CLI | 8 s wall. The binary is 10 MB |
| Oxigraph crates | `oxrdf` 0.3.5, `oxttl` 0.2.5, `spargebra` 0.4.8, and `spareval` 0.2.9, pinned exactly. `spareval` is touched only in `ograph-core/src/sparql.rs` and the inline-term conversion in `ograph-core/src/term.rs` |

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
| Load | 12.7 s, 278 K quads/s (4 M-quad transactions) | 401 s, 88 K quads/s |
| On disk after compaction | 658 MiB, 196 bytes/quad | 8.4 GiB, 256 bytes/quad |
| Open | 3 ms | 7 ms |
| W1 (incidents on one team's apps in one region), SPARQL / native / Oxigraph | 10.2 / 7.7 / 5.7 ms | 185 / 159 ms for 10.7x the rows |
| W2 (events in the 10 minutes before an incident), SPARQL / native / Oxigraph | 0.16 / 0.003 / 0.10 ms | 0.21 / 0.003 ms |
| W3 (blast radius of the hub), SPARQL / native / Oxigraph | 17.8 / 15.1 / 136.6 ms | 213 / 189 ms for 10x the rows |
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

Interleaved, sequential runs on 12 cases (3 each) give a median of 0.228 s before and 0.180 s after; the merged 90-case harvest goes from 14.8 s to 10.1 s (6 runs each). A later change parses traces with borrowed fields: the traces step on the largest case went from 160 ms to 119 ms and its peak memory from 320 MB to 133 MB (best of 5, sequential), with identical exports. Root-cause and similar-incident scores are unchanged. The rest of single-case time is trace parsing and 7 to 8 durable redb commits. Product ids such as `L9ECAV7KIM` escaped the masking rule, and so did ids after a `key=`. Masking upper-case ids with a digit, and judging the value after `=`, takes recommendationservice in one case from 9,113 distinct masked lines to 6, and cartservice from 57 to 4. Stored quads on the checked cases, RCAEval scores and similar-incident scores are unchanged; timing differences were inside the noise of a loaded machine.

## Phase 3: ontology, SHACL and gateway

| Check | Result |
|---|---|
| SHACL Core, W3C suite (`shacl-core.txt`) | 98 of 98 on `sh:conforms` and result count. rudof's measured 92 came from a harness that ignored `sht:dataGraph`; with a fixed harness it would likely match |
| `ograph ontology check` | 0 problems. Every IRI the code uses is declared |
| Gateway gate, 18 kinds of bad write plus 44 good events (`gateway.txt`) | 0 bad facts committed, every kind quarantined with its reason, 44 of 44 good events committed (4 of them misspelled or suffixed names that must still resolve), full audit clean, and a re-submit writes 1 quad (the harvest time) |

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
| 1. Which recent events could explain this incident? | `ograph before INC-5377`, `ograph chains INC-5377` | 9 events across the scope. Top cause ledger-db, with the chain `ledger-db:ALERT -> payments-api:LOG_ERROR -> payments-api:ALERT -> checkout:ALERT -> incident` (the designed cause) |
| 2. Which services and business processes are affected? | `ograph blast ledger-db` | 5 dependents by distance. Processes affected: card payments, online sales and order fulfilment |
| 3. Which past incidents resemble this one, and which mitigations worked? | `ograph similar INC-5377` | INC-4120, mitigation "raised ledger-db connection pool to 200". Its cause, ledger-db, shows only after the README's `verdict` step accepts INC-4120's top chain; before that the causes are empty |
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
| A fixed-window mean-shift ranking in SPARQL 1.1 aggregates | 0.63 / 0.94 / 0.89 | 0.68 / 0.97 / 0.90 |
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
| Accuracy on 16 labeled threads | 15 of 16, then 16 and 15 in two repeat runs with no cache. The first prompt scored 13 of 16 in one run. See Phase 9 for what changed. The gate fails below 14 of 16 (85%) |
| Replay | A re-harvest made 0 model calls and wrote nothing |
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
| Determinism | A fuzzy catalog match with tied scores picked its winner in hash order, so a re-submit could rewrite quarantine reasons. Ties now break by IRI. Re-submits wrote 1 quad, the harvest time, in 5 of 5 runs; an unchanged batch now writes nothing |

### SPARQL name check (`pi-contract.txt`)

A query that uses a class or property the ontology does not declare now fails, with the nearest declared name. Before, such a query returned no rows, which looked like an answer. Examples: `ops:dependOn` suggests `ops:dependsOn`, and `inc:severty` suggests `evt:severity`.

| Check | Result |
|---|---|
| False rejections | 0 of the 50 predicates and 16 classes stored in the demo graph |
| Agent contract | Three misspelled names were rejected through `serve --mcp` with the right suggestion. A correct query still answers |
| Found during the work | The check rejected `evt:source` in a query written for the alias eval. The property is `evt:sourceRecord` |

### Agent-written SPARQL (`sparql-agent.txt`)

`scripts/eval/sparql_agent_eval.py` asks 22 questions about the demo of a headless pi agent (`openai/gpt-5.6-terra`) with bash, read and the `skills/ograph` skill. The agent answers by writing SPARQL. Its final query is run, and its rows must equal those of the reference query in `examples/demo/sparql-questions.json`, on some choice of its columns, with a node counted as its `ops:name` (an incident as its `inc:id`). Four questions are close to a recipe in the skill; 18 are not (property paths, negation, aggregates, time arithmetic, joins across four classes). Each question ran 3 times, with the skill and without it.

| Arm | Recipe | Novel | All | Median time |
|---|---|---|---|---|
| With the skill | 10/12 | 54/54 | 64/66 (0.97) | 18.0 s |
| Without the skill | 12/12 | 52/54 | 64/66 (0.97) | 23.1 s |

- **Accuracy does not separate the arms on this set.** Without the skill the agent still writes correct queries. It can read `.ograph/ontology.ttl` in the workspace, which is the likely reason (inferred: the runner keeps no transcripts). The skill saves it about 5 s a question. A harder set or a smaller model may separate them; this one does not.
- **The misses are answer shape and one aggregate.** With the skill: 2 of 3 runs of "the top-ranked candidate" returned the whole ranking. Without it: one count of alerts by severity counted every event, and one final query kept an unescaped `/`.
- **Found during the work.** An id written as commands print it, `id:app/payments-api`, failed to parse with `expected OPTIONAL`, which does not mention the `/`. The error now ends `write id:app/payments-api as id:app\/payments-api inside a query, or use the full IRI`; the run recorded in `sparql-agent.txt` shows that message's earlier wording.
- **Limits.** One model, 22 questions written by the author of the reference queries, on a 10-application demo. A run that pi could not complete (a failed exit, an empty reply or no reply within 300 s, 3 times over, such as a rate limit) is reported as ERROR and not scored; this run had none.

### Ontology coverage against competency questions (`coverage.txt`)

`ograph ontology coverage QUESTIONS` asks the model which declared name represents each concept in each question. Code keeps a mapping only when the name is declared and the words are in the question. Code also drops a "missing" concept that is a catalog application name. What is left goes into proposed Turtle for a person to review.

The set is 21 questions in `examples/demo/questions.txt`. We labeled 13 of them as covered and 8 as needing a concept the ontology lacks: on-call, SLO, revenue, cluster, feature flag, customer, error budget and runbook. We wrote these labels, so treat this set as a smoke test, not a benchmark.

| Check | First prompt | Phase 9 prompt | Shipped (3 uncached runs) |
|---|---|---|---|
| Gap questions reported with the expected gap | 8 of 8 | 8 of 8 | 8, 8, 8 |
| Covered questions reported covered | 9 of 13 | 11 of 13 | 13, 12, 12 (mean 12.1 over 11 runs) |
| Held-out set B, 6 covered and 4 gaps, never inspected before the fixes were frozen | | 3, 4, 3 covered; 4 of 4 gaps | 6, 6, 6 covered, then 5 in the run on main (`coverage-heldout-b.txt`); 4 of 4 gaps |
| Proposals merged into the ontology | | 17 additions, 0 breaking | 15 additions, 0 breaking in `ograph ontology diff` |

Three changes took covered questions from 9 to 11:

- **Provenance classes in the prompt.** We had left them out.
- **Catalog names dropped.** Code drops a gap that is a catalog name.
- **Time words.** The prompt says that time words compare `evt:at`.

Four more changes took it to 12 or 13 (`scripts/eval/repeat_gates.py` reruns every model gate N times with no cache):

- **A failed model call no longer counts as covered.** A timed-out question used to pass silently. It is now reported FAILED and counted wrong, and model calls get one retry.
- **Model spellings of declared names.** "rca:state accepted" or a full IRI in angle brackets reduce to the declared name instead of being rejected.
- **Every class and property has an `rdfs:comment`.** 67 lacked one, and terms without a comment drew null mappings. A "feature toggle" comment on `ConfigChange` caused a false mapping and was removed.
- **One entry per multi-word concept.** The model had mapped "model" to `harv:LlmFact` and left "facts" null.

The false gaps left, "fails" and "facts", come and go from run to run, and both labels are arguable. Held-out set A (`questions-heldout.txt`) went from 4 to 5 covered to 6 of 6, but its baseline errors were seen before the comments were written, so set B (`questions-heldout-b.txt`) is the clean number. Asking the model to quote the comment of the closest declared term made it stricter, not more accurate (about 10 of 13, 23 s instead of 15 s), and was reverted. A majority of three samples gave the same mean at three times the calls.

### Exact-quote aliases (`alias.txt`)

A new `docs` source reads runbooks. It takes an alias only from a phrase that states one outright, such as "payments-api, also known as pay-gw", "search-api (aka finder)" or "ledger-db (ledger-pg)". The gateway resolves the first name against the catalog, and it quarantines an alias that already names another application.

| Check | Result |
|---|---|
| Aliases committed from the demo runbooks | 3 of 3 expected, 0 unexpected |
| Decoys | 0 of 6 committed. "ledger-db (postgres)", "checkout (INC-4388)" and a version string matched no phrase. "the primary database (ledger-db)" never reached the gateway: the two names share no part, so the enricher drops it. "orders-api is also called checkout" was quarantined |
| Alerts that use an alias name | 14 facts quarantined without runbooks. With them, both alerts resolve to payments-api and search-api |
| Root-cause chains for INC-5377 | Unchanged |

We first also let the model find aliases, with the same quote check. It found no alias that the phrases missed, and it added a false one, `ledger-db -> postgres`, whose quote passed. That path was removed, so no model reads runbooks.

In RCAEval, `frontend-external` stays quarantined as a near match for `frontend`, because no text states it is an alias. The Kubernetes Service selector does state it. Reading that selector belongs in `ogrep --entities`, and Phase 11 measures an ogrep that does.

### An explicit "unknown" (`llm-gate.txt`)

The classifier must now quote the words that state a cause, or answer UNKNOWN. A label in the vocabulary that comes without a verifiable quote becomes UNKNOWN in code. A label outside the vocabulary still goes to the gateway, so the quarantine keeps a record of that failure.

Accuracy went from 13 of 16 to 15, 16 and 15 of 16 in three runs. The question thread is now UNKNOWN. The model answered UNKNOWN itself, with no quote, so the prompt made this fix. The code-level abstain never fired on the real model. On the broken model it turned every unquoted label into UNKNOWN. One network thread flipped between NETWORK and CAPACITY: "resolvers there were overloaded" matched the CAPACITY definition word for word. One sentence in the NETWORK definition (use it even when the network component failed because it was overloaded) fixed it in 9 of 9 runs, but it came from looking at that one thread. Over 9 uncached runs the mean was 15.7 of 16 (those logs are not checked in), and the recorded gate run (`llm-gate.txt`) scored 16. The residual misses moved: the elasticsearch thread came back CAPACITY in 2 of 9 runs, and the WAL thread came back UNKNOWN once.

**Voting did not pay.** Three samples from one model with a majority vote scored 15, 15, 15 at 48 calls instead of 16, because the DNS miss was systematic. Cross-model voting with a second model through pi, 2 models × 2 samples, scored 15 right and 1 abstained at 64 calls. After the DNS fix, both models once agreed on a wrong label. Voting cost a correct label and did not stop a wrong one, so it is not shipped.

## Phase 10: code through ogrep (`code.txt`, `code-regression.txt`)

ograph used one ogrep feature, `--entities`. This phase adds six more: commits as events (`--timeline`), the statement behind a log burst (`--template-to-code`), the frames of a stack trace (`--stack-trace`), past write-ups (`--similar-incidents`), the files an incident's text is about (`--about`), and a ranking from the dependency graph and recent commits when there is no telemetry (`--suspects`). `scripts/eval/code_eval.py` measures each on the OpenTelemetry demo repository at `8f6908eae154`: 34 services, 2,493 commits and 88 log statements in 12 languages. The repository, its commits, its dependency graph and its log statements are real. The incidents and the telemetry are synthetic, each built from one real commit or one real statement, so the right answer is known. Measured on 2026-11-24 with ogrep 0.2.0.

| Question | Result |
|---|---|
| A burst of a real log message: does `evt:codeLocation` name the statement? | Right file for 84/88 (0.95), exact line for 83/88 |
| An error report with a stack trace in the file's language: does it name the line? | Exact line for 86/88 (0.98). The two misses are C# frames (`in path:line 12`), a form ogrep does not read |
| `ograph code`: is the statement's file among the files to open? | First for 74/88 (0.84), in the first three for 87/88 (0.99) |
| A real commit to a file, then errors from that file: is the commit shown? | Listed for 24/24, and first with the file it touches for 23/24. Without the code source the graph holds no commit |
| 36 incidents with no telemetry and 36 write-ups that word each fault differently | Right write-up first for 30/36 (0.83). `ograph similar` has nothing for any of them, since it compares telemetry |
| 40 incidents with no telemetry at all, the repository as it was at a real commit to the cause | `ograph chains` ranked nothing before. The fallback names the cause first for 34/40 (0.85) and in the first three for 40/40 |

The six misses on write-ups are one fault of the six, whose incident title shares no word with its write-up. Text matching needs shared words.

### What it does not change

- **Existing workspaces.** `scripts/eval/ab_compare.sh` runs the binary from before this phase and the new one over the demo and RCAEval. 12 of 12 outputs are byte-identical: the demo's harvest and eight queries, the 90 per-case rankings (chain AC@1 0.93) and the similar-incident scores (0.92). Query latency is the same within 0.5 ms (23 ms, which is mostly process start).
- **Ranking with a code source.** Commits join the candidate events of a chain. In 40 cases with a strong fault on one dependency and an unrelated real commit on another four minutes before the incident, the right application stayed first in 40/40, with and without the code source, and no top chain was rooted at the commit.

### What was tried and removed

A scoring rule gave a commit a 0.9 prior on the link to a later event located in a file it changed. On the 24 commit cases it never changed which application ranked first (4/24 with it, without it, and without the code source: the unrelated metric spike on a dependency wins in the other 20). At 0.9 it never rooted the top chain at the commit, because a chain it lengthens scores below the shorter one; at 1.0 it did so in 6/24. The rule is gone. `ograph code` shows the commit and the file instead, and chain scores do not use them.

### Cost

| Workspace (112 log files, 100 metric files, 188 incidents) | Harvest | Distinct triples |
|---|---|---|
| Telemetry and catalog only | 3.0 s | 6,688 |
| With the code and errors sources, commits of the last 30 days (the default) | 3.8 to 4.6 s in two runs | 9,171 |
| With every commit since 2020 (2,493 commits, 1,925 commit events) | 3.9 to 5.1 s | 35,023 |

`ograph code` takes a median of 67 ms. The first version ran each ogrep call in turn and cost 5.9 s more than the plain harvest. One call for all log files was faster again but found the right file for 75/88 bursts, not 84, because ogrep merges similar messages into one template and answers only for its first line. The harvest now makes one call per service, per stack trace and per log file, on up to eight threads.

Not measured: a Windows run, a repository larger than 654 files, and whether commit events change `ograph similar` on a workspace that has both a code source and accepted verdicts.

## Phase 11: more from ogrep (`catalog.txt`, `code-history.txt`, `log-new.txt`, `code-regression.txt`)

Phase 10 ended with a list of what else ogrep could give. This phase builds each item and measures it. Three of them needed changes in ogrep itself, which were on its `ops-gaps` branch when measured (five commits after 0.2.0) and are released as ogrep 0.3.0; every number below says which ogrep it was measured with. Measured on 2026-11-24.

| Addition | Result |
|---|---|
| Inferred calls as dependencies (`catalog`, on by default) | On RCAEval, a catalog read from Online Boutique's own Kubernetes manifests scores AC@1 0.32 with declared dependencies only, and 0.93 with the inferred calls, the same as the hand-written catalog |
| The commit that first added a log message (`ograph code`, `ogrep --introduced`) | The commit `git log -S` gives for 81/88 (0.92), an older commit for 4, none for 3 (rightly for 2). Median age 443 days, so none of them is in the 30-minute window |
| The first lines of a new error message (`LOG_NEW`) | Synthetic: an event for 88/88 single or double lines the burst rule misses, 0 events on 128 controls. RCAEval: 11 events in 90 cases, 9 on the injected service or a caller of it, and 0 of 90 rankings change |
| What a deployment changed (`ogrep --timeline X --since P --until V`, newer ogrep) | Changed files for 40/40 deployments between release tags, holding every file `git log P..V` lists for the service. `ograph code` puts the deployment first with the located file for 40/40 |
| Tests and callers of a located line (skill text only) | `ogrep --related-tests` puts the conventionally named test first for 3/3 (the repository has only three such pairs). `ogrep --callers -w` lists every file with a call for 36/40 functions |

### Inferred calls (`catalog.txt`, `scripts/eval/catalog_eval.py`)

`ogrep --entities` reports two kinds of edge: `depends_on`, which a file declares, and `calls`, which it infers from an environment variable that holds another service's address. ograph took only the first. Kubernetes manifests declare no dependencies, so a catalog read from them had applications and no edges.

RCAEval RE2-OB runs on Online Boutique. Its fixture catalog is a Backstage file written by hand with 15 dependencies. The project's own `release/kubernetes-manifests.yaml` gives 16 inferred calls: the same 15 (it names the cache `redis-cart`, the fixture names it `redis`) and `loadgenerator -> frontend`.

| Catalog | Dependencies it gives | Chain AC@1 | AC@3 |
|---|---|---|---|
| Hand-written Backstage file | 15 | 0.93 (84/90) | 0.99 |
| Manifests, declared only (`inferred = false`) | 0 | 0.32 (29/90) | 0.33 |
| Manifests, with inferred calls (the default) | 16 | 0.93 (84/90) | 0.99 |

The first application is the same in all 90 cases between the first and the third row. The list of five differs in 49, and in all 49 the lists agree once `redis` is taken out: the telemetry calls the cache `redis`, which the manifests do not name. On the OpenTelemetry demo, 46 of ogrep's 56 inferred calls are also declared in Compose, and 10 are not, among them `shipping -> quote` and `frontend-proxy -> image-provider`. We read those 10 and each is a real address in the service's environment. A call that is also declared is recorded as declared only. `telemetry` and `observes` edges are not taken: every service exports to the collector, which would put it upstream of everything.

### The commit that first added a message (`code-history.txt`, `scripts/eval/code_history_eval.py`)

The commits `ograph code` lists are those in the correlation window. A message can enter the code long before it is first written. For each located log line, `ograph code` now asks `ogrep --introduced` for the commit that first added the longest constant part of its template, among the commits made until the incident.

On the 88 real log statements of the OpenTelemetry demo, each written as a burst within the last week, the answer is the commit `git log -S` gives for the whole statement in 81. In 4 it is an older commit: the constant part was in the code before the statement got its present wording. In 3 there is none. For 2 of those that is right: the statement was added after the incident's time, and git has none either. In the third every constant part of the template is shorter than the 12 characters ograph asks about. The commits are a median of 443 days old. With the lookups `ograph code` takes a median of 137 ms. They need `ogrep index --history`, which took 1.3 s and 99 MB for the 2,493 commits; without it `ograph code` answers as before and `degraded` says how to build it.

### New messages (`log-new.txt`, `scripts/eval/lognew_eval.py`)

The burst rule needs three lines of a template in a minute. One or two first lines of a message a service never wrote were not events. `ogrep --log-templates --new-since` flags such messages, but it reads only plain lines with a leading ISO timestamp and gives no service, so it returned nothing for JSON logs. ograph's own Drain already holds each template's first line, so the rule is there: the first error line of a template absent from the 600 seconds of the service's log before it is an event with signal `LOG_NEW` and the lowest severity, unless a burst of the same template says it already.

- **Synthetic.** 88 logs with forty quiet minutes and then one or two lines of a real message. Without the rule 0 of 88 have any log event and `ograph before` is empty for all 88. With it all 88 have an event, and it points at the right file for 84. 40 logs where the message recurs from the start and 88 bursts give 0 `LOG_NEW` events.
- **RCAEval.** The 90 cases hold 46 bursts and now 11 new messages, in 7 cases, all after the fault was injected: 5 on the injected service, 4 on a service that calls it, 2 elsewhere. One is `failed to send order confirmation ... code = Unavailable` on checkoutservice, once, when the fault was on emailservice. No ranking changes (0 of 90).

The rule adds visibility, not accuracy: it did not move any ranking here. No causal pair was added for it.

### What a deployment changed (`code-history.txt`)

A deployment record names a version. When that version and the one the service ran before are revisions of a code repository, ograph asks `ogrep --timeline SERVICE --since P --until V` and keeps the files of those commits on the deployment, up to 200. ogrep 0.2.0 read a tag in `--since` as an approximate date (5 commits to checkout since tag 2.0.0, where git lists 102) and had no `--until` there. The `ops-gaps` branch bounds by ancestry: 20 commits for `2.0.0..2.1.0`, the same 20 `git log` lists.

For 40 pairs of consecutive release tags and a service that changed between them, every deployment carries files, and every file `git log P..V -- src/S` lists is among them. 70% of the files it carries are outside `src/S`: ogrep assigns a whole commit to each service it touches, and a dependency update often touches several. With a burst of a message from one changed file ten minutes after the deployment, `ograph code` lists the deployment first with that file under `touches` for 40/40. With ogrep 0.2.0 the deployments keep no files and the harvest line says `DEGRADED` with the reason.

### Gaps closed in ogrep (`ops-gaps` branch)

| Gap | Measured |
|---|---|
| `--until` for `--timeline` and `--suspects` | `ograph suspects INC` now passes the time the incident opened. With 0.2.0 it asks again without it and says that later commits count too |
| `--timeline A,B,C` | The code harvest makes one call for all services, where 0.2.0 needs one per service. On the Phase 10 workspace the harvest took 4.33 s with one call and 4.61 s with 34; Phase 10 measured 3.8 to 4.6 s for the same harvest, so the gain is within the noise |
| C# stack frames | Stack frames resolve for 88/88 error reports, up from 86/88 |
| A Kubernetes Service that selects one workload is its alias | With the manifests as the catalog, 0.2.0 makes `frontend-external` a second application, which takes 74 of the 510 events on the frontend over the 90 cases. With the branch all 510 are on `frontend`, and the first application is still the same in 90/90. The hand-written catalog holds the 666 facts of those 74 events in quarantine |
| A revision in `--since` or `--until` bounds by ancestry | See deployments above |

ogrep's own suite passes with the five commits (1,139 tests) and its `make check` is clean.

### What it does not change

`scripts/eval/ab_compare.sh` with the binary from before Phase 10 and the one after this phase: 12 of 12 outputs byte-identical (the demo's harvest and eight queries, the 90 RCAEval rankings at chain AC@1 0.93, the similar-incident scores at 0.92), and query latency within 0.5 ms (24 ms). A fresh harvest of the largest RCAEval case (17 MB of logs, 81 MB of traces) takes 0.29 s with both. `code-after-phase-11.txt` repeats the Phase 10 evaluation with both ogrep versions. With 0.2.0 its six results are the same. Two things differ: the store holds 10 more triples, the inferred calls of that repository, and `ograph code` takes a median of 80 ms where it took 67 ms. That workspace has no history index, so each call now asks ogrep for the introducing commit and gets a refusal (the cause is inferred, not measured).

Not measured: Windows, a repository with real deployment records (the versions here are release tags), `--related-tests` on a repository with more than three conventionally named tests, and whether `LOG_NEW` events change `ograph similar` scores on workspaces with accepted verdicts. One risk in ogrep's revision rule is inferred, not measured: a value of four or more hexadecimal characters, such as `100d`, is read as a commit when it is a unique abbreviation of one.

## Why not a pure knowledge graph (`purekg.txt`, `rcaeval-final.txt`)

A pure knowledge graph puts every metric point, log line and span in the graph as a node, so SPARQL can answer everything. I loaded one RCAEval case both ways: 24 minutes of an 11-service system, 96 MiB of raw exports.

| | Pure knowledge graph | ograph (episodes plus pointers) |
|---|---|---|
| Quads | 3,924,039 | 370 (10,000x fewer) |
| Store | 1,088 MiB | 0.52 MiB |
| Ingest | 15.9 s (0.8 s parse, 15.1 s load) | 0.23 s median per case for a first harvest, including detection; each batch is one durable commit so other processes can share the store |
| "What happened in the 10 minutes before?" | 279,168 raw records, 5.5 MiB of text, in 387 ms | 12 episodes, 2.6 KiB, in 0.07 ms |
| Root cause on 90 cases | 0.63 AC@1 for one baseline written in SPARQL 1.1: a fixed-window mean shift in units of standard deviation | 0.93 AC@1 |
| A day of this small system | 235 M quads, 64 GiB | 0.02 M quads |

These are the reasons, each measured above:

- **Size.** Raw telemetry is about 10,000 times larger as triples. At 196 to 256 bytes per quad, one day of an 11-service demo is 64 GiB. A real estate would multiply that by its service count.
- **Agents.** The answer an agent needs is the 12 episodes, not the 279,168 records. 5.5 MiB of text does not fit a model's context, and the episodes fit in 2.6 KiB.
- **SPARQL cannot express the detector.** SPARQL 1.1 has no median, MAD, window functions or online baseline. A fixed-window mean-shift ranking, which it can express, scores 0.63 AC@1 against ograph's 0.93. That is one baseline, not an upper bound on SPARQL rankings.
- **Netflix made the same choice.** AutoRCA "queried live" the raw systems and queried only the graph's harvested facts with SPARQL (post, page 12). ograph keeps a pointer to the first raw record of each episode (`evt:sourceRecord`, or the evidence in `ograph why`) for drill-down.

The graph is still where the value is. Dependency edges add about 5 points of top-1 accuracy over ignoring them. Investigation memory over verdicts reaches 0.92 top-1. Provenance, quarantine and closed vocabularies let both people and agents trust what they read.

## Phase 12, lane F: do agents with ograph name the right root cause (`agent-rca.txt`)

`scripts/eval/agent_rca_eval.py` gives an agent one open incident and asks which application is the root cause. The reply ends with `ROOT: <application>`, and a run passes when that is the labeled root (AC@1). Four arms answer the same incidents:

| Arm | What answers |
|---|---|
| `mcp` | Headless pi with only the ograph tools, through the extension `scripts/agent/install.py` writes |
| `skill` | Headless pi with `skills/ograph`, bash and read, and `ograph` on `PATH` |
| `raw` | Headless pi with bash and read over the raw source files. No store and no `ograph` |
| `chains` | No agent. Rank 1 of `ograph chains`, the floor |

**The result is partial.** The plan was 39 cases, 3 runs each. The pi login reached its usage limit 21 minutes into the main run, during run 1. The numbers below are one run per case on the cases that finished. Flakiness across runs was not measured. The 5 cases that `chains` ranks wrong outside the sample (`--misses`) got no agent run. Rerunning the main command finishes the run, because rows in `docs/evidence/agent-rca/runs.jsonl` are kept.

**Cases.** 30 RCAEval cases, one per root service and fault type, with the repetition drawn by a fixed seed. The 4 demo incidents are a second suite. I labeled them from the story `examples/demo/generate.py` writes, so they are author labels.

**Gates for the harness, both pass.**

- **The floor is the published scorer.** The `chains` arm gives the same rank 1 as `rcaeval_score.py` on 35 of 35 cases, on each of 3 runs. AC@1 is 0.8286 for both on those 35 (the sample plus the 5 extra misses), and 29 of 30 on the sample.
- **Planted leaks are caught.** `scripts/eval/test_agent_rca_eval.py` plants a labels file, a note that names the dataset and the cases directory, and a symlink in a workspace. It plants transcripts that read the labels file, climb out with `..`, list the home directory, list another run, or use `ograph` in the `raw` arm. Each is flagged, and a clean workspace and a clean transcript are not. End to end, a planted labels file voids every run of that case before any arm is called. 14 of 14 tests pass.
- **The real workspaces are clean.** A separate `grep` over the pilot's 28 kept workspaces found no file that names the dataset, the labels file or the repository, and no label-shaped JSON. The harness voided 0 of 112 runs in the main run.

**How the labels stay hidden.** Each run gets a fresh copy of the case under a neutral path. The catalog is copied in and the config rewritten, because the exported config pointed at the repository by absolute path. The binary and the skill are copies too. The environment drops every variable that names the repository. The `raw` arm gets a stub `ograph` that says "command not found", because a real one is installed on this machine.

**RCAEval sample, one run per case (`agent-rca.txt`, section 5):**

| Arm | AC@1 | On the 22 cases every arm answered | Says "hypothesis" | Tool calls | Wall | Tokens | Cost per run |
|---|---|---|---|---|---|---|---|
| `mcp` | 18 of 26 (0.69) | 0.68 | 25 of 26 | 7 | 25 s | 22,574 | $0.036 |
| `skill` | 15 of 25 (0.60) | 0.59 | 1 of 25 | 12 | 40 s | 85,169 | $0.098 |
| `raw` | 19 of 22 (0.86) | 0.86 | 0 of 22 | 14 | 89 s | 225,366 | $0.171 |
| `chains` | 29 of 30 (0.97) | 0.95 | | 0 | 0.1 s | 0 | 0 |

Tool calls, wall time and tokens are medians. Cost is the list price pi reports for the tokens.

**An agent with ograph did not beat `chains` rank 1. It did worse.**

- **Against rank 1.** `mcp` was worse on 7 cases and better on none (sign test p = 0.016). `skill` was worse on 9 and better on none (p = 0.004).
- **Every departure from rank 1 was wrong.** `mcp` answered something other than rank 1 on 8 of 26 runs and was right on none, while rank 1 was right on 7 of them. `skill` departed on 10 of 25 and was right on none, while rank 1 was right on 9.
- **The one sampled case where rank 1 is wrong was not recovered.** On `c-3a0809d8d0` both ograph arms answered `redis`. The `raw` arm answered `currencyservice`, which is right.

**An agent with ograph did not beat the raw-files agent either.** `mcp` was better on 2 cases and worse on 6 (p = 0.289). `skill` was better on 2 and worse on 8 (p = 0.109). The direction is against ograph and the sample is too small to call the gap settled. The raw-files agent also did not beat rank 1. It was better on 1 case and worse on 3 (p = 0.625).

**What ograph bought was cost, not accuracy.** The `mcp` arm used a tenth of the raw arm's tokens and under a third of its time. The `skill` arm used 38% of the tokens and 45% of the time.

**Why the ograph agents miss: `redis` (`agent-rca.txt`, section 6).** 14 of their 18 wrong answers are `redis`, which is never the labeled root. 18 of the 30 sampled cases have a redis event at severity 1.0 before the incident. In the transcripts I read it is a disk I/O anomaly. The agents answered `redis` on 7 of 17 such runs in each arm, and on 0 of 17 runs where no redis event reaches 1.0. `chains` ranks redis between 2 and 6 in those cases, so the ranking discounts the event and the agent does not. The replies give the reason. The redis event is the earliest high-severity event, and redis sits upstream of `cartservice`, so the agent calls the rank-1 application a downstream symptom. `redis_diskio` is the sparse series Phase 5 describes, where one spike against a flat baseline saturates severity. The `raw` arm never answered `redis`.

**The two agent surfaces differ on calling the cause a hypothesis.** The extension's instructions say "A chain is a hypothesis. Say so", and 25 of 26 `mcp` replies used the word or said the cause was unconfirmed. `skills/ograph` says the same in its step list, and 1 of 25 `skill` replies did. The check is a regular expression (`HYPOTHESIS` in the script), so it counts wording, not judgment.

**Demo incidents, from the pilot, one run each (`agent-rca.txt`, section 3).** `skill` and `raw` named the root on 4 of 4. `mcp` and `chains` named it on 3 of 4. The miss is INC-4501, whose root is `elasticsearch`. It has no telemetry in the demo, so `chains` ranks `search-api`, and the `mcp` agent repeated that. The agents with file access read it in the chat thread. Four cases with author labels show the harness works on a second data shape and little more.

**What this does not show.**

- **One model.** Every run used `openai/gpt-5.6-terra`. Another model may weigh the redis event differently.
- **One run per case.** The `stable_pass` and `stable_fail` columns in the output mean pass and fail here.
- **A high floor.** `chains` is right on 29 of the 30 sampled cases, so the sample can show an agent doing worse far more easily than doing better. The `--misses` suite exists to test recovery, and it did not run.
- **No fix was tried.** The redis finding points at two candidates: how the tools present a saturated severity on a sparse series, and whether the skill should tell an agent to keep rank 1 unless it can name evidence the ranking lacks. Neither was built, because any such change needs cases this run has not already shown.

**A harness bug the pilot caught.** The first pilot's `mcp` arm ran with no tools, because the harness read the extension's tool list with a pattern that matched nothing. Every answer was "unknown". It was fixed before the main run, and an extension that registers no tools is now a skipped arm.

**Arms are a registry.** `ARMS` in the script maps a name to how the arm answers, whether its workspace holds a store, and what tool calls it forbids. `--arms` picks by name. An arm that cannot run here is a skipped row in the output, and `--strict` turns that into a failure.

**Cost and time.** The pilot was 28 agent runs for $1.37. The main run made 73 scored agent runs in 21 minutes at `--parallel 5` for $7.15. The full plan at these rates is about $36.

## Phase 12, lane G: what ograph knew at a time (`known-at.txt`, `known-at-unchanged.txt`, `store-ab.txt`)

A harvest used to overwrite: `replace_graph` wrote the difference and the removed quads were gone, so nobody could ask what `chains` answered last week, or what the catalog said when an incident opened. semantica keeps `recorded_at` and `superseded_at` on every fact and recomputes a closure for a `known_at` time (`semantica/kg/temporal_model.py:28-66`, `semantica/reasoning/temporal_truth_maintenance.py:154-168`). ograph takes the recording-time half of that and stores it only for what changes.

**Verdict: KEEP.** The gates below passed, and nothing that existed got slower beyond noise. The valid-time half is not built, because ogrep cannot date a dependency line (see the end of this section).

### What was built

- **Three tables.** `born` holds, per graph, when its present quads began. `added` holds a present quad written to a graph that already existed. `history` holds a removed quad with the time it was added and the time it was removed. A first harvest writes one `born` row per graph and nothing else, so the history grows with what changes.
- **`Snapshot::known_at(t)`.** It reads `added`, `history` and every graph born after `t`, then answers every read (`quads`, `objects`, `subjects`, `contains`, `events_on`, `graphs`) as the store would have at `t`. Live rows and restored rows merge in key order, so a pattern's first match is the one the store gave then. The native paths and the SPARQL view read through it unchanged.
- **Commands.** `--known-at T` on `chains`, `before`, `blast` and `sparql`; `ograph changes --since T`; `history_days` (default 90), applied by `ograph compact`.
- **Limits stated in the output.** Standard error says how many quads the view hides and restores and when the history starts. A time before that is an error. The view sees only episodes a harvest recorded and the history still holds, so a root whose telemetry left the sources is a candidate again in a view of a time when the store held it, and only there.

### Gates, written before the code

| Gate | Pass condition | Result |
|---|---|---|
| Scripted sequence: catalog v1, an incident, then catalog v2 with one new dependency and one changed ownership fact | Each of 10 commands (`chains`, `before`, `blast`, `sparql`, human and JSON) prints something else now, and with `--known-at t1` prints what it printed at t1, byte for byte and with the same exit code | 10 of 10 |
| | `changes --since t1` lists exactly the two changes | 2 listed: `+ id:app/checkout ops:dependsOn id:app/fraud-api` and `~ id:app/ledger-db ops:supports id:process/settlement (was id:process/billing)` |
| No-op harvest | The store file grows by 0 bytes | 0, 0, 0 bytes over three harvests; 0 bytes on each of 90 RCAEval cases |
| Retention | `compact` with `history_days = 0` leaves no history, and t1 is then refused | 17 rows dropped, 0 left, exit 2 |
| RCAEval, 90 cases: harvest, save `chains` in both rank modes, empty the metrics and traces files, harvest again | `chains --known-at t1` equals the saved output | 90 of 90 equal, while 90 of 90 differ now |
| Current answers | RCAEval per-case rankings identical between the build before this lane and the build after | identical on 90 cases; chain AC@1 0.93, AC@3 0.99 |
| | The demo's harvest and eight queries, byte for byte | 9 of 9 identical, with no model call |
| SPARQL correctness | 0 mismatches over 41 queries against Oxigraph and the native paths | 0 in each of 5 runs |
| Store latency | W1, W2 and W3 not worse beyond run-to-run noise | see the table below |

ograph has no owner fact that a source writes (`ops:ownedBy` is declared but no enricher emits it), so the "owner change" of the scripted sequence is the ownership file moving `ledger-db` from one business process to another.

### Cost

`store_ab.py` runs the store bench of the build before this lane and the build after in turns, five runs each, on a machine that other work kept busy (load average 2.8 to 3.7 on 12 cores). The bench files were on the internal disk, because one run on the repository's disk stalled for 7 minutes at 1% CPU under the other work. Median, with the range in parentheses:

| | old | new | new / old |
|---|---|---|---|
| load (s) | 13.300 (13.100 to 17.100) | 13.600 (13.300 to 14.900) | 1.02 |
| on disk after compaction (MiB) | 658.400 (658.400 to 658.400) | 658.400 (658.400 to 658.400) | 1.00 |
| open (ms) | 3.546 (2.598 to 3.858) | 4.309 (3.275 to 17.836) | 1.22 |
| W1 SPARQL p50 (ms) | 13.582 (11.370 to 14.609) | 11.491 (10.299 to 13.481) | 0.85 |
| W1 native p50 (ms) | 10.193 (8.487 to 14.661) | 10.099 (7.911 to 13.780) | 0.99 |
| W2 SPARQL p50 (ms) | 0.172 (0.164 to 0.173) | 0.174 (0.167 to 0.189) | 1.01 |
| W2 native p50 (ms) | 0.004 (0.004 to 0.004) | 0.004 (0.003 to 0.004) | 1.00 |
| W3 SPARQL p50 (ms) | 18.775 (18.070 to 20.199) | 19.463 (18.314 to 19.981) | 1.04 |
| W3 native p50 (ms) | 16.630 (16.444 to 17.575) | 16.436 (15.617 to 16.853) | 0.99 |
| re-harvest, unchanged (ms) | 69.912 (69.120 to 74.131) | 69.410 (67.507 to 81.243) | 0.99 |
| re-harvest, 3,276 changed (ms) | 479.846 (462.159 to 482.845) | 477.205 (423.595 to 510.320) | 0.99 |
| re-harvest back (ms) | 467.435 (457.283 to 517.093) | 468.618 (457.109 to 516.828) | 1.00 |

No number separates the two builds: every ratio is inside the other build's range. The re-harvest that replaces 3,276 quads takes the same 0.48 s although the new build also writes 3,276 `history` rows and 3,276 `added` rows, because one durable commit bounds it (the same replace took 0.16 s on the other disk). `open` has overlapping ranges, 2.6 to 3.9 ms against 3.3 to 17.8 ms; a snapshot now opens four more tables, and five runs do not show whether that costs anything.

- **Size.** A new store is larger by the three empty tables: 16,293 bytes per RCAEval case, summed over 90 first harvests (50.1 MiB against 48.7 MiB of 552 KiB files that hold about 440 quads each). At 3.52 M quads the compacted file is the same 658.4 MiB, 196 bytes per quad.
- **Growth per real re-harvest.** The bench replaces 3,276 of a day graph's 98,268 quads and then puts them back. That leaves 6,552 rows in `history` (480 KiB, 75 bytes per row) and 3,276 in `added` (136 KiB, 42 bytes per row): about 115 bytes for each replaced quad, against 196 bytes for a quad the store holds. On RCAEval, emptying two source files leaves a median of 338 rows and 28,672 bytes in the history tables. The file itself moves in allocator pages: +32,768 bytes for the 17 rows of the scripted sequence.
- **Reading a view.** Built in 4.6 ms for 3,276 hidden and 3,276 restored quads at 3.52 M quads, with 0 mismatches over 44 answers against the answers saved when the store held that state. W1 and W3 on the view cost what they cost live. W2 takes 0.15 ms natively against 0.003 ms live, because the time index describes the store now and a view finds an application's events without it. One `chains --json` process took a median of 86 ms with `--known-at` and 90 ms without on 20 RCAEval cases, the two taking turns: process start dominates both.

### What it does not do

- **Valid time on dependency edges is not built.** The request was to withhold a dependency from correlation when the catalog gained it after the incident. `--known-at` covers the case where ograph harvested before the incident. For a first harvest after the incident, ograph would need the date of each dependency line, and ogrep does not give it: `ogrep --entities . --json` (0.2.0 and 0.3.0) returns each edge with `from`, `to`, `relation`, `provenance`, `text` and `evidence {file, line}` and no commit or date, and `ogrep --entities . --at REV` answers `--at cannot be combined with --entities`. `ogrep --introduced PATTERN` dates a text, not a line: `- fraud-api` under one service is the same text as under another. ograph would use either of these if ogrep had them: entities at a revision, or the commit that introduced each edge.
- **The time is recording time.** A store that first harvests a year of exports today knows all of it from today.
- **`serve`, `similar`, `why`, `code`, `news` and `lab` have no `--known-at`.**
- **Not measured:** a view far back on a large store. Building one reads every graph first written after the time, so its cost grows with what was added since, and it keeps those quads in memory.

## Phase 12, lane A2: verdict history, evidence policy, replay (`verdicts.txt`, `replay-grid.txt`)

**Verdict history.** Judging a chain again used to replace its verdict. Now the chain's graph keeps every verdict, and each earlier one is `rca:supersededBy` the next. The gate is a contract test, `judging_a_chain_again_keeps_every_verdict_and_the_latest_decides`. It accepts a chain, rejects it, re-harvests, and accepts it again. Both events stay in order (`id:verdict/<chain>`, then `-v2`, then `-v3`). `inc:cause` is gone after the reject and back after the second accept, and `ograph why CHAIN` lists the events oldest first, each with the rank the chain had when judged. `similar_reports_only_the_current_verdict` checks that a rejected chain stops giving `similar` a cause. The results are in `verdicts.txt`. Nothing else moved. With the binary from before this lane and the one after, `rcaeval_score.py` gives the same top 5 for all 90 cases in both ranking modes (chain AC@1 0.93, propagation 0.83), and `similar_eval.py` gives the same output (top-1 82/89 = 0.92). The demo harvest quarantines the same 3 facts. Harvest time showed no difference beyond machine noise: four alternating merged harvests took 25.2, 29.8, 25.2 and 15.5 s with nine lanes sharing the machine.

**Evidence policy.** `[verdict] require_evidence`, `evidence_patterns` and `evidence_in_store` are off by default, and a verdict without evidence is still stored as "(none given)" (`without_a_policy_a_verdict_needs_no_evidence`). `the_evidence_policy_refuses_a_verdict_and_says_why` asserts each refusal's full message, and that a refused verdict writes nothing. A pattern that is not a regular expression fails when the workspace opens.

**Replay.** `ograph replay --set k=v` correlates every judged incident twice, under `config.toml` and under the overrides. It counts flips (verdicts that only the overrides contradict) and fixed verdicts (the reverse). The gate is `scripts/eval/replay_gate.py`. It harvests each RCAEval case into its own store, accepts its true chain (90 of 90), and then, at the configured settings and at each of the 206 propagation weight points of `rcaeval_score.py --grid`, runs both the grid's `ograph chains` and `ograph replay`. AC@1 from the replay is the share of verdicts the configured settings agree with, minus flips, plus fixed. It matched the grid's AC@1 at all 207 points, on all 90 cases, and on both splits, with 0 of 18,630 case-points differing. At the configured settings it is 0.9333, and at the shipped propagation weights `(0.4, 0.5, 0.1, 0.0)` it is 0.8333, as `rcaeval-final.txt` reports. Each replay process, which opens the store and correlates one incident twice, took a median of 84 ms on the shared machine (100 ms in an earlier run, on the binary before a label rename, with the same 207/207 result). One replay over the merged 90-verdict store from `similar_eval.py` took 0.08 s in three runs back to back. The slowest of five runs took 0.46 s. That store lists the 6 verdicts the configured settings already contradict. With `--set rank=propagation`, 9 verdicts flip and 1 is fixed, and the command exits 1.

**`rcaeval-grid.txt` is stale.** Its tuning-split numbers come from a correlator older than the "proximity runs from the end of an episode" change. For example, it gives `(0.4, 0.5, 0.1, 0.0)` 0.87, and the current `ograph chains` gives 0.77. So no replay can reproduce that file, because the current ranking does not either. The gate compares the replay with the grid recomputed by the same binary, and `replay-grid.txt` holds every point. The recomputed grid still does not generalize. Its five best tuning points (0.90 on rep 1) score 0.72 to 0.77 on the held-out reps, against 0.87 for the shipped weights.

**Not done.** Chains store `corr:settings`, and a verdict copies the chain's `corr:rank` and `corr:settings` as `rca:rank` and `rca:settings`. Replay uses them only to report how many stored chains predate the current settings. It does not reconstruct old settings from the hash. A workspace whose `.ograph/ontology.ttl` predates this lane quarantines `corr:settings` and refuses verdicts (`rca:supersededBy` is unknown to it) until the ontology is updated, as for any new term.
