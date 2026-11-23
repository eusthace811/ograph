# SPARQL over the graph

`ograph sparql 'QUERY' [--limit N]` runs read-only SPARQL 1.1. The default graph is the
deduplicated union of every harvest; `GRAPH ?g { }` reaches the named graphs. Updates are refused.

## Prefixes (predeclared)

| Prefix | For |
|---|---|
| `ops:` | applications, teams, regions, business processes, vendors, dependencies |
| `evt:` | events: deployments, config changes, alerts, log errors, metric anomalies, traces, news |
| `inc:` | incidents |
| `disc:` | chat threads |
| `corr:` | correlation chains |
| `rca:` | verdicts |
| `harv:` | harvest provenance and quarantine |
| `id:` | data nodes: `id:app/…`, `id:inc/…`, `id:ev/…`, `id:chain/…`, `id:process/…`, `id:vendor/…` |
| `g:` | named graphs: `g:harvest/<source>/<key>`, `g:quarantine/<source>/<key>` |
| `rdf:` `rdfs:` `xsd:` `owl:` `sh:` | the standard ones |

## Classes

- `ops:Application`, `ops:Team`, `ops:Region`, `ops:BusinessProcess`, `ops:Vendor`
- Events (each node has only its most specific type): `evt:Deployment`, `evt:ConfigChange`,
  `evt:Alert`, `evt:LogError`, `evt:MetricAnomaly`, `evt:TraceFailed`, `evt:TraceSlow`,
  `evt:Experiment`, `evt:E2EFailure`, `evt:NewsItem`, `inc:Incident`
- `disc:Thread`, `corr:Chain`, `rca:Verdict`, `harv:Harvest`, `harv:Quarantined`, `harv:LlmFact`

## Properties you will use

| Property | From → to |
|---|---|
| `ops:name`, `ops:alias` | anything → name |
| `ops:dependsOn` | application → application |
| `ops:supports`, `ops:usesVendor`, `ops:ownedBy`, `ops:inRegion`, `ops:tier` | application → process, vendor, team, region, tier |
| `evt:on`, `evt:at`, `evt:until` | event → application, start time, end time |
| `evt:signal`, `evt:severity`, `evt:value`, `evt:count` | event → kind, severity, value, count |
| `evt:version`, `evt:template`, `evt:metric`, `evt:summary`, `evt:sourceRecord` | event details and the raw record |
| `evt:mentions` | news item → vendor |
| `inc:id`, `inc:title`, `inc:status`, `inc:resolvedAt`, `inc:mitigation`, `inc:cause`, `inc:faultType` | incident fields |
| `disc:about`, `disc:category`, `disc:sentiment`, `disc:channel`, `disc:at` | thread fields |
| `corr:leadsTo`, `corr:rootCause`, `corr:link`, `corr:candidateApp`, `corr:confidence`, `corr:rank` | chain fields |
| `rca:onChain`, `rca:state`, `rca:by`, `rca:evidence`, `rca:at`, `rca:rootOn`, `rca:rootAt` | verdict fields |

Closed values: `inc:status` open, mitigated, resolved. `rca:state` accepted, rejected.
`ops:tier` critical, high, medium, low. `.ograph/ontology.ttl` is the source of truth.

## Recipes

```bash
# everything that depends on ledger-db, directly or not
ograph sparql 'SELECT ?app WHERE { ?a ops:dependsOn+ id:app\/ledger-db ; ops:name ?app }'

# deployments since a date, newest first
ograph sparql 'SELECT ?app ?v ?at WHERE { ?d a evt:Deployment ; evt:on/ops:name ?app ; evt:version ?v ; evt:at ?at
  FILTER(?at > "2026-11-22T00:00:00Z"^^xsd:dateTime) } ORDER BY DESC(?at)'

# count events of every kind (list the subclasses, see pitfalls)
ograph sparql 'SELECT ?type (COUNT(?e) AS ?n) WHERE {
  VALUES ?type { evt:Alert evt:Deployment evt:ConfigChange evt:LogError evt:MetricAnomaly evt:TraceFailed evt:TraceSlow evt:Experiment evt:E2EFailure }
  ?e a ?type } GROUP BY ?type ORDER BY DESC(?n)'

# incidents and their status
ograph sparql 'SELECT ?id ?app ?status WHERE { ?i a inc:Incident ; inc:id ?id ; evt:on/ops:name ?app OPTIONAL { ?i inc:status ?status } }'

# stored chains of one incident, by rank
ograph sparql 'SELECT ?rank ?app ?conf WHERE { ?c corr:leadsTo/inc:id "INC-5377" ; corr:rank ?rank ;
  corr:candidateApp/ops:name ?app ; corr:confidence ?conf } ORDER BY ?rank'

# which vendors each application uses
ograph sparql 'SELECT ?app ?vendor WHERE { ?a ops:usesVendor ?v ; ops:name ?app . ?v ops:name ?vendor }'

# chat threads, their category and the incident they discuss
ograph sparql 'SELECT ?t ?cat ?inc WHERE { ?t a disc:Thread ; disc:category ?cat OPTIONAL { ?t disc:about/inc:id ?inc } }'

# what each named graph holds
ograph sparql 'SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY DESC(?n)'

# everything about one node
ograph sparql 'SELECT ?p ?o WHERE { <https://ograph.dev/id/app/ledger-db> ?p ?o }'
```

## Pitfalls

- **`/` and dots in a prefixed name.** Write `id:app\/ledger-db` or the full IRI
  `<https://ograph.dev/id/app/ledger-db>`. The ids that commands print (`id:app/ledger-db`)
  need that escape inside a query, and a deployment id's version dots too
  (`id:ev\/deploy-payments-api-2\.3\.1-1795386160000`).
- **Superclasses match nothing.** Only the most specific type is stored, so
  `?e a evt:Event` is rejected with the list of subclasses to use in `VALUES`.
- **Misspelled names are errors, not empty results.** `evt:onn` fails with
  "did you mean evt:on?". Fix the name; do not conclude the data is missing.
- **Times are `xsd:dateTime` in UTC.** Compare with `"…Z"^^xsd:dateTime`.
- **Exit 1 means no rows.** The query ran; nothing matched.
- **`--limit N`** caps the rows; use it on exploratory queries over large stores.
- **`--json`** gives `{"kind":"solutions","variables":["id"],"rows":[["INC-4120"]]}`.
