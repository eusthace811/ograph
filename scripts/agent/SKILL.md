---
name: ograph
description: Investigate an incident with ograph, the observability knowledge graph. Use when asked why an incident happened, what changed before it, what a failure affects, whether it happened before, or what news touches our vendors, in a directory with .ograph/.
---

# Investigating with ograph

ograph holds harvested facts (catalog, dependencies, deployments, config changes, alerts,
incidents, log and metric anomalies, slow and failed traces, chat threads, news) and pointers
back to the raw records. Raw logs, metrics and traces stay in their own systems.

Work in this order and cite what each step returned:

1. `ograph_before {incident}`: the events before the incident on its application and its
   dependencies, newest first. Read the signal types and strengths.
2. `ograph_chains {incident}`: ranked root-cause candidates, each with its chain of evidence.
   Rank 1 is the answer to report. A chain is a hypothesis: say so in the answer, and write
   "the strongest candidate is" with its score and chain id, never "the cause is". Name another application only when you
   can quote evidence the ranking does not use: a chat message or operator action, a deployment
   or config change no ranked chain contains, a past write-up, a verdict (`precedent`, or
   `causes` in `ograph_similar`). Being earliest, most severe or upstream is not such evidence:
   the ranking already weighed it. When you depart, quote that evidence and keep rank 1 in the
   answer as the alternative. To say how sure rank 1 is, read its `weakest` link and each
   link's `time`, `severity` and `edge` (in `scores`).
3. `ograph_similar {incident}`: past incidents with the same pattern, their accepted causes and
   the mitigations that worked. Prefer a mitigation that worked before.
4. `ograph_blast {app}` for the suspected cause: who else is affected, and which business
   processes.
5. `ograph_why {node}` before you rely on a fact: which harvest asserted it, from which source
   record, and whether a model proposed it (with its confidence).
6. Drill into the raw record named in `source_record` in its own system when you need detail.
7. `ograph_sparql` for anything the verbs do not answer. Each node stores only its most
   specific type, so match `evt:Event` through its subclasses
   (`VALUES ?type { evt:Alert evt:Deployment ... } ?e a ?type`); the error for
   `?e a evt:Event` lists them.

8. `ograph_resolve {mentions}` before you name an application no tool answer showed you.
   `ambiguous` and `unresolved` names are not applications.
9. `ograph_cite_check {text}` on your draft answer. Fix or drop every `stale` or `unknown`
   citation before you answer.

Only call `ograph_verdict` after a person confirms the cause, and pass their evidence.
Never present a chain as a conclusion without that verdict.
