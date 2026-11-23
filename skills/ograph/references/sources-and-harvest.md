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

## Model-assisted facts (optional)

With an `[llm.extract]` table, chat threads also get a category, sentiment, related incident
and proposed dependencies from a model run through a command (by default headless `pi`).
ograph holds no credentials. Answers are cached by content hash in `.ograph/cache/`, so a
re-harvest makes no model calls. A model's dependency needs a catalog or trace edge behind it.
