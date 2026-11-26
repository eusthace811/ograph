# Using ograph

ograph turns your service catalog and operational exports into one knowledge graph, then
answers the questions an incident raises: what changed before it, what it affects, whether it
happened before, and which vendor news matters. This guide takes you from install to a first
investigation. `docs/REFERENCE.md` has every option and format.

## 1. Install

The quickest way is a prebuilt release, on Linux, macOS and Windows (Git Bash). It installs
ograph and, if it is missing, ogrep into `~/.local/bin` (`~/bin` in Git Bash):

```bash
curl -fsSL https://raw.githubusercontent.com/eusthace811/ograph/main/install.sh | sh
```

Where downloaded executables are not allowed, build from source with Rust 1.85 or newer. ograph
is pure Rust, so no C compiler is needed. On Windows, rustup's MSVC toolchain also needs the
Visual Studio Build Tools for the linker.

```bash
./x doctor        # checks cargo, the crate registry and ogrep, and says what to fix
./x install       # release build into ~/.cargo/bin (Windows: ~/bin); it says what to run next
```

On a corporate network, cargo downloads crates from whatever `~/.cargo/config.toml` puts in
place of crates.io, usually an Artifactory Cargo remote. If `./x doctor` cannot fetch the
crates, it prints the lines to add. `OGRAPH_CARGO_REGISTRY=sparse+https://<host>/artifactory/api/cargo/<repo>/index/`
uses a registry for one run, and `./x vendor` prepares an offline build.

ograph also needs `ogrep` 0.3 or newer on `PATH`. It reads the service catalog (Backstage,
Compose, Kubernetes and Prometheus files), and only the catalog can create applications. Without
it the harvest still runs, but every fact about a service is quarantined as "not in the catalog",
so the questions have nothing to answer. `install.sh` installs it; from source it is:

```bash
cargo install --locked --git https://github.com/eusthace811/ogrep ogrep-cli
```

`OGRAPH_OGREP=/path/to/ogrep` points at another binary.

## 2. Try it on the demo

The demo is a small company with four incidents, already configured.

```bash
cd examples/demo
ograph harvest              # reads every source, then correlates
ograph before INC-5377      # what happened in the 10 minutes before
ograph chains INC-5377      # likely root causes, with their evidence
ograph similar INC-5377     # past incidents like it, and what fixed them
ograph blast ledger-db      # what ledger-db failing affects
ograph lab --out lab.html   # one page over everything; open it in a browser
```

## 3. Set up your own workspace

```bash
mkdir ops-graph && cd ops-graph
ograph init                 # creates .ograph/config.toml, ontology.ttl, shapes.ttl
```

Add one `[[source]]` per input to `.ograph/config.toml`. The catalog is required: every other
source names services, and only names the catalog knows are kept. Start with the catalog and
incidents, then add the rest:

```toml
[[source]]
kind = "catalog"            # git repo with Backstage, Compose, Kubernetes or Prometheus files
path = "catalog"            # inferred = false keeps only dependencies a file declares

[[source]]
kind = "incidents"          # JSON: id, title, service, opened, severity, status, mitigation
path = "exports/incidents"

[[source]]
kind = "alerts"             # Alertmanager webhook JSON
path = "exports/alerts"

[[source]]
kind = "deployments"        # JSON: service, version, at
path = "exports/deploys"
```

Other kinds: `ownership`, `docs`, `config_changes`, `code`, `logs`, `metrics`, `traces`, `errors`,
`discussions`, `news`. Their formats are in `docs/REFERENCE.md`, "Input formats".

With a model (`[llm.extract]`), a `discussions` source also reads operator actions from chat:
rollbacks, restarts, scaling, failovers, flag and config changes that someone says they did.
Each needs a quote from its message, and takes that message's time. `before` lists them as
`OPERATOR_ACTION`, and `similar` lists them under a past incident's mitigations. Correlation
leaves them out. Actions are on by default; `actions = false` on the source turns them off.
The first harvest with them on asks the model once more about every thread of that source.

Harvest and check the result:

```bash
ograph harvest
ograph validate             # expect "0 violations"
ograph sparql 'SELECT ?fact ?reason WHERE { GRAPH ?g { ?q a harv:Quarantined ; harv:fact ?fact ; harv:reason ?reason } }'
```

The last command lists facts the gateway refused and why. The usual cause is a service name
the catalog does not know: add it to the catalog, or declare an alias in `ownership` or a runbook.

Harvest again whenever exports change. Unchanged files cost nothing, deleted files are pruned,
and a source that cannot be read keeps its last good facts.

## 4. Investigate an incident

```bash
ograph before INC-5377                  # the timeline; add --window 30m if it is empty
ograph chains INC-5377                  # ranked candidates, each a hypothesis with its chain
ograph similar INC-5377                 # past incidents, confirmed causes, mitigations that worked
ograph blast ledger-db                  # dependents, business processes, open incidents
ograph why id:ev/alert-ledger-db-ledgerpoolsaturated-1795385980000   # where a fact came from
```

Two marks change how to read these answers:

- **`chronic`.** In `before` and `chains`, an event whose series (same application, signal and
  metric or log template) fired at least twice in the 30 minutes before the incident is marked
  as likely background:

  ```
    -  5.7 min  series-db        METRIC_ANOMALY  sev 0.05  id:ev/metric-series-db-gc-pause-1762276503000
                   chronic: series-db gc-pause fired 4 times in the 30 min before the incident: likely background, not a new fault
  ```

  A series that keeps firing is not the start of this fault, however early or strong it is.
  The mark does not change any rank; look past it for the first event that is not chronic.
- **`explains`.** In `chains`, an application whose anomaly came first and was still under way
  when another application's began ranks above it, even with a lower score:

  ```
    1. firmware-api           score 0.975
       explains alarm-api, device-hub: its anomaly came first and was still under way when theirs began, so it ranks above them
  ```

  The explained applications' chains are the later part of the same cascade. A known pair such
  as deployment to errors also counts only within one application, so a deploy on a healthy
  service no longer outranks the service that degraded. This is `own_degradation`, on by
  default; `--set own_degradation=false` shows the ranking without it.

Read `chains` as suggestions. When a person confirms the cause, record it with the chain id
that `chains` printed:

```bash
ograph verdict id:chain/inc-5377-a7370ff7d4358525 accept --evidence PM-5377 --by "jane (oncall)"
```

From then on, `similar` shows that cause for any incident that looks like this one.

For vendor news: `ograph news --since 7d`.

**Dependencies have dates.** With an ogrep newer than 0.3.0, each dependency the catalog
repository gained after its service was declared is dated by the commit that added it.
`before` and `chains` do not follow a dependency added after the incident opened, and say so:

```
  not followed: checkout -> search-api, added to the repository at 2026-11-26T10:01:58Z, after the incident opened
```

`--json` lists them under `withheld`. For the blast radius as it was when the incident opened:

```bash
ograph blast ledger-db --at INC-5377    # or an xsd:dateTime with a zone, or how long ago (2d)
```

## 5. Point ograph at your code

With a `code` source an investigation can end at a commit, a file and a line. The source is a
git repository that holds the services' code. It may be the catalog's repository.

```toml
[[source]]
kind = "code"               # commits become events; log lines and stack traces get code locations
path = "../shop"
since = "30d"               # optional: how far back commits are read

[[source]]
kind = "errors"             # optional: error tracker export with stack traces
path = "exports/errors.jsonl"
```

Harvest, then ask:

```bash
ograph harvest
ograph code INC-5377        # lines behind the events, commits that changed them, related files, past write-ups
ograph suspects INC-5377    # likely causes from dependencies and recent commits, with no telemetry
```

`code` reads the graph first: the lines behind the log bursts and exceptions before the
incident, and the commits before it. A commit that changed one of those lines' files is listed
first. A deployment is listed the same way when its version and the one before it are tags or
commit ids of the repository. Under a log line, `first added` names the commit that first put
that message in the code, which can be long before the window. Then ogrep ranks the files and the past incident write-ups that match the incident's
description.

`suspects` needs no telemetry. Use it when `before` and `chains` find little. With a `code`
source, `chains` adds the same list when it ranks no application.

Run `ogrep index .` in the code repository once to make these lookups fast, and
`ogrep index --history` for the `first added` line. ograph builds neither index. Without the
first ogrep scans every file.

## 6. Ask anything else

Everything is queryable with SPARQL. The ontology's prefixes, `id:` and `g:` are predeclared.

```bash
ograph sparql 'SELECT ?app WHERE { ?a ops:dependsOn+ id:app\/ledger-db ; ops:name ?app }'
ograph sparql 'SELECT ?id ?status WHERE { ?i a inc:Incident ; inc:id ?id ; inc:status ?status }'
```

Two things to know: write `/` in ids as `\/` inside a query (and a version's dots as `\.`),
and match event types by their specific class (`evt:Alert`, `evt:Deployment`, ...), since
`evt:Event` itself is not stored. A misspelled name is an error that suggests the right one. The
prefixes and more recipes are in `skills/ograph/references/sparql.md`.

Add `--json` to any command for machine-readable output. Exit codes: 0 found, 1 nothing found,
2 error.

## 7. Keep it fresh

```bash
ograph serve --addr 127.0.0.1:7474
```

`serve` re-harvests within 5 seconds of any change to a source file or to `.ograph/`
settings, and answers JSON-RPC on that address. A scheduled `ograph harvest` (cron, CI) works
too. Both can run while people and agents query.

**Before you fix or remove a source file**, ask what rests on it:

```bash
ograph uses alerts.json             # a file, or a directory of them
ograph uses incidents.json#4        # one record, as evt:sourceRecord names it
```

It lists the file's facts, the events and incidents only it states, the chains that would go,
be relinked or move, and the verdicts on them. Check those verdicts again after the change. To
take a file out, delete it and remove its `[[source]]` entry if that entry names it.

**After a harvest, when rank 1 is not what it was**, ask which file moved it. Note the time
before you harvest:

```bash
T=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ograph harvest
ograph chains INC-5377 --top 3 --since "$T"
```

```
since 2026-11-26T10:01:14Z: the ranking moved; source files changed: 1
  then  1. ledger-db 0.944  2. payments-api 0.930  3. checkout 0.900
  now   1. ledger-db 0.944  2. payments-api 0.930  3. orders-api 0.910
  g:harvest/alerts/alerts.json [alerts]: taking it back alone changes the ranking now, and it alone changes the ranking then
```

Here an alert on orders-api was added to `alerts.json`. Each file named was tested by ranking
again without its change (`needed`) and with only its change (`enough`), so it is the reason,
not a guess. A file that changed but is not named did not move the ranking. `ograph changes
--since "$T"` shows what changed in it. T must fall inside the store's history, which starts at
its first harvest.

## 8. Use it from an agent

**Any agent with a shell.** Install the skill in `skills/ograph/`, which teaches the commands,
the investigation order and the rules (chains are hypotheses; verdicts only after a person
confirms):

```bash
./x install-skill                              # ~/.claude/skills and ~/.agents/skills
./x install-skill .claude/skills               # or any directory an agent reads skills from
```

**A note at session start.** `./x install-hook` adds a Claude Code `SessionStart` hook that runs
`ograph status`. In a workspace it prints one line when source files changed since the last
full harvest or incidents have chains but no verdict, and nothing otherwise.

**MCP clients.** Point them at `ograph serve --mcp --root /path/to/workspace`. The tools are
`before`, `chains`, `blast`, `similar`, `code`, `suspects`, `why`, `uses`, `news`, `sparql`,
`resolve`, `cite_check` and `verdict`. They pass on the `chronic` and `explains` marks. The resources are `ograph://ontology`, `ograph://incidents` and
`ograph://incident/{id}`, and the prompt `investigate` carries the investigation order.

**pi.** `python3 scripts/agent/install.py --bin "$(which ograph)" --root /path/to/workspace --out ~/.pi/agent/extensions/ograph.ts`.

## 9. Maintain

```bash
ograph stats                    # store size and contents
ograph compact                  # drop history older than history_days, shrink the store file
ograph changes --since 7d       # what the sources state differently now than a week ago
ograph export --out all.trig    # every graph as TriG, for archive or another triple store
ograph ontology check           # after upgrading ograph
ograph ontology diff new.ttl    # before changing the ontology: BREAKING lines and the facts a re-harvest would quarantine
```

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `no .ograph/ here or above; run ograph init` | Run from inside the workspace, or pass `--root DIR` |
| `nothing in the graph is called INC-...` | Wrong id, or incidents not harvested yet. `ograph harvest`, then check the id |
| `before` or `chains` shows nothing | Widen `--window`, check the incident's service matches the catalog, check quarantine |
| Many facts quarantined as "not in the ops:Application catalog" | Sources use names the catalog lacks. Add aliases via `ownership` or runbooks |
| `DEGRADED: ...` on a harvest line | That file or directory could not be read; its previous facts were kept. Fix it and harvest again |
| `unknown field ...` on any command | A typo in `.ograph/config.toml`. The message names the expected keys |
| `the ontology declares no X (did you mean Y?)` | A misspelled name in a SPARQL query |
| `verdict not recorded: the ontology refuses ...` | `.ograph/ontology.ttl` is older than the binary. Run `ograph ontology check` and update it |
| `DEGRADED: ogrep ...` on a `code`, `logs` or `errors` line | ogrep could not answer for a code repository. The events are kept without code locations. Check that the path is a git repository and that `ogrep` runs there, then harvest again |
| `DEGRADED: catalog: dependencies carry no date` | ogrep 0.3.0 or older cannot date dependencies, so each holds at every time. Everything else works. An ogrep release after 0.3.0 dates them |
| `DEGRADED: ogrep not runnable` on the catalog line, and almost everything quarantined | `ogrep` is not on `PATH`; install it (section 1) or set `OGRAPH_OGREP`, then harvest again |
| Windows: `running pi` fails in a harvest with `[llm.extract]` | npm installs pi as `pi.cmd`, which Windows starts only through `cmd`. Begin the command with `"cmd", "/C", "pi", ...` |
| Windows: `./x install` says ograph.exe is still running | Stop `ograph serve` (Ctrl-C), or `taskkill //IM ograph.exe //F` |
