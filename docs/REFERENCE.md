# ograph reference

## Commands

Every command takes `--root DIR` (the directory holding `.ograph/`, found by searching upward when omitted) and `--json`, which `serve` ignores because its output is the protocol. Exit codes follow ogrep: 0 when something was found, 1 when nothing was, 2 on error. Commands can run side by side on one store: `serve`, read commands and a harvest share `.ograph/store.redb`, and harvests and verdicts take turns through `.ograph/store.lock`.

| Command | What it does |
|---|---|
| `ograph init` | Creates `.ograph/` with `config.toml`, `ontology.ttl` and `shapes.ttl` |
| `ograph harvest [--only KIND]` | Runs the enrichers in layer order. Each source file becomes one named graph, except incidents, which all share one, then correlation runs for every incident. KIND is a kind from `config.toml` such as `config_changes`, its graph name such as `config`, or `correlation` |
| `ograph before INC [--window 10m] [--depth 4] [--known-at T]` | Events before an incident on its application and the applications it depends on (question 1). An episode that began up to 24 hours before the window and is still under way when it opens counts too |
| `ograph chains INC [--rank chain\|propagation] [--window 30m] [--top 5] [--set k=v] [--known-at T]` | Ranked root-cause applications, each with its best chain. A chain's id, such as `id:chain/inc-5377-e1fef786a90c3b12`, comes from its incident and root event, so it names the same hypothesis after any re-correlation. The number printed before an application is its chain's `corr:rank` after a harvest with the configured settings; a harvest stores the chains of the first `max_chains` applications, each with `corr:settings`, a hash of the settings that ranked it. Each chain carries `scores`, one entry per event of the chain, root first, for the link that leaves it; the last one closes the chain at the incident. An entry has `from` and `to` (`application:SIGNAL`, or `incident`), `time` (time proximity), `severity`, `known_pair`, `edge` and `score`. `score` is `known_pair_prior` when `known_pair` is true and `w_time * time + w_severity * severity` otherwise, and the chain's `confidence` is the geometric mean of the scores. `edge` says where the dependency the link crosses comes from: `same_app` (none crossed), `catalog` (declared, or inferred by ogrep from configuration), `trace` (only parent spans state it), `other`, or `indirect` (the link closes at the incident from more than one hop away). `weakest` is the first link with the lowest score. The text output prints one line per link and names the weakest link of a chain with more than one. When the store holds verdicts, an application carries `precedent` if its chain's root has the application and signal of an accepted cause of one of the three most similar past incidents that have one: that incident, its `similarity`, the cause's `app` and `signal`, and the verdict's `evidence`. With `precedent_weight` above 0 the application's `score` also gains the weight times that similarity, so it can exceed 1; a chain's `confidence` never changes. With a weight, a new verdict changes the ranks `chains` prints for similar later incidents at once and their stored `corr:rank` at the next harvest, so until then record a verdict on such an incident by chain id, not by rank. When no application ranks and a `code` source is configured, the answer also carries `suspects`, the list `ograph suspects INC` gives, and the exit code is 0 when that list is not empty |
| `ograph code INC [--top 5]` | Where an incident points in the code. `locations` are the lines behind its candidate events: the log bursts and exceptions that carry `evt:codeLocation`, newest first. `commits` are the candidate commits. Those that changed a located file come first, and `touches` lists those lines. `deployments` are the candidate deployments that carry changed files, listed the same way. A located log message also gets `introduced`, the commit that first added it to the code among the commits made until the incident, with `days_before`. That lookup needs `ogrep index --history` in the repository, and `degraded` says so when the index is missing. `files` are the files the incident's description is about, and `writeups` are the past incident write-ups that share its words. ogrep ranks both in every `code` repository, and `--top` limits them. The description is the incident title, then the applications and messages along the top-ranked chain. Candidate events come from the configured correlation window. A repository ogrep cannot answer for is named in `degraded`. Without a `code` source `files` and `writeups` are empty |
| `ograph suspects INC\|APP[,APP...] [--since WHEN]` | Likely causes from the dependency graph and recent commits, with no telemetry. It runs `ogrep --suspects` in each `catalog` and `code` repository and uses the first that answers. For an incident, the failing applications are its own and those of the other incidents opened within the correlation window of it, and commits count from the day before it until it opened. ogrep 0.2.0 cannot stop at a time, so with it later commits count too and a note says so. For application names, commits count from 24 hours ago. `--since` takes what ogrep takes: `24h`, `3d` or a date such as `2026-11-22`. A repository ogrep cannot answer for is reported on standard error |
| `ograph blast APP [--depth 8] [--known-at T]` | Dependents by distance, the business processes they support, and open incidents (question 2) |
| `ograph similar INC [--top 5]` | Past incidents with the same signature, with their accepted causes and mitigations (question 3). An incident with telemetry is compared by the applications and signals before it; what the incidents state (the application, `fault` and `inc:faultClass`) then orders the matches, by reciprocal rank fusion of the two rankings. An incident with no telemetry is compared by what it states alone. `similarity` is the cosine of the signature that chose the matches. `ruled_out` lists the roots a verdict rejected on each: `app`, `signal` and the verdict's `evidence`. It is what that incident was found not to be, and it does not change any ranking |
| `ograph news [--since DURATION]` | News that mentions a vendor our applications use (all of it by default, or only the last DURATION, such as `7d`), with the affected applications and processes (question 4) |
| `ograph why NODE` | Provenance of every fact about a node: the harvest graph, source, extractor and time, model provenance, and quarantined proposals. For a chain it also lists every verdict on it, oldest first, with its state, who gave it, when, its evidence, the rank the chain had then, and whether a later verdict superseded it (`verdicts` in `--json`) |
| `ograph verdict CHAIN accept\|reject --evidence REF --by WHO` | Records a human verdict on a chain id, or on `id:chain/<incident>-<rank>` for the chain at that rank now. Any other name that is not a chain is an error. The verdict keeps the chain's incident and root event, and stores the root's class, application, time and signal on the verdict node (`rca:rootClass`, `rca:rootOn`, `rca:rootAt`, `rca:rootSignal`), so it means the same after re-correlation or after the root's telemetry leaves the sources. A root whose telemetry is gone is no longer a candidate for correlation or `before`, except in a view of a time when the store still held it (`--known-at`, below). It also stores the rank the chain had and the hash of the settings that ranked it (`rca:rank`, `rca:settings`). Judging a chain again adds a verdict: the first is `id:verdict/<chain>`, the next `id:verdict/<chain>-v2`, and each earlier one gets `rca:supersededBy` the one after it. The verdict nothing supersedes is the current one. Only the current verdict counts: an accepted one writes `inc:cause`, and `similar` and `replay` read only it. `[verdict]` in the configuration can require evidence |
| `ograph replay [--set k=v]` | Correlates every incident that has a current verdict twice, under the configured settings and with the `--set` overrides (as `chains` takes them), on the data in the store now. It lists the verdicts each ranking contradicts. A ranking contradicts an accepted verdict when the root's application is not ranked first: `lost rank 1`, or `dropped out` when the application is not among the `max_chains` applications a harvest stores. It contradicts a rejected verdict (`rose to rank 1`) when the rejected chain itself is ranked first, because the verdict judged that chain, not the application. A flip is a verdict that the configured settings agree with and the overrides contradict, and a fixed verdict is the reverse. Run it with the change as `--set` before you edit `config.toml`: it exits 0 when no verdict flips and 1 when any does. Without `--set` nothing can flip, and the output lists the verdicts the current settings already contradict. `--json` gives every verdict with the rank it had when judged and its application and chain ranks under both settings. It also counts stored chains whose `corr:settings` is not the configured settings' hash, which the next harvest re-ranks |
| `ograph sparql 'QUERY' [--limit N] [--known-at T]` | Runs SPARQL 1.1. The prefixes are predeclared, and the default graph is the deduplicated union of all harvests. A class or property in an ograph namespace that the ontology does not declare is an error that names the nearest declared one. Each node stores only its most specific type, so a class with subclasses, such as `evt:Event`, is an error that lists them |
| `ograph validate` | Runs a full SHACL audit of the store |
| `ograph export [--out FILE]` | Writes every named graph as TriG |
| `ograph ontology check` / `ograph ontology diff OTHER.ttl` | Checks drift between the code and the ontology, or lists the changes that would break stored data |
| `ograph ontology coverage QUESTIONS [--out FILE]` | Needs a model. For each question in the file (one per line), it reports the declared names that represent it and the concepts nothing declared represents. Gaps go to `.ograph/proposed.ttl` as Turtle for a person to review, merge into `ontology.ttl`, then check with `ontology diff`. Exits 1 when any question has a gap |
| `ograph lab [--out FILE]` | Writes a self-contained HTML page of incidents, causes, timelines and similar incidents |
| `ograph changes --since T` | What the sources state differently now than at T: each fact added (`+`), removed (`-`) or changed (`~`, one value of a subject's property replaced by another), with when ograph recorded it and the graph, source and extractor it came from. See [What ograph knew at a time](#what-ograph-knew-at-a-time) |
| `ograph stats`, `ograph compact` | Shows size per table, or drops history older than `history_days` and rewrites the store file without free pages |
| `ograph serve [--addr A]` / `ograph serve --mcp` | Serves line-delimited JSON-RPC over TCP, with the address in `.ograph/serve.json`. Every 5 s it compares the path, modification time and size of each file a harvest reads (for a catalog, the YAML and `.env` files `ogrep --files` lists, so ignore rules apply; for a `code` repository, only `.git/logs/HEAD`, which changes with every commit, pull and checkout), plus `config.toml`, `ontology.ttl` and `shapes.ttl`, and re-harvests on any addition, edit or deletion, retrying a failed harvest at the next poll. With `--mcp`, serves MCP over stdio without polling |

A node name may be any of these:

- an IRI
- a prefixed name such as `id:inc/inc-5377`
- an incident id such as `INC-5377`
- an application name or alias

### What ograph knew at a time

A harvest replaces each graph with what its file says now. What it removes goes to a history in the store, with the time ograph held it. `--known-at T` on `before`, `chains`, `blast` and `sparql` answers from the store as it was at T: facts recorded after T are hidden and facts removed after T are back. `chains` and `before` compute their answer again over that view, so `chains INC --known-at T` prints what `chains INC` printed at T, given the same settings. T is an `xsd:dateTime` with a zone, such as `2026-11-25T09:30:00Z`, or how long ago, such as `90m` or `2d`.

- **The time is when ograph recorded a fact, not when it became true.** A dependency that the catalog gained last week and ograph first harvested today is known from today.
- **Only harvested episodes come back.** The view holds what a harvest recorded and the history still keeps. Raw telemetry was never in the store. `ograph compact` drops history older than `history_days`, and a T before the history starts is an error, not a partial answer.
- **A retired root is a candidate again in a view of a time when the store held it.** Without `--known-at`, a root whose telemetry left the sources is not a candidate.
- **Standard output is the answer; standard error says what the view covers**: how many quads it hides and restores, and when the history starts.
- **Cost.** The history grows by one row per quad a harvest removes or adds to a graph that already existed. A first harvest and an unchanged harvest add none. Building a view reads the history and every graph first written after T, and keeps both in memory; the time index describes the store now, so a view finds events by application without it.

`ograph changes --since T` lists the difference between that view and the store now. It leaves out harvest bookkeeping (`harv:` facts), quarantined proposals and chains, which are recomputed from the facts it lists. A fact that moved between files, or left and came back, is not a change. `serve`, `similar`, `why`, `code`, `news` and `lab` answer only from the store now.

## Configuration (`.ograph/config.toml`)

A key or table that ograph does not know, such as `[corelation]` or `windw_s`, is an error.

```toml
history_days = 90           # days of removed facts that `ograph compact` keeps; above the first table

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
precedent_weight = 0.0      # added to a root's score, times similarity, when a similar past incident's accepted cause has its application and signal; 0.01 only breaks near-ties (docs/EVIDENCE.md)
depth = 4                   # dependency hops in scope
max_chains = 10             # ranked applications whose chains a harvest stores

[verdict]                   # what evidence a verdict must cite; the defaults take any verdict
require_evidence = false    # refuse a verdict with no --evidence (one without is stored as "(none given)")
evidence_patterns = []      # regular expressions, e.g. ['^PM-\d+$']; at least one piece of evidence must match one
evidence_in_store = false   # at least one piece of evidence must name a node in the graph, as `ograph why` resolves it

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
| `incidents` | `{"id", "title", "service", "opened", "severity", "status", "resolved", "mitigation", "fault"}`. The id may be a string or an integer. An incident may appear in several records, files and `incidents` sources: each field comes from the last record that states it, in file path order across all incident sources and then record order, so name date-stamped exports so that they sort by date. The fields are merged before they are checked, so an incident whose service is in one file and whose open time is in another is whole after one harvest. Optional `classify = true` on the source asks the model (`[llm.extract]`) for the fault class of each title its files state, as `inc:faultClass` from a closed vocabulary (CPU, MEMORY, DISK, CONNECTION_POOL, QUEUE, DNS, CERTIFICATE and others; see `ontology.ttl`). A class needs a quote from the title, or it becomes UNKNOWN, which `similar` ignores. Without `[llm.extract]` the batch is degraded and says so. | `inc:Incident`; with `classify`, `inc:faultClass` |
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

Rejected facts go to `g:quarantine/<source>/<key>` with their reason. Accepted facts replace `g:harvest/<source>/<key>`, and only the difference is written, so an unchanged re-harvest writes nothing, and a graph's `harv:at` is when it last changed. What the difference removes is kept in the history (see [What ograph knew at a time](#what-ograph-knew-at-a-time)). The key is the file's path relative to the workspace, percent-encoded but otherwise kept, so two files never share a graph. Incidents are the exception: every file of every `incidents` source is one batch with the key `all`, in `g:harvest/incidents/all`, and each fact's evidence still names its `file#n`. If any incident file cannot be read, any incident source path cannot be listed, or an incident directory that held files lists none, that batch fails and every incident keeps its last good facts. The incidents graph records each file it read as `harv:file`. A file that cannot be read at all, or is empty, keeps its last good facts and is marked degraded. A full harvest clears the graphs of files that no longer exist, of sources no longer configured and of incidents that no longer have chains; verdicts are never cleared, and `--only KIND` clears only that kind's and then re-correlates and fails on a kind that is not configured. Input graphs are cleared before any batch is checked, and chain graphs after correlation, so one harvest leaves the store as two would. A source path or a directory below it that cannot be listed reports one degraded batch and keeps the graphs of every file under it, and only those: a sibling source of the same kind is still pruned. A source directory that lists no files while the store holds graphs under it, such as an unmounted mount point, is treated the same way.
