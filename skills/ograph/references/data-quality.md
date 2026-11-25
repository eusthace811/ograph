# Quarantine, degraded harvests and data quality

Every proposed fact passes a validation gateway before it is stored. Refused facts are not
lost: they go to `g:quarantine/<source>/<key>` with the reason.

## See what was refused

```bash
ograph sparql 'SELECT ?fact ?reason WHERE { GRAPH ?g { ?q a harv:Quarantined ; harv:fact ?fact ; harv:reason ?reason } }'
ograph why id:app/checkout        # quarantined proposals about one node are listed at the end
```

Common reasons and what they mean:

| Reason (as printed) | Meaning | Fix |
|---|---|---|
| `"X" is not in the ops:Application catalog; not created` | a source names an unknown service | add it to the catalog, or an alias via ownership or a runbook |
| `"X" only fuzzily matches id:app/Y (0.88); not created` | a misspelling too far from any name to trust | use the exact name or add an alias |
| `"X" is close to id:app/Y (0.98) but names a sibling or ties another entity; not created` | a different variant such as `-uk` vs `-eu` or `-canary`, or two equally close names | use the exact name or add the variant to the catalog |
| `alias "X" already names id:app/Y` | two services claim one name | fix the alias in the source |
| `"P1" is not in the closed vocabulary of evt:severity` | a value outside a closed list | map the value in the exporter, or extend the ontology |
| `SHACL MinCountConstraintComponent: an event needs a time` | a record without a usable time | fix the exporter's time field |
| `SHACL DatatypeConstraintComponent: "yesterday" is not a well-formed ...dateTime` | a value of the wrong type | fix the exporter |
| `unconfirmed: no catalog or traces edge backs this model-proposed ops:dependsOn` | a model guessed an edge nothing confirms | declare the edge in the catalog if it is real |
| `model confidence 0.20 is below 0.5` | a model was unsure | nothing; it is kept out on purpose |

## Degraded harvests

A batch line ending in `DEGRADED: <why>` kept the facts it had. Causes: a file that cannot be
read or parsed, an empty file, a directory that cannot be listed, or a directory that held files
and is now empty (an unmounted share). Fix the source and harvest again.

## Is the graph current

```bash
ograph status          # nothing when nothing is due
ograph validate        # SHACL audit; also the focus nodes each shape selects
```

- `status` prints one line when source files or settings changed since the last full harvest,
  and lists incidents with chains but no verdict. `--json` gives `harvested_at`, `changed` and
  `unjudged`. Exit 0 when something is due. `./x install-hook` in the ograph repository makes
  it a Claude Code `SessionStart` hook.
- `validate` prints the focus nodes each `shapes.ttl` shape selects and warns about a shape that
  selects none: misspelled, or waiting for data that never came. It exits 1 only on violations.

## Where a fact came from

```bash
ograph why INC-5377
```

Each fact names its graph (`g:harvest/<source>/<file>`), extractor and time. For a
model-proposed fact it adds the model, the prompt hash and the confidence. Events also carry
`evt:sourceRecord` (`alerts.json#3`, `metrics.csv:347`, `logs.jsonl@1795386000000`): open that
record in its own system for the raw detail.

## What a wrong source puts in doubt

When a log export, an incident file or a runbook turns out to be wrong, ask what rests on it:

```bash
ograph uses logs/2026-11-22.jsonl          # a file, or a directory of them
ograph uses incidents.json#4               # one record, as evt:sourceRecord names it
```

It lists the file's facts (`also_in` names other sources that state the same fact, so those
stay), the events and incidents only it states, facts of other files that would be quarantined
without it (`because`), the chains that would go, be relinked, move or enter (with incident
and rank), the current verdicts on chains that go or are relinked, and its quarantined
proposals. Re-open each listed verdict before you rely on it. For a `catalog`, `ownership` or
`docs` file the answer says `naming`: names in every other file resolve against it, so more
can change than it lists. To take a file out, delete it and remove its `[[source]]` entry if
the entry names it; ograph keeps the facts of a configured path it cannot read.

