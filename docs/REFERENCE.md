# ograph reference

## Commands

Every command takes `--root DIR` (the directory holding `.ograph/`, found by searching upward when omitted) and `--json`. Exit codes follow ogrep: 0 when something was found, 1 when nothing was, 2 on error.

| Command | What it does |
|---|---|
| `ograph init` | Creates `.ograph/` with `config.toml`, `ontology.ttl` and `shapes.ttl` |
| `ograph harvest [--only KIND]` | Runs the enrichers in layer order. Each source file becomes one named graph, then correlation runs for every incident |
| `ograph before INC [--window 10m] [--depth 4]` | Events before an incident on its application and the applications it depends on (question 1) |
| `ograph chains INC [--rank chain\|propagation] [--window 30m] [--top 5] [--set k=v]` | Correlation chains and ranked root-cause candidates |
| `ograph blast APP [--depth 8]` | Dependents by distance, the business processes they support, and open incidents (question 2) |
| `ograph similar INC [--top 5]` | Past incidents with the same signature, with their accepted causes and mitigations (question 3) |
| `ograph news [--since 7d]` | News that mentions a vendor our applications use, with the affected applications and processes (question 4) |
| `ograph why NODE` | Provenance of every fact about a node: the harvest graph, source, extractor and time, model provenance, and quarantined proposals |
| `ograph verdict CHAIN accept\|reject --evidence REF --by WHO` | Records a human verdict. An accepted chain also writes `inc:cause` |
| `ograph sparql 'QUERY' [--limit N]` | Runs SPARQL 1.1. The prefixes are predeclared, and the default graph is the deduplicated union of all harvests. A class or property in an ograph namespace that the ontology does not declare is an error that names the nearest declared one |
| `ograph validate` | Runs a full SHACL audit of the store |
| `ograph export [--out FILE]` | Writes every named graph as TriG |
| `ograph ontology check` / `ograph ontology diff OTHER.ttl` | Checks drift between the code and the ontology, or lists the changes that would break stored data |
| `ograph ontology coverage QUESTIONS [--out FILE]` | Needs a model. For each question in the file (one per line), it reports the declared names that represent it and the concepts nothing declared represents. Gaps go to `.ograph/proposed.ttl` as Turtle for a person to review, merge into `ontology.ttl`, then check with `ontology diff`. Exits 1 when any question has a gap |
| `ograph lab [--out FILE]` | Writes a self-contained HTML page of incidents, causes, timelines and similar incidents |
| `ograph stats`, `ograph compact` | Shows size per table, or rewrites the store file without free pages |
| `ograph serve [--addr A]` / `ograph serve --mcp` | Serves line-delimited JSON-RPC over TCP, with the address in `.ograph/serve.json` and sources polled and re-harvested on change. With `--mcp`, serves MCP over stdio |

A node name may be any of these:

- an IRI
- a prefixed name such as `id:inc/inc-5377`
- an incident id such as `INC-5377`
- an application name

## Configuration (`.ograph/config.toml`)

```toml
[[source]]
kind = "catalog"            # one table per input, any order
path = "repo"

[detect]
metric_z = 5.0              # robust z threshold for metric episodes
metric_baseline_s = 300     # trailing baseline window
metric_min_run = 3          # consecutive anomalous points that make an episode
metric_min_history_s = 150  # seconds of history before the detector judges a point (no warm-up episodes)
log_burst_factor = 3.0      # error-template count vs. the 5-minute trailing mean
log_min_count = 3
trace_error_rate = 0.05
trace_slow_factor = 2.0     # p95 vs. the trailing median p95
two_sided = ["workload", "throughput", "requests"]  # metrics where a drop also counts

[correlation]
window_s = 1800             # 30 minutes (Netflix uses 10; see docs/EVIDENCE.md)
after_s = 0
min_confidence = 0.6        # Netflix: chains under 0.6 are discarded
w_time = 0.6                # Netflix: time_proximity * 0.6 + severity * 0.4
w_severity = 0.4
known_pair_prior = 0.9      # Netflix: known causal pairs carry a 0.9 prior
rank = "chain"              # or "propagation"
prop_weights = [0.4, 0.5, 0.1, 0.0]  # propagation: strength, chain, spread, onset
signature_window_s = 900    # the window `similar` compares
depth = 4                   # dependency hops in scope

[llm.extract]               # optional; without it, discussions get regex facts only
command = ["pi", "-p", "--no-tools", "--no-session", "--no-extensions", "--no-skills",
           "--no-context-files", "--offline", "--thinking", "off", "--model", "openai/gpt-6-luna"]
model = "pi:openai/gpt-6-luna"
parallel = 4
timeout_s = 90
```

## Input formats

Times may be any of these:

- Unix seconds or milliseconds, as a number or a numeric string
- ISO 8601 with any fraction and offset; no offset means UTC

JSON inputs may be a JSON array, one object, or JSON lines.

| `kind` | Format | Facts it proposes |
|---|---|---|
| `catalog` | A git repository. ograph runs `ogrep --entities . --json` in it, which reads Backstage, Compose, Kubernetes and Prometheus rules. Set `OGRAPH_OGREP` to point at another binary | `ops:Application`, `ops:name`, `ops:dependsOn` (declared or inferred) |
| `ownership` | `{"app", "process"}`, `{"app", "vendor"}`, `{"app", "alias"}` | `ops:supports`, `ops:usesVendor`, `ops:alias` |
| `docs` | Runbooks and other prose, Markdown or text | `ops:alias`, only from a phrase that states one: `A, also known as B`, `A aka B`, `A (aka B)`, `A, formerly B`, `A is also called B`, or `A (b-c)` where the parenthesis holds a kebab or snake identifier that does not end in a number |
| `deployments` | `{"service", "version", "at"}` | `evt:Deployment` |
| `config_changes` | `{"service", "key", "at"}` | `evt:ConfigChange` |
| `alerts` | Alertmanager webhook (`{"alerts": [{"labels": {...}, "startsAt"}]}`) or one alert per line | `evt:Alert` with mapped severity |
| `incidents` | `{"id", "title", "service", "opened", "severity", "status", "resolved", "mitigation", "fault"}` | `inc:Incident` |
| `logs` | `{"ts", "service", "message", "level"?}`, or plain text with leading timestamps (the service is then the file name) | `evt:LogError` per error-template burst. Raw lines stay in the file |
| `metrics` | CSV with a `time` column and `<service>_<metric>` columns | `evt:MetricAnomaly` per robust-z episode. Points stay in the file |
| `traces` | `{"trace_id", "span_id", "parent_span_id", "service", "start_ms", "duration_us", "status"}`. The RCAEval and Jaeger field names also work | `evt:TraceFailed` and `evt:TraceSlow` per minute, plus `ops:dependsOn` inferred from parent spans |
| `discussions` | A Slack export: `[{"ts", "thread_ts"?, "channel", "text"}]` | `disc:Thread`, with incident references by regex. With a model, it also proposes `disc:category`, `disc:sentiment`, `disc:about` and `ops:dependsOn`. A category needs a quote from the thread that states the cause, or it becomes UNKNOWN. A dependency needs a quote that names both applications |
| `news` | `{"at", "title", "summary", "mentions": [names]}` | `evt:NewsItem` and `evt:mentions` |

Every event carries its evidence: `file#record`, `file:line` or `file@ms`. `ograph why` shows it.

## The gateway

Enrichers never write to the store. A batch goes through these checks in order:

1. **IRI scheme.** IRIs must have the form `https://ograph.dev/id/<kind>/<slug>`.
2. **Catalog.** Applications and teams must resolve against the catalog:
   - an exact name or alias is accepted;
   - a fuzzy match at Jaro-Winkler 0.93 or above is accepted and counted (tied scores go to the first IRI in order), unless another entity scores within 0.03 of it or the name is a sibling: the same words but one, and that word unlike its counterpart (`inventory-service-uk` against `inventory-service-eu`, `v1` against `v2`). Those are quarantined as ambiguous;
   - a match from 0.80 to 0.93 is quarantined as ambiguous;
   - anything lower is quarantined as missing.
3. **Catalog-only types.** Only the catalog may create an application.
4. **Ontology membership.** Only ontology properties and classes are allowed.
5. **Closed vocabularies.** Values must come from the vocabularies declared in `og:vocabulary`.
6. **Model confidence.** A model's confidence must be at least 0.5. The `Llm` variant cannot exist without one.
7. **Model-proposed structure.** A model may propose `ops:dependsOn` only when the catalog or traces already hold the same edge. Otherwise the edge is quarantined as unconfirmed.
8. **Aliases.** An alias that already names a different application is quarantined.
9. **Evidence.** Each fact needs evidence that names a record this harvest read.
10. **Incremental SHACL.** Shapes run on the nodes the batch touches, seen as the store will be after the commit. They are the derived domain, range, datatype, vocabulary and functional shapes, plus `shapes.ttl`. A violation that no single fact caused, such as a missing required value, rejects the node. The checks repeat until no violation is left.

Rejected facts go to `g:quarantine/<source>/<key>` with their reason. Accepted facts replace `g:harvest/<source>/<key>`, and only the difference is written, so an unchanged re-harvest writes one quad: the harvest time.
