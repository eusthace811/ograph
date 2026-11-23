# Apache AGE and Apache HugeGraph as servers: spike

Throwaway prototype. It answers one question by measurement: should Apache AGE (PostgreSQL
extension, openCypher) or Apache HugeGraph (Java, Gremlin) be the graph engine, a supported
backend, or an export target for the observability CLI? It loads the exact graph that
`../redb-quadstore` generates, runs the same three workloads from a Rust client, checks every
answer against the redb spike's rows, and compares the numbers with `../redb-quadstore/data/run5.txt`.

## Verdict

AGE with events in a plain table (the hybrid) is a credible supported backend for teams that
already run Postgres. It loads in about 6 s, restarts in under 0.4 s, and answers the
correlation query in 0.28 to 0.33 ms warm. Its Cypher is the weak part. Variable-length paths
enumerate paths, so the blast radius of the hub never finishes, and the first path query on each
new connection spends about 0.5 s building an in-memory copy of the graph. Cypher writes also have
open lost-write bugs. Use SQL for closures and loads, and use Cypher only for fixed-shape matches.

HugeGraph is not worth adopting. It is slower than AGE on every workload, it needs 2 to 4.5 GiB
of resident memory and 10 s to start, and it has had no release in the last 12 months. It also has
an open report of data lost across a RocksDB restart. Its schema enforcement is the best of the
two engines, but that does not pay for the rest.

Neither engine reads or writes RDF. A Turtle archive per harvest is client code in either case.
Dumping the AGE graph to Turtle from Rust took 1.6 s for 3,520,089 triples.

## Data model

The generator is `../redb-quadstore/src/main.rs`, copied byte for byte into
`src/generator.in.rs` and included by `src/bin/export.rs`. The copy uses the same seed and the same
SplitMix64. `export` asserts the quad count (3,520,089), writes CSV to `data/export/`, and computes
the expected rows with the redb spike's own native path over `../redb-quadstore/data/store.redb`.
The expected rows match `run5.txt`: W1 has 1,938 rows, W2 has 56 rows over 20 incidents, and W3 has
42,185 rows over 20 start apps.

| element | AGE graph `obs` | AGE hybrid `obs_h` + `public.events` | HugeGraph |
|---|---|---|---|
| app | vertex label `app` {id, team, region} | same | vertex label `app`, id = n |
| incident | vertex label `incident` {id, severity, opened_at} | same | vertex label `incident` {severity, openedAt}, id = 10,000,000 + n |
| event | vertex label `event` {id, kind, at} | row in `events(id, kind, app_id, at)` | vertex label `event` {kind, at}, id = 20,000,000 + n |
| dependsOn | edge label `depends_on` app to app | same | edge label `dependsOn`, `sourceLabel('app').targetLabel('app')` |
| affects | edge label `affects` incident to app | same | edge label `affects`, incident to app |
| about | edge label `about` event to app | the `app_id` column | edge label `about`, event to app |

Timestamps are epoch milliseconds stored as integers (`agtype` integer in AGE, `asLong()` in
HugeGraph). The redb spike compares `xsd:dateTime` values with millisecond precision, so integer
comparison is equivalent. HugeGraph ids are `CUSTOMIZE_NUMBER` and must be unique across labels,
hence the offsets.

## Indexes

AGE 1.8.0 already creates a btree primary key on `id` for every vertex label and btrees on
`start_id` and `end_id` for every edge label (`\d obs.app` and `\d obs.depends_on` show them). The AGE
manual has no indexing page. The regression test `regress/sql/index.sql` uses GIN on `properties`
and expression btrees on `agtype_access_operator(properties, '"key"')`, so the spike adds those:

| index | serves | used in the plan (EXPLAIN ANALYZE) |
|---|---|---|
| `app USING gin (properties)` | `{team: 'Bedrock', region: 'us-east-1'}` becomes `properties @> ...` | yes, Bitmap Index Scan, 387 apps |
| `app` btree on `properties->'id'` | the W3 anchor `a.id = N` | yes |
| `incident` btree on `properties->'id'` | the W2 anchor `i.id = N` | yes |
| `event` btree on `properties->'at'` | window filter | no; the planner joins through `about_end_id_idx` and filters |
| `events (app_id, at)` (hybrid) | the window per app | yes, one index range scan per app in scope |

HugeGraph gets a secondary index `appByTeamRegion` on `(team, region)` and a range index
`eventByAt` on `at`. Adjacency needs no index because RocksDB stores each edge under both endpoints.

## Reproduce

Everything runs from this directory. The path has a space, so quote it. The machine is the same
Apple M6 (12 cores, 32 GB) as the redb spike, with Docker 29.9 and 8 GB and 12 CPUs given to the
Docker VM. Load average was 2.4 to 3.4 from other sessions during the runs.

```sh
cd "/Volumes/External Disk/Github/graph-rag/spikes/server-graphs"
cargo build --release
./target/release/export            # CSV + expected rows (needs ../redb-quadstore/data/store.redb)

docker run -d --name age -p 55432:5432 -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=obs \
  -v "$PWD/data/export":/tmp/age:ro -v age-data:/var/lib/postgresql apache/age:release_PG18_1.8.0 \
  -c shared_buffers=2GB -c effective_cache_size=4GB -c work_mem=64MB \
  -c maintenance_work_mem=1GB -c max_wal_size=8GB -c checkpoint_timeout=30min
docker exec age psql -U postgres -d obs -c "CREATE EXTENSION IF NOT EXISTS age"
docker run -d --name hugegraph -p 18080:8080 -e JAVA_OPTS="-Xms2g -Xmx2g" hugegraph/hugegraph:1.7.0

./target/release/bench age-load      # sql/age_load.sql, timed per statement, sizes
./target/release/bench hg-load       # schema via Gremlin, REST batches of 500, 8 in flight
./target/release/bench age-bench     # PROBE=1 adds the 120 s VLE probes
./target/release/bench hg-bench
./target/release/bench age-fresh     # new connection per W2 query, what a CLI pays
./target/release/bench coldstart age # DROP=1 also drops the Docker VM page cache
./target/release/bench coldstart hg
./target/release/bench age-turtle    # writes data/harvest.ttl and parses it back
docker exec -i age psql -U postgres -d obs < sql/age_schema.sql
./scripts/hg_schema.sh
```

The AGE CSV loader reads only from `/tmp/age/` inside the container. That directory is hardcoded
in `age_load.c`, so the export directory is mounted there.

`bench` gives each query one untimed run per variant, compares its sorted rows with
`data/expected/`, and prints `MISMATCH` on any difference. Then it times 7 rounds that alternate the
variants and asserts the row count on every run. Raw outputs are `data/age_run*.txt`,
`data/hg_run*.txt`, `data/age_load*.txt`, `data/hg_load*.txt`, `data/coldstart.txt`,
`data/age_fresh.txt`, `data/age_vle_probes.txt`, `data/age_schema.txt` and `data/hg_schema.txt`.

Versions are apache/age `release_PG18_1.8.0` (PostgreSQL 18, Debian 13) and hugegraph/hugegraph
`1.7.0` (Temurin 11.0.30, Ubuntu 22.04, RocksDB JNI 8.10.2), with tokio-postgres 0.7 and reqwest
0.12 on the client.

## Results

Ranges are over separate processes: runs 2 to 5 for queries (run 1 predates the last driver
change), and every load and restart I ran. Latencies are client-side wall time on the Rust driver,
including the round trip through Docker's port forwarding.

| measure | AGE graph | AGE hybrid | HugeGraph | redb + spareval (memo) | redb native | oxigraph in memory |
|---|---|---|---|---|---|---|
| load | 3.5 to 4.0 s (3 runs) | 2.5 to 3.4 s more (3 runs) | 5.5 to 5.8 s (4 runs) | 8.3 to 9.2 s | same file | 1.73 to 1.80 s |
| on disk | 346 MB | 155 MB | 292 MB | 544 MiB | same file | none |
| server memory at rest | 12 MiB anon + 89 to 503 MiB shared buffers | same server | 1.75 to 2.3 GiB after load, 2.9 to 4.5 GiB after queries | in process | in process | 4.5 GB peak RSS |
| cold start to first correct W1 | 120 to 361 ms (6 runs) | same server | 9.75 to 10.26 s (6 runs) | 3 ms open + 12.5 ms | n/a | must reload |
| W1 median | 3.6 to 5.4 ms | 3.6 to 5.1 ms | 6.4 to 12.5 ms | 8.13 (4.09) ms | 2.06 ms | 3.48 ms |
| W2 p50 of medians | 1.09 to 1.17 ms | 0.28 to 0.33 ms | 0.80 to 1.47 ms | 0.316 (0.229) ms | 0.103 ms | 0.094 ms |
| W2 worst median | 1.91 to 2.30 ms | 0.32 to 0.97 ms | 1.39 to 3.26 ms | 0.954 (0.696) ms | 0.116 ms | 0.288 ms |
| W3 p50 of medians | 1.01 to 1.06 ms (SQL CTE) | 1.01 to 1.07 ms (SQL CTE) | 0.94 to 1.57 ms | 0.011 ms | 0.005 ms | 0.012 ms |
| W3 hub, 19,999 rows | 28.2 to 29.2 ms (SQL CTE); Cypher never finishes | same | 60.9 to 64.2 ms | 14.85 (14.89) ms | 12.81 ms | 135.15 ms |
| W2, new connection per query | 453.7 ms (VLE), 8.9 ms (UNION) | 35.9 ms (VLE), 5.3 ms (UNION) | not measured | n/a | n/a | n/a |
| round trip floor | 0.09 to 0.13 ms (`SELECT 1`) | same | 0.42 to 1.50 ms (Gremlin `1`), 0.23 to 0.47 ms (`GET /versions`) | none | none | none |

Correctness: zero `MISMATCH` lines across 11 AGE and HugeGraph bench runs, including one AGE run
after a full reload and two HugeGraph runs in fresh containers. AGE returns W1 in ascending numeric
incident id. The redb spike's SPARQL `ORDER BY ?inc` sorts IRIs as strings, so the sets match but
the two orders differ.

### Load

AGE's `load_labels_from_file` and `load_edges_from_file` read the CSV on the server. In the first
load, the 1,000,000 event vertices took 1.4 s and the 1,000,000 `about` edges took 1.3 s. The "AGE
graph" figure includes `CREATE INDEX` and `VACUUM ANALYZE`. The hybrid figure is the second graph
without events plus `COPY` of events and `about` into staging tables, the join into `events`, and its
indexes. Measured as its own phase it took 2.5 to 3.4 s, against 3.5 to 4.0 s for the full graph.

HugeGraph took 2.0 to 2.5 s for the vertices and 2.0 to 2.2 s for the edges. The client sends
500-element batches, 8 in flight, with `check_vertex=false` on edges. The batch size is the
server's default `batch.max_vertices_per_batch`. Schema creation took 20 to 706 ms, depending on
whether the Groovy engine was warm.

### Storage

AGE sizes come from `pg_total_relation_size` per schema, which counts heap, indexes, and TOAST. The
data directory also holds 849 MB of `pg_wal`. With `max_wal_size=8GB` Postgres keeps old segments
for reuse, so that figure is a setting, not data. `pg_dump -Fc` of both graphs and the table is
41.9 MB. Per relation, `obs.event` is 137 MB and `obs.about` is 57 MB. The two `about` btrees add
49 MB, and the `at` expression index adds 39 MB. The hybrid stores the same events in 61 MB of heap
plus 51 MB of indexes.

HugeGraph's `rocksdb-data` was 437 MB right after the load, with only 14 MB in SST files. After the
first restart flushed the memtables it was 292 MB: 151 MB of data and 141 MB of WAL.

### Memory

Memory figures come from the container cgroup (`/sys/fs/cgroup/memory.stat`), because the
containers cannot read `/proc/*/smaps_rollup`. For Postgres, `anon` is 11 to 12 MiB. `shmem` is
the part of `shared_buffers` that queries have touched: 89 MiB after a restart and one W1, 350 to
503 MiB after the benchmarks. The 2 GB `shared_buffers` setting caps it.

For HugeGraph, `anon` is the JVM heap plus RocksDB's native memory. By default the image sets
`-XX:MaxRAMPercentage=50`, which gave `-Xmx3593m` here, and the heap grew to fill it (4.47 GiB
after load and one run). With `-Xmx1g`, the W3 hub query failed with HTTP 503: "The server available
memory 61(MB) is below than threshold 64(MB)". With `-Xmx2g`, everything passed, at 2.3 GiB after
load and 2.9 GiB after a full run.

### Cold start

`coldstart` runs `docker stop`, then `docker start`, then polls until W1 returns 1,938 rows.
Postgres answers in 120 to 181 ms, or 167 to 361 ms when the Docker VM page cache is dropped first.
HugeGraph takes about 10 s either way. Its log shows about 2.5 s until RocksDB opens and 7 s more
until `RestServer started`.

## What limits each number

- **The client is not the limiter for AGE.** `EXPLAIN ANALYZE` execution time is 3.0 to 3.9 ms for
  W1, against 3.6 to 5.4 ms at the client. The gap is the round trip (0.1 ms) plus moving 1,938 rows
  as text. For W2 hybrid the server takes 0.13 to 0.17 ms and the client sees 0.28 to 0.33 ms. That
  is about 0.15 ms of round trip and parsing, which is half the total at this size.
- **The Gremlin path is the limiter for small HugeGraph queries.** `profile()` puts W3 on app/5549
  at 0.11 to 0.13 ms on the server, against 1.35 to 2.00 ms at the client. A Gremlin script that
  returns `1` already costs 0.42 to 1.5 ms. For W1 and the hub, server time dominates: the profile
  shows 6.8 to 12.4 ms for W1 and 62 to 71 ms for the hub. Scripts pass ids as `bindings`, so the
  server compiles each script once. W1 is 11 to 14 ms in a fresh JVM and 6.4 ms after a few
  thousand queries, which is JIT warmup.
- **AGE W1** walks 387 apps from the GIN bitmap scan, probes `affects_end_id_idx` 387 times and
  `incident_pkey` 1,938 times, then sorts. That is the same join order as the redb native path.
- **AGE W2 on the graph** has the shape of the SPARQL plan. It reads every `about` edge of the apps
  in scope (850 for inc/48305), fetches each event, and filters on `at`. Server time tracks the
  history size, from 0.75 ms to 1.97 ms. The hybrid reads `events (app_id, at)` for the window only,
  so it stays flat at 0.13 to 0.17 ms across all 20 incidents.
- **The VLE graph context is a per-connection cost.** AGE answers `[:depends_on*0..1]` and every
  other variable-length pattern through `age_vle`. That function first builds an in-memory copy of
  the graph's edges in each backend: 453 to 517 ms for the full graph with 2.16M edges, and 36 ms for
  the hybrid graph with 160k. A CLI that connects per command pays this every time. Writing W2 as a
  Cypher `UNION` of the two fixed-length matches avoids it: 8.9 ms with a new connection, against
  454 ms.
- **AGE VLE enumerates paths, not vertices.** `MATCH (x)-[:depends_on*1..]->(a {id: 5549})` puts the
  unbound end first. It timed out at 120 s for a 32-row answer. Anchoring the bound end first,
  `MATCH (a:app {id: N})<-[:depends_on*1..]-(x)`, answers 18 of the 19 non-hub apps in 0.18 to
  9.8 ms. app/1 (19,829 dependents) and the hub time out, at 10 s and 120 s. On the hub, `*1..3`
  took 125 ms and `*1..5` took 2.3 s, which is path explosion in a DAG with many paths to app/0.
  The recursive CTE over `obs.depends_on` deduplicates through `UNION`. It probes `end_id` once per
  reached app (60,089 edges for the hub) and answers in 28 ms. For small answers it is bound by a
  final hash join that sequentially scans all 20,000 apps: 2.0 of 2.2 ms under EXPLAIN ANALYZE
  for app/6813.
- **HugeGraph W3** `repeat(in('dependsOn').dedup())` crosses all 60,089 edges for the hub, about
  1 µs per edge in the profile. The native `kneighbor` REST traverser was 4 to 5 times slower and
  needs a `max_depth`, which I set to 40 (the longest chain is 32). I did not find out why it is
  slower.

## Beyond speed

### Schema enforcement at write time

I ran each check. Outputs are in `data/age_schema.txt` and `data/hg_schema.txt`.

| attempt | AGE as shipped | AGE with CHECK constraints on label tables | HugeGraph |
|---|---|---|---|
| `affects` edge from an event (or app) to an app | accepted | rejected | rejected: "Undefined link of edge label 'affects': 'event' -> 'app'" |
| incident without `opened_at` | accepted | rejected | rejected: "All non-null property keys [severity, openedAt] ... missed keys [openedAt]" |
| severity `SEV9` | accepted | rejected | accepted: no enum or value constraint exists |
| `openedAt: "yesterday"` | accepted | not tested | rejected: "expect a value of type Long" |
| undeclared property `color` | accepted | accepted | rejected: "Undefined property key: 'color'" |

AGE label tables are ordinary Postgres tables, so `ALTER TABLE ... ADD CONSTRAINT ... CHECK` works.
The endpoint check is `_extract_label_id(start_id) = _label_id('chk','incident')`. Through Cypher
`CREATE`, every violation fails with the internal error "invalid perminfoindex 0 in RTE with relid
0" instead of naming the constraint. The transaction still rolls back: no orphan vertices remain. A
plain SQL `INSERT` reports `violates check constraint "incident_shape"`. HugeGraph skips the
non-null vertex check in `LOADING` mode, the mode hugegraph-loader uses
(`GraphTransaction.java:346`, read at tag 1.7.0; not tried).

### Provenance on an edge

Both engines store source, evidence id, confidence and observedAt as edge properties. In AGE it is
one `SET` with no schema change. An expression index on `confidence` serves
`WHERE r.confidence >= 0.7 AND r.source = 'pagerduty'`, and the query returned the edge. In
HugeGraph you first declare four property keys and `append()` them to the edge label as nullable
keys. Filtering by a property then needs an index label. A query on `confidence` and `source` with
only `confidence` indexed is refused ("Don't accept query based on properties [confidence, source]
that are not indexed"), so the second condition moves into a `filter()` step. Neither engine lets
one edge point at another edge. Several observations of one fact need either parallel edges or an
evidence vertex.

### Standards

Neither engine imports or exports RDF. The HugeGraph loader reads CSV, TEXT and JSON. I found no
AGE RDF tool, only an unanswered request in discussion #1706. `bench age-turtle` shows what a Turtle
archive per harvest costs on AGE. Four Cypher queries stream the graph to a Rust writer, which
produced 122 MiB of Turtle in 1.6 s. `oxttl` parsed it back into 3,520,089 triples in 1.55 s, the
same count the generator emits. For HugeGraph the same writer would page through Gremlin or REST
(not built).

### Operations on a corporate network

- **Images.** `apache/age:release_PG18_1.8.0` is 717 MB, built `FROM postgres:18` on Debian 13.
  `hugegraph/hugegraph:1.7.0` is 1.23 GB, on Ubuntu 22.04 with Temurin 11. Its classpath ships test
  and unrelated jars, among them junit, mockito, gremlin-test, cassandra-all, an HBase client,
  kubernetes-client and arthas, which a security review would have to clear.
- **Tuning.** Postgres needs the usual `shared_buffers`, `work_mem`, and WAL settings. AGE adds two
  operational traps: the `/tmp/age` loader path and the per-connection VLE context. HugeGraph needs
  an explicit heap of at least 2 GB for this graph, and about 900 MB of RSS on top of the heap.
- **Backup.** For AGE, the standard Postgres tools apply. `pg_dump -Fc` wrote 41.9 MB, and
  `pg_restore` into a fresh container took 2 s. After the restore, W1, a Cypher `CREATE`, and a VLE
  query all worked. `ag_graph.graphid` kept the source cluster's namespace oid (17105 against 17040),
  which is the condition open issue #2503 reports as breaking Cypher. Test restores against the
  operations you rely on. HugeGraph has `snapshot_create` and `snapshot_resume` REST calls and
  hugegraph-tools backup and restore. I tried neither.
- **Licenses.** AGE is Apache-2.0, and PostgreSQL uses the PostgreSQL License. The base image
  carries the usual Debian userland, including GPL packages. HugeGraph is Apache-2.0. Its release
  `LICENSE` lists 580 bundled artifacts under Apache-2.0, EPL-2.0 (Jersey, HK2, Grizzly), CDDL
  (javax.activation, jaxb-api, javax.json), EPL-1.0 (junit), EDL-1.0 and BSD-2 (the PostgreSQL JDBC
  driver). It lists no GPL or LGPL. I took this from the file and did not audit the jars.

### Activity and health

Release and issue data come from the GitHub API on 2026-11-22, checked with `gh`.

- **AGE.** Four releases in the last 12 months: PG18 1.7.0 (2026-01-21), PG17 1.7.0 (2026-02-11),
  PG18 1.8.0 (2026-07-09) and PG15 1.8.0 (2026-09-19, prerelease). 131 commits and 245 open issues.
- **AGE open correctness issues.** Silent lost writes: #2491 (`CREATE` discarded in a multi-part
  query), #2494, #2485 and #2620. Wrong results: #2618, #2473 and #2471. Restore: #2503. There are
  also about ten open segfault reports.
- **HugeGraph.** No release in the last 12 months. 1.7.0 shipped on 2025-11-16. 327 commits and 341
  open issues.
- **HugeGraph open correctness issues.** #2258 (data lost after stop and start with local RocksDB,
  open since 2023) and #2187 (edge count differs from what was imported).
- **A HugeGraph bug I hit.** `g.E().hasLabel('about').count()` fails with "Big id max length is
  16384, but got 22022", while `g.V().hasLabel('app').inE('about').count()` returns 1,000,000.

## Layout

- `src/generator.in.rs` is the verbatim copy of the redb spike's generator, native paths and
  `instances`.
- `src/bin/export.rs` writes `data/export/*.csv` and `data/expected/`.
- `src/bin/bench.rs` is the Rust driver: AGE through tokio-postgres `simple_query`, and HugeGraph
  through reqwest against `/gremlin` and `/graphs/hugegraph/...`.
- `sql/age_load.sql` is the AGE load and index script. `sql/age_schema.sql` and
  `scripts/hg_schema.sh` hold the schema and provenance checks.
- `data/` holds raw outputs. It is throwaway and can be regenerated.
