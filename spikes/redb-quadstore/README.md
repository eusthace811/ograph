# redb quad store behind spareval: spike

Throwaway prototype. It answers one question by measurement: can a pure-Rust quad store on
redb serve SPARQL through Oxigraph's evaluator (`spareval`) by implementing
`spareval::QueryableDataset`, without forking Oxigraph, and how does it compare with
Oxigraph's own in-memory `Store` on three observability workloads?

## Data shape

Everything lives in one redb file. All ids are `u64`.

### Term dictionary

A term is encoded to bytes as one tag byte followed by its payload.

| tag | term | payload |
|---|---|---|
| `0x01` | named node | IRI |
| `0x02` | blank node | blank node id |
| `0x03` | simple literal (`xsd:string`) | value |
| `0x04` | language-tagged literal | `lang 0x00 value` |
| `0x05` | other typed literal | `datatype-IRI 0x00 value` |

Two tables hold the dictionary.

| table | key | value |
|---|---|---|
| `str2id` | encoded term bytes | id |
| `id2str` | id | encoded term bytes |

Ids are dense, assigned in first-seen order starting at 1. Id 0 is never assigned, so `0` and
`u64::MAX` can bound range scans.

Query-time terms that are not in the dictionary (a FILTER constant, a computed `BIND` value)
get an ephemeral id with the top bit set. The view keeps those in memory for the life of one
query. A dictionary term always maps to its stored id, so `u64` equality stays RDF term
equality, which the trait requires of `InternalTerm`.

### Quad indexes

The idea is copied from Oxigraph's RocksDB layout: three orderings for the default graph and
six for named graphs. Each table's key is a fixed array of ids and its value is `()`. redb
compares `[u64; N]` keys element by element, so binding a prefix of an ordering turns into a
single range scan from `[prefix.., 0..]` to `[prefix.., MAX..]`.

| table | key | answers (bound positions) |
|---|---|---|
| `dspo` | `[s, p, o]` | `{}`, `{s}`, `{s,p}`, `{s,p,o}` in the default graph |
| `dpos` | `[p, o, s]` | `{p}`, `{p,o}` |
| `dosp` | `[o, s, p]` | `{o}`, `{o,s}` |
| `gspo` | `[g, s, p, o]` | named graph `g` bound, same coverage as `dspo` |
| `gpos` | `[g, p, o, s]` | named graph `g` bound, same coverage as `dpos` |
| `gosp` | `[g, o, s, p]` | named graph `g` bound, same coverage as `dosp` |
| `spog` | `[s, p, o, g]` | any named graph (`g` unbound), same coverage as `dspo` |
| `posg` | `[p, o, s, g]` | any named graph, same coverage as `dpos` |
| `ospg` | `[o, s, p, g]` | any named graph, same coverage as `dosp` |

For every subset of bound positions there is exactly one ordering whose leading positions are
that subset, so no pattern needs a scan plus filter. A default-graph quad is written to the
three `d*` tables only. A named-graph quad is written to the six `g*`/`*g` tables only. That is
what Oxigraph does, and it keeps the default-graph cost at three writes per triple.

### Time index

| table | key | value |
|---|---|---|
| `time` | `[timestamp_millis, event_id]` | `()` |

The loader writes one entry for every quad whose predicate is `ops:at` and whose object is an
`xsd:dateTime`. A window `[t0, t1]` is one range scan from `[t0, 0]` to `[t1, MAX]`. The SPARQL
path cannot reach this table, because `QueryableDataset` exposes only quad patterns.

### Generated graph

`ops:` is `http://ops.example/`. A fixed-seed SplitMix64 generator makes the same graph on
every run.

| node | count | triples per node |
|---|---|---|
| `ops:Application` | 20,000 | `rdf:type`, `ops:ownedBy` (one of 10 team literals, one is `"Bedrock"`), `ops:deployedIn` (one of 5 region literals, one is `"us-east-1"`), 1 to 5 `ops:dependsOn` |
| `ops:Incident` | 100,000 | `rdf:type`, `ops:affects` (one app), `ops:severity` (`"SEV1"`, `"SEV2"` or `"SEV3"`), `ops:openedAt` (`xsd:dateTime`) |
| event | 1,000,000 | `rdf:type` (Deployment, LogError, Alert, ConfigChange or MetricAnomaly), `ops:about` (one app), `ops:at` (`xsd:dateTime`) |

Two choices shape the result sizes, and both are deliberate.

- **Dependencies form a DAG.** App `i` depends only on apps with a lower index, drawn with a
  bias toward low indexes (`j = floor(i * u^2)`), the way real services lean on a few platform
  services. App 0 therefore has no dependencies, and app `i < 5` has at most `i`. A random
  digraph would put almost every app in one strongly connected component and make every blast
  radius the same size.
- **Some events are precursors.** Uniform events over 30 days would leave a 10-minute window on
  four apps nearly always empty (about 0.05 expected events), so the correlation query would
  measure nothing. For each incident the generator emits 0 to 5 events on the affected app or
  one of its direct dependencies, timed uniformly in the 12 minutes before the incident, so
  some fall outside the window and test the filter edge. The rest of the 1,000,000 events are
  uniform over the 30 days.

## Reproduce

Everything runs from the spike directory. The path has a space, so quote it.

```sh
cd "/Volumes/External Disk/Github/graph-rag/spikes/redb-quadstore"
cargo build --release

# Generate the graph, bulk-load redb and Oxigraph, run W1, W2 and W3 on all engines,
# and write data/store.redb and data/plans.txt (spareval plans with row counts).
./target/release/redb-quadstore

# Fresh process: open data/store.redb, run W1 twice, print per-table sizes.
./target/release/redb-quadstore open sizes

# Profile one workload on one engine (w1, w2 or w3; redb, memo or oxigraph).
./target/release/redb-quadstore profile w2 redb > p.out & P=$!
until grep -q ready p.out; do sleep 0.3; done; sample $P 8 -mayDie -file w2-redb.sample; wait $P

# C and C++ check across every target, including Windows.
cargo tree -e normal,build --target all | grep -c '\[build-dependencies\]'
cargo tree -e normal,build --target x86_64-pc-windows-msvc --prefix none | grep -Ei '^(cc|cmake|bindgen|[a-z0-9_-]*-sys) v'
```

The main run prints a `MISMATCH` line for any query where an engine's sorted rows differ from
the first engine's, and it asserts the row count on every timed run.

Versions are redb 5.0.0, oxrdf 0.3.5, spargebra 0.4.8, spareval 0.2.9 (with `sep-0002` for
`dateTime - dayTimeDuration`), and oxigraph 0.5.13 with default features off, which gives the
in-memory `Store` and no RocksDB. The machine is an Apple M6 with 12 cores and 32 GB, macOS
(Darwin 27.2), rustc 1.99.1. The redb file sits on an external disk volume. Load average was 1.8
to 3.8 during the runs because other sessions had idle builds open. Release profile, with
`debug = 1` for profiler symbols only.

## Engines

| engine | what runs |
|---|---|
| redb+spareval | `spareval::QueryEvaluator` over `store::View`, the `QueryableDataset` impl in `src/store.rs` |
| redb+spareval memo | the same, plus an override of `externalize_expression_term` that caches decoded terms for one query |
| oxigraph | `oxigraph::store::Store::new()` (in memory), queried through `SparqlEvaluator`, which runs the same spareval |
| native redb | hand-written Rust over the same redb tables, using the time index and a breadth-first walk |

Both SPARQL engines run the same evaluator and optimizer on the same parsed query, so the
comparison isolates the storage layer and its term encoding.

## Results

Numbers come from `data/run5.txt` (final code). Runs 1 to 5 are separate processes. Each query
gets one untimed run per engine for the correctness check, then 7 timed rounds that alternate the
engines. "Median" and "max" are over those 7 rounds.

### Correctness

All four engines returned the same sorted rows for all 41 queries (1 for W1, 20 for W2, 20 for
W3) in every run, with zero `MISMATCH` lines in runs 1 to 5. For W1 the `ORDER BY ?inc` order was
also identical between the two SPARQL engines. The first run caught a real bug this way.
Oxigraph returned 0 rows because its `BulkLoader` needs `commit()`, and the check flagged it.

### Load, size, open

| measure | redb + spareval | oxigraph in memory |
|---|---|---|
| bulk load, 3,520,089 quads | 8.3 to 9.2 s (5 runs) | 1.73 to 1.80 s (5 runs) |
| on disk | 544 MiB | none (process RSS for the whole benchmark peaked at 4.5 GB) |
| open in a fresh process | 1.5 to 4.0 ms, median 3.3 ms (7 runs) | n/a, must reload every process |
| first W1 after a fresh open | 12.0 to 14.0 ms, median 12.5 ms | n/a |
| second W1 in that process | 8.2 to 8.7 ms | n/a |

The redb load is one write transaction on one thread. A run split it into 1.3 s to encode,
intern and permute in memory, 1.4 s to write the two dictionary tables, 3.1 s to sort and write
the three quad indexes and the time index, and 2.2 s to commit. Oxigraph's load writes nothing to
disk. The two load numbers compare a durable single-threaded load against an in-memory one, so
they are not a like-for-like race. A CLI built on the in-memory store would pay its 1.7 s load,
plus parsing, on every invocation. The redb store pays 3 ms.

"Fresh process" means the redb cache is cold but the OS page cache is warm. An OS-cold open was
not measured, because `purge` needs root.

| redb table | entries | stored MiB | branch + fragmented MiB |
|---|---|---|---|
| str2id | 2,219,792 | 118.1 | 14.8 |
| id2str | 2,219,792 | 118.1 | 11.1 |
| dspo | 3,520,089 | 80.6 | 2.2 |
| dpos | 3,520,089 | 80.6 | 2.2 |
| dosp | 3,520,089 | 80.6 | 2.2 |
| time | 1,000,000 | 15.3 | 0.4 |

The dictionary is 262 MiB of the 544 MiB, because every term is stored twice.

### W1, Bedrock incidents in us-east-1 (1,938 rows)

| engine | median (ms) | max (ms) | median range over runs |
|---|---|---|---|
| redb+spareval | 8.13 | 8.42 | 8.13 to 8.28 (5 runs) |
| redb+spareval memo | 4.09 | 4.22 | 4.09 to 4.22 (5 runs) |
| oxigraph | 3.48 | 4.94 | 3.48 to 4.60 (5 runs) |
| native redb | 2.06 | 2.39 | 2.06 to 2.12 (runs 4 and 5) |

Native runs 1 to 3 measured 4.8 to 5.0 ms because the native path sorted with a string
allocation per comparison. The sort now builds each key once. The rdflib figure of 42 s for this
query was not reproduced here.

### W2, events in [t - 10 min, t] on the incident's app or its direct dependencies

20 incidents, each with at least one generated precursor. Rows per incident ranged from 0 to 5,
median 3, 56 in total. One incident (`inc/9346`) returned 0 rows. Its precursors all landed in the 2
minutes before the window opens, and its timings still cover a full scan.

| engine | min of medians (ms) | p50 | p90 | max of medians | max of all runs |
|---|---|---|---|---|---|
| redb+spareval | 0.127 | 0.316 | 0.594 | 0.954 | 1.036 |
| redb+spareval memo | 0.100 | 0.229 | 0.456 | 0.696 | 0.781 |
| oxigraph | 0.047 | 0.094 | 0.185 | 0.288 | 0.306 |
| native redb | 0.077 | 0.103 | 0.114 | 0.116 | 0.239 |

The spread is explained by what each path scans. The SPARQL plan reads every event ever recorded
about the apps in scope and filters by time. The native path reads the time index for the window
only. Across the 20 incidents the SPARQL candidates ranged from 104 to 850 and its median latency
tracked them at about 1.1 µs per candidate (104 took 0.127 ms, 850 took 0.848 ms). The native
candidates stayed between 181 and 275, at about 0.4 µs each. The per-incident table is in
`data/run5.txt`.

### W3, blast radius with `?x ops:dependsOn+ <app>`

20 start apps: the most depended-on (`app/0`, 854 direct dependents, blast radius 19,999 apps)
plus 19 random apps with at least one dependent. Rows ranged from 1 to 19,999, median 8.

| engine | min of medians (ms) | p50 | p90 | max of medians (app/0) | max of all runs |
|---|---|---|---|---|---|
| redb+spareval | 0.007 | 0.011 | 1.348 | 14.85 | 16.20 |
| redb+spareval memo | 0.007 | 0.011 | 1.341 | 14.89 | 20.46 |
| oxigraph | 0.007 | 0.012 | 12.419 | 135.15 | 137.15 |
| native redb | 0.001 | 0.005 | 1.199 | 12.81 | 12.90 |

The app/0 medians held at 14.8 to 15.0 ms (redb+spareval), 135.1 to 136.0 ms (oxigraph) and
12.7 to 13.0 ms (native) across runs 1 to 5.

## What limits each number

The profiles come from macOS `sample` at 1 ms intervals for 8 s while a profile run looped
the query. The shares are inclusive, from `data/prof/*.sample`.

- **W1 redb+spareval** is bound by `ORDER BY`. spareval's sort comparator evaluates the sort
  expression on both rows for every comparison, and each evaluation calls
  `externalize_expression_term`. For this store that is an `id2str` B-tree lookup and a string
  allocation, about 2 x 1,938 x 11 times per query. 73% of samples sat under the sort and 75%
  under externalization. Memoizing decoded terms per query cut the median from 8.1 ms to 4.1 ms,
  which confirms it. Oxigraph spends 50% in the same sort, but its decode is an in-memory hash
  lookup.
- **W2 redb+spareval** is bound by dictionary decoding for the FILTER. 49% of samples were in
  externalization (30% in `id2str` gets and 12% parsing `xsd:dateTime` text), and 36% in quad
  pattern scans. Oxigraph spends 1% decoding, because its `EncodedTerm` stores a dateTime value
  inline. The memo helps only with `?t0`, which repeats. Every `?t` is a different event, so
  most of that decoding stays.
- **W3 oxigraph** is bound by its in-memory storage, not by the evaluator. 88% of samples were
  in its quad iterator, 52% in its MVCC visibility check `is_node_in_range`, and 31% in mutex
  locks. The in-memory store finds `?x dependsOn <node>` by walking every quad whose object is
  `<node>`, which includes about 50 `ops:about` and 5 `ops:affects` quads per app, and filtering
  on the predicate. The redb path range-scans `dpos` on `(dependsOn, node)` and touches only
  matches. On redb, 55% was quad scans, 23% decoding the 19,999 result IRIs, and 27% allocation.
- **Native paths** are bound by redb B-tree descents. W3 native is about 0.65 µs per reached app
  for one range scan, one hash insert and one decode. W2 native is about 0.4 µs per time-index
  entry for one `dspo` seek.

The `explain` durations in `data/plans.txt` come from a run with `compute_statistics()` on,
which adds timing per node (W1 shows 76 ms there against 8 ms without it). Use only its row
counts.

## Findings

### The trait works without a fork

The spike compiles against unmodified crates.io releases. `QueryableDataset` needs three
methods: `internal_quads_for_pattern`, `internalize_term` and `externalize_term`. The other five
have defaults. `src/store.rs`, which holds the trait implementation, the dictionary, the indexes
and the loader, is 353 lines. The whole spike is 834 lines, over the 700-line target. About 130
of them are instrumentation added to explain the numbers (the profile mode, size stats,
per-incident work counts and the memo variant). It answers every query identically to Oxigraph.

### Friction

1. **Lifetimes.** `internal_quads_for_pattern` returns `impl Iterator + use<'a, Self>`, so the
   iterator may not borrow `&self`. The store therefore splits into an owner (`Tables`, which
   holds the redb read transaction and open tables) and a per-query `View<'a>` that holds
   `&'a Tables`. The iterator borrows the tables for `'a`. redb's `ReadOnlyTable::range` returns
   `Range<'_, K, V>`, which fits once the tables outlive the view. `range_owned` would also work
   at the cost of two `Arc` clones per row. Three table shapes (`[u64; 3]` and two `[u64; 4]`)
   forced a `Box<dyn Iterator>` per call.
2. **Error types.** No friction. `redb::Error` meets `Error + Send + Sync + 'static`.
   Errors raised while creating a range have to come back as the first item of the iterator.
3. **Term interning.** `internalize_term` must return an id for any term, including a FILTER
   constant or a computed value that the store never saw, and the read snapshot cannot insert.
   The view hands out ephemeral ids with the top bit set from a `RefCell` side table, and must
   always return the stored id for a stored term so that id equality stays `sameTerm`. Each call
   costs a `str2id` lookup. Oxigraph avoids the lookup by deriving ids from hashes.
4. **Decoding cost.** The evaluator decodes terms for every FILTER test and twice per ORDER BY
   comparison, through `externalize_expression_term`. With a dictionary store that is the
   largest cost in W1 and W2. The optional override is the only lever, and a per-query cache
   through it halved W1.
5. **No pushdown.** The trait exposes only quad patterns, so the time index is unreachable from
   SPARQL. A FILTER on a range cannot reach storage.
6. **No statistics.** sparopt orders joins with fixed constants per bound-position shape (for
   example 1,000 for `?s p o`), and the trait has no way to supply real counts.
7. **Features.** `dateTime - dayTimeDuration` needs spareval's `sep-0002`. Feature unification
   with oxigraph would turn it on silently, so the spike enables it explicitly.
8. **Deprecations.** `QueryEvaluator::execute` is deprecated in favor of
   `prepare(&query).execute(dataset)`.

### Join order

- **W1.** The plan starts at `?app ownedBy "Bedrock"` (1,945 rows), probes `deployedIn` per app
  (387), range-scans `?inc affects ?app` (1,938), then probes `rdf:type`. That is the same order
  as the hand-written native path. The two literal patterns carry the same estimate, so the
  choice between Bedrock (1,945 apps) and us-east-1 (about 4,000) is a tie. Whether reordering the
  query text flips it was not tested.
- **W2.** The plan starts from the bound incident, expands `dependsOn?` (6 apps for the plan
  sample), joins `?e about ?x` (271) and `?e at ?t` (271), and applies the FILTER at the top. The
  `openedAt` pattern becomes a keyless hash join with its single row. Given only quad patterns
  this is the best available plan. Its cost grows with each app's whole event history.
- **W3.** spareval walks the closure from the bound end, the same breadth-first walk as the
  native path. It is 16% slower than native on app/0.

### Where SPARQL loses to the native path

| workload | redb+spareval vs native (median) | memo vs native |
|---|---|---|
| W1 | 8.13 vs 2.06 ms, 3.9x | 4.09 ms, 2.0x |
| W2 p50 | 0.316 vs 0.103 ms, 3.1x | 0.229 ms, 2.2x |
| W2 worst incident | 0.954 vs 0.116 ms, 8.2x | 0.696 ms, 6.0x |
| W3 app/0 | 14.85 vs 12.81 ms, 1.16x | 14.89 ms, 1.16x |

W2's gap grows with retention, because the SPARQL plan scans all history for the apps in scope
while the native path scans one window.

### Against Oxigraph's in-memory store

With the memo, redb+spareval is within noise of Oxigraph on W1 (4.1 vs 3.5 to 4.6 ms). It is
2.4x slower at the W2 median (0.229 vs 0.094 ms), almost all of it in decoding dateTime values.
It is 9x faster on the W3 hub (14.9 vs 135 ms), where Oxigraph's in-memory storage is the
limiter. Oxigraph's RocksDB backend has real POS indexes and would likely close the W3 gap.
That backend was not measured, because it compiles C++.

### C and C++ dependencies

None. No crate in the graph has a build dependency on any target, including
`x86_64-pc-windows-msvc`, so `cc`, `cmake` and `bindgen` are absent and no `-sys` crate is
present. Nine crates run build scripts (crossbeam-utils, getrandom, libc, parking_lot_core,
proc-macro2, quote, redb, thiserror, zerocopy). The ones that spawn a process run `rustc` to
probe its version, and libc also runs `freebsd-version` or `emcc` on those targets. None
compiles C. The only C++ in reach is `oxrocksdb-sys`, which oxigraph pulls in
through its default `rocksdb` feature. `cargo tree -e build` on its own prints only the root,
because it follows build edges from the root, so use `-e normal,build` as above.

### What I would change in a real store

1. **Inline literal values in the id.** Store `xsd:dateTime`, numbers, booleans and short
   strings in the id itself (a tagged 128-bit id, as Oxigraph does), so FILTER and ORDER BY
   decode without touching `id2str`. The W2 profile says this is most of the gap to Oxigraph.
2. **Keep the per-query decode cache.** It is a 10-line override with a measured 2x on W1.
3. **Index events by `(app, timestamp, event)`.** The `(timestamp, event)` index scales with the
   global event rate, and the SPARQL plan scales with per-app history. A per-app time index
   scales with the window on the apps in scope. Expose the window to SPARQL through a custom
   function or a SERVICE handler (spareval has hooks for both, untested here), or run it as a
   native CLI command.
4. **Shrink the dictionary.** Hash-derived ids would drop `str2id` (132 MiB) and make
   `internalize_term` lookup-free, at the cost of wider index keys. That trade was not measured.
5. **Parallelize the encode step of the bulk load** and measure whether the 2.2 s commit is
   bound by fsync on this disk. Both are guesses from the phase timings.

## Layout

- `src/store.rs` holds the term encoding, the bulk loader, `Tables` (read side, pattern-to-index
  dispatch, time index) and `View` (the `QueryableDataset` impl).
- `src/main.rs` holds the generator, the queries, the native paths, the benchmark loop and the
  `open` and `profile` modes.
- `data/` holds `store.redb`, `plans.txt`, `run*.txt` outputs and `prof/*.sample` profiles. It is
  throwaway and can be regenerated.
