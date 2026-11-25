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
ontology does not declare, or a class `shapes.ttl` targets that nothing declares. Run it after
upgrading ograph: an older `ontology.ttl` can refuse facts a newer binary writes (verdicts fail
with "the ontology refuses N of its facts"). Lines starting with `warning:` never change the
exit code. They name unfinished terms: no domain or range, no comment or still `Proposed:`, no
code writes them, the store has none, or an event class no shape requires a time of.

Version 1.2 removed `evt:Experiment`, `evt:E2EFailure` and the signals `EXPERIMENT`,
`E2E_FAILURE`, `DISCUSSION` and `DEPENDENCY_CHANGE`, which nothing wrote.

Version 1.1 added the code terms: the classes `evt:Commit` and `evt:Exception`, the properties
`evt:revision`, `evt:changedFile` and `evt:codeLocation`, and the signals `COMMIT` and
`EXCEPTION`. A workspace created before it needs the new `ontology.ttl` and `shapes.ttl` before
a `code` or `errors` source is added, or their facts are quarantined.

## Diff before you install a change

```bash
cp .ograph/ontology.ttl new.ttl; cp .ograph/shapes.ttl new-shapes.ttl
# edit either
ograph ontology diff new.ttl --shapes new-shapes.ttl
```

Lines starting with `BREAKING` remove or narrow something stored data uses, with the count of
uses: a removed class, property or vocabulary value, a closed vocabulary, a lost superclass, a
property made functional, or a domain or range that is not a superclass of the old one. The
same change to a term the store never uses ends in `(unused)` and is safe. Then, per harvest
graph, `N facts in g:harvest/... would be quarantined` with the reasons: what a re-harvest under
the new files would refuse. Exit code 1 when anything breaks or would be quarantined.

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
check with `ontology diff`. To decline a proposal for good, add it to `.ograph/declined.ttl`:

```turtle
@prefix og: <https://ograph.dev/meta#> .
@prefix ops: <https://ograph.dev/ops#> .
ops:KubernetesCluster og:declined "We model services, not clusters." ;
  og:by "dana" ; og:question "Which Kubernetes cluster runs payments-api?" .
```

A declined gap is reported as `declined by dana` and never proposed again. Exit code 1 when any
question has a gap nobody has judged.
