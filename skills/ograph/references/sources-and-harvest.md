# Sources and harvesting

## Create a workspace

```bash
ograph init            # writes .ograph/config.toml, ontology.ttl, shapes.ttl
```

Then list one `[[source]]` table per input in `.ograph/config.toml`. Paths are relative to the
workspace; a path may be a file or a directory (every file below it is read).

```toml
[[source]]
kind = "catalog"          # a git repo with Backstage, Compose, Kubernetes or Prometheus files
path = "catalog"
[[source]]
kind = "ownership"        # {"app","process"} / {"app","vendor"} / {"app","alias"} lines
path = "ownership.jsonl"
[[source]]
kind = "docs"             # runbooks: aliases stated outright ("ledger-db, also known as ledger-pg")
path = "runbooks"
[[source]]
kind = "deployments"      # {"service","version","at"}
path = "exports/deploys"
[[source]]
kind = "config_changes"   # {"service","key","at"}
path = "exports/config.jsonl"
[[source]]
kind = "code"             # a git repo with the services' code; since = "30d" is optional
path = "../shop"
[[source]]
kind = "alerts"           # Alertmanager webhook JSON or one alert per line
path = "exports/alerts"
[[source]]
kind = "incidents"        # {"id","title","service","opened","severity","status","resolved","mitigation"}
path = "exports/incidents"
[[source]]
kind = "logs"             # {"ts","service","message","level"} or plain text lines
path = "exports/logs"
[[source]]
kind = "metrics"          # CSV: time column, then <service>_<metric> columns
path = "exports/metrics.csv"
[[source]]
kind = "traces"           # {"trace_id","span_id","parent_span_id","service","start_ms","duration_us","status"}
path = "exports/traces"
[[source]]
kind = "errors"           # error tracker: {"service","at","message","stack","level","count"}
path = "exports/errors.jsonl"
[[source]]
kind = "discussions"      # Slack export [{"ts","thread_ts","channel","text"}]
path = "exports/slack"
[[source]]
kind = "news"             # {"at","title","summary","mentions":[...]}
path = "exports/news.jsonl"
```

- The catalog is required and is the authority: only it creates applications, and every other
  source must name an application it knows (exact name, alias, or a close spelling). Reading it
  needs `ogrep` on `PATH` (or `OGRAPH_OGREP=/path/to/ogrep`). Without it, the harvest still runs
  but quarantines every fact about a service as "not in the ops:Application catalog".
- The catalog also takes the calls ogrep infers from configuration (an environment variable
  that holds another service's address) as dependencies. Kubernetes manifests declare none, so
  there these are the whole graph. `inferred = false` on the source keeps declared ones only.
- A `code` source turns each service's commits into `COMMIT` events with their changed files.
  It also lets ogrep locate log bursts and stack traces, so `LOG_ERROR` and `EXCEPTION` events
  get a `path:line`. A deployment whose version and the one before it are tags or commit ids of
  the repository gets the files changed between them (ogrep 0.3 or newer). It may be the same
  repository as the catalog. `ograph code` and the
  `chains` fallback to `suspects` need it.
- Run `ogrep index .` in the code repository to make those lookups fast, and
  `ogrep index --history` so `ograph code` can name the commit that first added a message.
  ograph builds neither index; without the first ogrep scans every file.
- A `logs` source gives `LOG_ERROR` for a burst of one error template and `LOG_NEW` for the
  first line or two of an error message the service had not written in the ten minutes of log
  before it (`[detect] log_new_after_s`, 0 turns it off).
- An `errors` source works without a `code` source: the exceptions are events, without lines.
- Times may be epoch seconds, ms, µs or ns, or ISO 8601 with any offset; between 1971 and 2200.
- JSON files may be one array, one object, or JSON lines.
- An incident may be updated across files: each field comes from the last record that states
  it, in file-path order, so name exports so they sort by date.
- A typo in a key (`windw_s`, `[corelation]`) is an error, not ignored.
- Full field lists: `docs/REFERENCE.md`, "Input formats".

## Harvest

```bash
ograph harvest
```

```
harvest/alerts/alerts.json: 52 proposed, 52 accepted, 0 quarantined, +61 -0 quads (0.00 s)
harvest/correlation/inc-5377: 22 proposed, 22 accepted, 0 quarantined, +27 -0 quads (0.00 s)
harvested 16 batches in 0.2 s; 3 facts quarantined (see `ograph sparql` on graph g:quarantine/...)
```

- One line per batch: what was proposed, accepted by the gateway, quarantined, and the quads
  added and removed. Each file is one named graph (incidents share `g:harvest/incidents/all`).
- Re-running is safe: an unchanged file writes nothing (`+0 -0`).
- Deleted files and removed sources are pruned; verdicts are never pruned.
- Correlation reruns after every harvest, including `--only`.
- `--only KIND` harvests one kind (`alerts`, `config_changes`, `correlation`, ...).
- A source that cannot be read keeps its last good facts and shows `DEGRADED: <why>`.
- `DEGRADED: ogrep ...` on a `logs` or `errors` line means ogrep could not answer for a code
  repository: the events are kept, without code locations, until a harvest where it can.

## Model-assisted facts (optional)

With an `[llm.extract]` table, chat threads also get a category, sentiment, related incident
and proposed dependencies from a model run through a command (by default headless `pi`).
ograph holds no credentials. Answers are cached by content hash in `.ograph/cache/`, so a
re-harvest makes no model calls. A model's dependency needs a catalog or trace edge behind it.
