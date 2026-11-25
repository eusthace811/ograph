# ograph reference

## Commands

Every command takes `--root DIR` (the directory holding `.ograph/`, found by searching upward when omitted) and `--json`, which `serve` ignores because its output is the protocol. Exit codes follow ogrep: 0 when something was found, 1 when nothing was, 2 on error. Commands can run side by side on one store: `serve`, read commands and a harvest share `.ograph/store.redb`, and harvests and verdicts take turns through `.ograph/store.lock`.

| Command | What it does |
|---|---|
| `ograph init` | Creates `.ograph/` with `config.toml`, `ontology.ttl` and `shapes.ttl` |
| `ograph harvest [--only KIND]` | Runs the enrichers in layer order. Each source file becomes one named graph, except incidents, which all share one, then correlation runs for every incident. KIND is a kind from `config.toml` such as `config_changes`, its graph name such as `config`, or `correlation` |
| `ograph before INC [--window 10m] [--depth 4]` | Events before an incident on its application and the applications it depends on (question 1). An episode that began up to 24 hours before the window and is still under way when it opens counts too |
| `ograph chains INC [--rank chain\|propagation] [--window 30m] [--top 5] [--set k=v]` | Ranked root-cause applications, each with its best chain. A chain's id, such as `id:chain/inc-5377-e1fef786a90c3b12`, comes from its incident and root event, so it names the same hypothesis after any re-correlation. The number printed before an application is its chain's `corr:rank` after a harvest with the configured settings; a harvest stores the chains of the first `max_chains` applications. When no application ranks and a `code` source is configured, the answer also carries `suspects`, the list `ograph suspects INC` gives, and the exit code is 0 when that list is not empty |
| `ograph code INC [--top 5]` | Where an incident points in the code. `locations` are the lines behind its candidate events: the log bursts and exceptions that carry `evt:codeLocation`, newest first. `commits` are the candidate commits. Those that changed a located file come first, and `touches` lists those lines. `deployments` are the candidate deployments that carry changed files, listed the same way. A located log message also gets `introduced`, the commit that first added it to the code among the commits made until the incident, with `days_before`. That lookup needs `ogrep index --history` in the repository, and `degraded` says so when the index is missing. `files` are the files the incident's description is about, and `writeups` are the past incident write-ups that share its words. ogrep ranks both in every `code` repository, and `--top` limits them. The description is the incident title, then the applications and messages along the top-ranked chain. Candidate events come from the configured correlation window. A repository ogrep cannot answer for is named in `degraded`. Without a `code` source `files` and `writeups` are empty |
| `ograph suspects INC\|APP[,APP...] [--since WHEN]` | Likely causes from the dependency graph and recent commits, with no telemetry. It runs `ogrep --suspects` in each `catalog` and `code` repository and uses the first that answers. For an incident, the failing applications are its own and those of the other incidents opened within the correlation window of it, and commits count from the day before it until it opened. ogrep 0.2.0 cannot stop at a time, so with it later commits count too and a note says so. For application names, commits count from 24 hours ago. `--since` takes what ogrep takes: `24h`, `3d` or a date such as `2026-11-22`. A repository ogrep cannot answer for is reported on standard error |
| `ograph blast APP [--depth 8]` | Dependents by distance, the business processes they support, and open incidents (question 2) |
| `ograph similar INC [--top 5]` | Past incidents with the same signature, with their accepted causes and mitigations (question 3) |
| `ograph news [--since DURATION]` | News that mentions a vendor our applications use (all of it by default, or only the last DURATION, such as `7d`), with the affected applications and processes (question 4) |
| `ograph why NODE` | Provenance of every fact about a node: the harvest graph, source, extractor and time, model provenance, and quarantined proposals |
| `ograph verdict CHAIN accept\|reject --evidence REF --by WHO` | Records a human verdict on a chain id, or on `id:chain/<incident>-<rank>` for the chain at that rank now. Any other name that is not a chain is an error. The verdict keeps the chain's incident and root event, and stores the root's class, application, time and signal on the verdict node (`rca:rootClass`, `rca:rootOn`, `rca:rootAt`, `rca:rootSignal`), so it means the same after re-correlation or after the root's telemetry leaves the sources. A root whose telemetry is gone is no longer a candidate for correlation or `before`. An accepted chain also writes `inc:cause` |
| `ograph sparql 'QUERY' [--limit N]` | Runs SPARQL 1.1. The prefixes are predeclared, and the default graph is the deduplicated union of all harvests. A class or property in an ograph namespace that the ontology does not declare is an error that names the nearest declared one. Each node stores only its most specific type, so a class with subclasses, such as `evt:Event`, is an error that lists them |
| `ograph validate` | Runs a full SHACL audit of the store |
| `ograph export [--out FILE]` | Writes every named graph as TriG |
| `ograph ontology check` / `ograph ontology diff OTHER.ttl` | Checks drift between the code and the ontology, or lists the changes that would break stored data |
| `ograph ontology coverage QUESTIONS [--out FILE]` | Needs a model. For each question in the file (one per line), it reports the declared names that represent it and the concepts nothing declared represents. Gaps go to `.ograph/proposed.ttl` as Turtle for a person to review, merge into `ontology.ttl`, then check with `ontology diff`. Exits 1 when any question has a gap |
| `ograph lab [--out FILE]` | Writes one self-contained HTML page (no server, no external files, light and dark themes, readable at phone width). Per incident: the ranked causes with their chains; a timeline with one lane per application in scope, nearest dependency first, each event a bar from its start to its end, the incident as the zero line and the top chain as arrows; the dependency neighbourhood (dependencies up to the correlation depth, and the callers of the top cause) with the applications that had events filled and the top chain outlined; the evidence of every event in the top five chains, as `ograph why` gives it, with the chain's verdict and an `ograph verdict` command to copy; the events table and similar past incidents. The SVG is drawn when the page is written, so it reads with scripts off; the script only opens evidence, filters the timeline to an application clicked in the neighbourhood, and copies the command |
| `ograph stats`, `ograph compact` | Shows size per table, or rewrites the store file without free pages |
| `ograph serve [--addr A]` / `ograph serve --mcp` | Serves line-delimited JSON-RPC over TCP, with the address in `.ograph/serve.json`. Every 5 s it compares the path, modification time and size of each file a harvest reads (for a catalog, the YAML and `.env` files `ogrep --files` lists, so ignore rules apply; for a `code` repository, only `.git/logs/HEAD`, which changes with every commit, pull and checkout), plus `config.toml`, `ontology.ttl` and `shapes.ttl`, and re-harvests on any addition, edit or deletion, retrying a failed harvest at the next poll. With `--mcp`, serves MCP over stdio without polling |

A node name may be any of these:

- an IRI
- a prefixed name such as `id:inc/inc-5377`
- an incident id such as `INC-5377`
- an application name or alias

## Configuration (`.ograph/config.toml`)

A key or table that ograph does not know, such as `[corelation]` or `windw_s`, is an error.

```toml
[[source]]
kind = "catalog"            # one table per input, any order
path = "repo"

[detect]
metric_z = 5.0              # robust z threshold for metric episodes
metric_baseline_s = 300     # trailing baseline window, widened for coarse series to hold 12 points
metric_min_run = 3          # consecutive anomalous points that make an episode
metric_min_history_s = 150  # seconds of history before the detector judges a point (no warm-up episodes)
log_burst_factor = 3.0      # error-template count vs. the 5-minute trailing mean
log_min_count = 3
log_new_after_s = 600       # a message absent from this much of a service's log is new at its first error line; 0 turns it off
trace_error_rate = 0.05
trace_slow_factor = 2.0     # p95 vs. the trailing median p95
two_sided = ["workload", "throughput", "requests"]  # metrics where a drop also counts

[correlation]
window_s = 1800             # 30 minutes (Netflix uses 10; see docs/EVIDENCE.md)
after_s = 0                 # seconds after the incident still searched; such an event closes its chain at the incident
min_confidence = 0.6        # Netflix: chains under 0.6 are discarded
w_time = 0.6                # Netflix: time_proximity * 0.6 + severity * 0.4
w_severity = 0.4
known_pair_prior = 0.9      # Netflix: known causal pairs carry a 0.9 prior
rank = "chain"              # or "propagation"
prop_weights = [0.4, 0.5, 0.1, 0.0]  # propagation: strength, chain, spread, onset
signature_window_s = 900    # the window `similar` compares
depth = 4                   # dependency hops in scope
max_chains = 10             # ranked applications whose chains a harvest stores

[llm.extract]               # optional; without it, discussions get regex facts only
command = ["pi", "-p", "--no-tools", "--no-session", "--no-extensions", "--no-skills",
           "--no-context-files", "--offline", "--thinking", "off", "--model", "openai/gpt-6-luna"]
model = "pi:openai/gpt-6-luna"
parallel = 4
timeout_s = 90
```

## Input formats

Times may be any of these:

- Unix seconds, milliseconds, microseconds or nanoseconds, as a number or a numeric string. The magnitude picks the unit: below 10^10 is seconds, below 10^13 milliseconds, below 10^16 microseconds, and anything larger nanoseconds
- ISO 8601 with any fraction and offset (`Z`, `+05:30`, `+0530`, `+05` or a trailing ` UTC`); no offset means UTC

A time must fall between 1971-01-01 and 2200-01-01. ograph rejects any other value, such as `nan`, `inf`, a bare year (`2026`) or a compact date (`20261122`), and treats the record as having no time.

JSON inputs may be a JSON array, one object, or JSON lines.

| `kind` | Format | Facts it proposes |
|---|---|---|
| `catalog` | Required. A git repository. ograph runs `ogrep --entities . --json` in it, which reads Backstage, Compose, Kubernetes and Prometheus rules, so `ogrep` must be on `PATH`; set `OGRAPH_OGREP` to point at another binary. Only the catalog creates applications, so without it every fact about a service is quarantined. Optional `inferred` (default `true`) also takes the calls ogrep infers from configuration, such as an environment variable that holds another service's address. Kubernetes manifests declare no dependencies, so there the inferred calls are the whole graph. Set `inferred = false` to keep only what a file declares | `ops:Application`, `ops:name`, `ops:dependsOn` (declared, or inferred from a call), `ops:alias` for a Kubernetes Service that fronts a workload with another name (ogrep 0.3 or newer) |
| `ownership` | `{"app", "process"}`, `{"app", "vendor"}`, `{"app", "alias"}` | `ops:supports`, `ops:usesVendor`, `ops:alias` |
| `docs` | Runbooks and other prose, Markdown or text | `ops:alias`, only from a phrase that states one: `A, also known as B`, `A aka B`, `A (aka B)`, `A, formerly B`, `A is also called B`, or `A (b-c)` where the parenthesis holds a kebab or snake identifier that does not end in a number and shares a part other than `api`, `svc` or a similar word with `A` |
| `deployments` | `{"service", "version", "at"}`. `tag` and `commit` also work for the version | `evt:Deployment`. With a `code` source, a deployment whose version and the version the service ran before it are revisions of the repository (a tag or a commit id) also gets up to 200 `evt:changedFile`: the files of the commits to the service between the two. This needs ogrep 0.3 or newer; otherwise the harvest line says `DEGRADED` and the deployment keeps no files |
| `config_changes` | `{"service", "key", "at"}` | `evt:ConfigChange` |
| `code` | A git repository that holds the services' code and their operations files. It may be the catalog's repository. Optional `since` (default `"30d"`) is passed to `ogrep --since` as written: `30d`, `2w` or a date. ograph runs `ogrep --entities` to list the services, then `ogrep --timeline` for all of them in one call (one call each with ogrep 0.2.0) | `evt:Commit` per service and commit, with `evt:revision`, `evt:summary` (the subject) and up to 50 `evt:changedFile`. With a `code` source, a log burst or an exception also gets `evt:codeLocation` where ogrep finds its message or its stack frames |
| `alerts` | Alertmanager webhook (`{"alerts": [{"labels": {...}, "startsAt"}]}`) or one alert per line | `evt:Alert` with mapped severity |
| `incidents` | `{"id", "title", "service", "opened", "severity", "status", "resolved", "mitigation", "fault"}`. The id may be a string or an integer. An incident may appear in several records, files and `incidents` sources: each field comes from the last record that states it, in file path order across all incident sources and then record order, so name date-stamped exports so that they sort by date. The fields are merged before they are checked, so an incident whose service is in one file and whose open time is in another is whole after one harvest | `inc:Incident` |
| `logs` | `{"ts", "service", "message", "level"?}`, or plain text with leading timestamps (the service is then the file name) | `evt:LogError` per error-template burst (signal `LOG_ERROR`), and one for the first error lines of a message the service had not written in the `log_new_after_s` seconds of log before it, when they are too few for a burst (signal `LOG_NEW`, severity SEV4). Raw lines stay in the file |
| `metrics` | CSV with a `time` column and `<service>_<metric>` columns | `evt:MetricAnomaly` per robust-z episode. Points stay in the file |
| `traces` | `{"trace_id", "span_id", "parent_span_id", "service", "start_ms", "duration_us", "status"}`. The RCAEval names `serviceName`, `startTimeMillis`, `duration`, `statusCode`, `spanID`, `parentSpanID` and `traceID` also work. A Jaeger export, which keeps services under `processes`, needs flattening to one span per line first. A span's parent is looked up in the span's own trace. Spans without a trace id count as one trace | `evt:TraceFailed` and `evt:TraceSlow` per minute, plus `ops:dependsOn` inferred from parent spans |
| `errors` | `{"service", "at", "message", "stack", "level"?, "count"?}` from an error tracker. `app`, `time`, `timestamp`, `ts`, `error`, `title`, `stacktrace`, `trace` and `severity` also work | `evt:Exception` with mapped severity (SEV3 when the record states none). With a `code` source, up to 8 `evt:codeLocation` from the stack trace. A file's first 200 distinct traces are looked up |
| `discussions` | A Slack export: `[{"ts", "thread_ts"?, "channel", "text"}]` | `disc:Thread`, with incident references by regex. With a model, it also proposes `disc:category`, `disc:sentiment`, `disc:about` and `ops:dependsOn`. A category needs a quote from the thread that states the cause, or it becomes UNKNOWN. A dependency needs a quote that names both applications |
| `news` | `{"at", "title", "summary", "mentions": [names]}` | `evt:NewsItem` and `evt:mentions` |

Every event carries its evidence: `file#record`, `file:line`, `file@ms` or, for a commit, `repository@revision`. `ograph why` shows it.

### Code

ograph asks ogrep about code and keeps only pointers. Paths are relative to the workspace root.

| Term | Meaning |
|---|---|
| `evt:Commit` | A commit to a service's code or configuration. Signal `COMMIT`, severity SEV4 |
| `evt:Exception` | An error report with a stack trace. Signal `EXCEPTION` |
| `evt:revision` | The full commit id |
| `evt:changedFile` | A file a commit changed, or a deployment changed since the version before it |
| `evt:codeLocation` | `path:line` of the code behind an event: the statement that writes a log template, or a frame of a stack trace |

- **Correlation.** A commit is a candidate event like a deployment and scores by the generic rule. `ograph code` shows which commits changed the file behind a later event; chain scores do not use that (docs/EVIDENCE.md).
- **Speed.** Run `ogrep index .` in the code repository to make these lookups fast, and `ogrep index --history` so `ograph code` can name the commit that first added a message. ograph builds neither index. Without the first ogrep scans every file and prints a warning; without the second `ograph code` lists the reason under `degraded`.
- **Failure.** A missing or failing ogrep never fails a harvest or a query. The harvest line says `DEGRADED`, the events are kept without code locations, and the queries list the reason.

## The gateway

Enrichers never write to the store. A batch goes through these checks in order:

1. **IRI scheme.** IRIs must have the form `https://ograph.dev/id/<kind>/<slug>`.
2. **Catalog.** Applications and teams must resolve against the catalog:
   - an exact name or alias is accepted;
   - a fuzzy match at Jaro-Winkler 0.93 or above is accepted and counted (tied scores go to the first IRI in order), unless another entity scores within 0.03 of it or the name is a sibling: the same words but one, and that word unlike its counterpart (`inventory-service-uk` against `inventory-service-eu`, `v1` against `v2`), or the same words plus more that are not filler such as `svc` or `api` (`inventory-service-eu-canary` against `inventory-service-eu`). Those are quarantined as ambiguous;
   - a match from 0.80 to 0.93 is quarantined as ambiguous;
   - anything lower is quarantined as missing.
3. **Catalog-only types.** Only the catalog may create an application.
4. **Ontology membership.** Only ontology properties and classes are allowed.
5. **Closed vocabularies.** Values must come from the vocabularies declared in `og:vocabulary`.
6. **Model confidence.** A model's confidence must be at least 0.5. The `Llm` variant cannot exist without one.
7. **Model-proposed structure.** A model may propose `ops:dependsOn` only when the catalog or traces already hold the same edge. Otherwise the edge is quarantined as unconfirmed.
8. **Aliases.** An alias that already names a different application is quarantined, and so is an alias that two applications claim in one batch, for both of them.
9. **Evidence.** Each fact needs evidence that names a record this harvest read.
10. **Incremental SHACL.** Shapes run on the nodes the batch touches, seen as the store will be after the commit. They are the derived domain, range, datatype, vocabulary and functional shapes, plus `shapes.ttl`. A violation that no single fact caused, such as a missing required value, rejects the node. A bad value that another graph holds, such as a verdict's cause, is not the batch's fault and rejects nothing. The checks repeat until no violation is left.

Rejected facts go to `g:quarantine/<source>/<key>` with their reason. Accepted facts replace `g:harvest/<source>/<key>`, and only the difference is written, so an unchanged re-harvest writes nothing, and a graph's `harv:at` is when it last changed. The key is the file's path relative to the workspace, percent-encoded but otherwise kept, so two files never share a graph. Incidents are the exception: every file of every `incidents` source is one batch with the key `all`, in `g:harvest/incidents/all`, and each fact's evidence still names its `file#n`. If any incident file cannot be read, any incident source path cannot be listed, or an incident directory that held files lists none, that batch fails and every incident keeps its last good facts. The incidents graph records each file it read as `harv:file`. A file that cannot be read at all, or is empty, keeps its last good facts and is marked degraded. A full harvest clears the graphs of files that no longer exist, of sources no longer configured and of incidents that no longer have chains; verdicts are never cleared, and `--only KIND` clears only that kind's and then re-correlates and fails on a kind that is not configured. Input graphs are cleared before any batch is checked, and chain graphs after correlation, so one harvest leaves the store as two would. A source path or a directory below it that cannot be listed reports one degraded batch and keeps the graphs of every file under it, and only those: a sibling source of the same kind is still pruned. A source directory that lists no files while the store holds graphs under it, such as an unmounted mount point, is treated the same way.
