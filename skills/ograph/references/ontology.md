# Changing the ontology safely

The ontology (`.ograph/ontology.ttl`) declares every class, property and closed vocabulary the
graph may hold; `.ograph/shapes.ttl` adds SHACL constraints. The gateway refuses anything else,
so changing them changes what can be stored.

## Check

```bash
ograph ontology check
```

```
ontology v1.2: 24 classes, 62 properties, 8 closed vocabularies, 136 shapes; 0 problems, 0 warnings
```

It reports drift between the code and the file, such as a property the code writes that the
ontology does not declare. Run it after upgrading ograph: an older `ontology.ttl` can refuse
facts a newer binary writes (verdicts fail with "the ontology refuses N of its facts").

Version 1.1 added the code terms: the classes `evt:Commit` and `evt:Exception`, the properties
`evt:revision`, `evt:changedFile` and `evt:codeLocation`, and the signals `COMMIT` and
`EXCEPTION`. A workspace created before it needs the new `ontology.ttl` and `shapes.ttl` before
a `code` or `errors` source is added, or their facts are quarantined.

## Diff before you install a change

```bash
cp .ograph/ontology.ttl new.ttl
# edit new.ttl
ograph ontology diff new.ttl
```

Lines starting with `BREAKING` would invalidate stored data or start refusing current inputs:
a removed class or property, a changed domain or range, a property made functional, a removed
vocabulary value, or a vocabulary added to a free property (`BREAKING vocabulary closed`).
Exit code 1 when anything breaks. `added ...` and `vocabulary opened` lines are safe.

Install by copying over `.ograph/ontology.ttl`; the next `ograph serve` poll or
`ograph harvest` picks it up. Then `ograph validate`.

## Find gaps with real questions

Needs a model (`[llm.extract]` configured).

```bash
printf 'Which deployments touched services that page the payments team?\nWhich region was the failing pod in?\n' > questions.txt
ograph ontology coverage questions.txt
```

For each question it lists the declared names that can answer it and the concepts nothing
declares. Proposed additions go to `.ograph/proposed.ttl` for a person to review, merge, and
check with `ontology diff`. Exit code 1 when any question has a gap.
