# Verdicts

A verdict is a person's decision on one chain. It is the only way a cause becomes a fact
(`inc:cause`), and it is what later `similar` answers report as the cause.

## Record one

Only after a person confirmed it. Pass what they based it on and who they are.

```bash
ograph chains INC-5377                       # note the chain id of the confirmed application
ograph verdict id:chain/inc-5377-a7370ff7d4358525 accept --evidence PM-5377 --by "jane (oncall)"
ograph verdict id:chain/inc-5377-f1ece3ced5be33c7 reject --evidence "rollback changed nothing" --by "jane (oncall)"
```

- `--evidence` may repeat: a postmortem id, a ticket, a dashboard link.
- `--by` must not be blank.
- `id:chain/inc-5377-1` also works and means "the chain ranked 1 right now". Prefer the hash id:
  a re-harvest between reading and judging can change ranks.
- Judging the same chain again adds a verdict and keeps the earlier ones. The earlier one gets
  `rca:supersededBy` the new one; only the newest counts for `inc:cause`, `similar` and `replay`.
  `ograph why id:chain/...` lists the history, oldest first.
- A workspace can require evidence in `.ograph/config.toml` under `[verdict]`
  (`require_evidence`, `evidence_patterns`, `evidence_in_store`). A refused verdict says which
  rule refused it.
- A verdict outlives the telemetry: it stores the root event's class, application, time and
  signal, so the cause still shows after the raw alert has aged out of the sources.

## Read them

```bash
ograph sparql 'SELECT ?id ?state ?by ?cause ?at WHERE {
  ?v a rca:Verdict ; rca:state ?state ; rca:by ?by ; rca:at ?at ; rca:onChain ?c .
  FILTER NOT EXISTS { ?v rca:supersededBy ?later }
  ?c corr:leadsTo/inc:id ?id ; corr:candidateApp/ops:name ?cause }'
ograph sparql 'SELECT ?id ?app WHERE { ?i inc:id ?id ; inc:cause ?e . ?v rca:onChain [ corr:rootCause ?e ] ; rca:rootOn/ops:name ?app }'
```

## Before changing correlation settings

```bash
ograph replay --set window_s=600            # what the change would do to past verdicts
```

It correlates every judged incident again and lists accepted chains whose application loses
rank 1 or drops out, and rejected chains that rise to rank 1. Exit 1 means some verdict would
flip: show the list to the person before changing `config.toml`.

## Errors

- `no chain id:chain/...; ograph chains INCIDENT lists them`: the id is wrong or the chain no
  longer exists. Run `ograph chains INC` and use a printed id.
- `the root of ... has no telemetry left and no earlier verdict`: the chain is stale. Run
  `ograph harvest` and judge a current chain.
- `verdict not recorded: [verdict] ...`: the workspace's evidence policy refused it; the message
  names the rule. Ask the person for the evidence it wants.
- `verdict not recorded: the ontology refuses N of its facts`: the workspace ontology is older
  than the binary. Run `ograph ontology check`, then update `.ograph/ontology.ttl`.
