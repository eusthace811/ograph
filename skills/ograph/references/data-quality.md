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

## Where a fact came from

```bash
ograph why INC-5377
```

Each fact names its graph (`g:harvest/<source>/<file>`), extractor and time. For a
model-proposed fact it adds the model, the prompt hash and the confidence. Events also carry
`evt:sourceRecord` (`alerts.json#3`, `metrics.csv:347`, `logs.jsonl@1795386000000`): open that
record in its own system for the raw detail.
