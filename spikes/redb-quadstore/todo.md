# Prototype playbook

- [x] 1. Scope the decision the prototype exists to make: can a redb quad store serve SPARQL via spareval::QueryableDataset without forking Oxigraph, and how it compares to oxigraph's in-memory Store.
- [x] 2. Gather references when the design space is open. skip: direction set by the brief; only reference is Oxigraph's own storage layout and the trait source.
- [x] 3. Build throwaway in an isolated scratch dir, separate from production source.
- [x] 4. When comparing alternatives, build them behind one switcher. Engines: redb+spareval, oxigraph Store, native redb path, one binary.
- [x] 5. Verify on the matching surface. Log timings, row counts, and cross-engine equality.
- [x] 6. Present alternatives, tradeoffs, and a recommendation.

# Task

- [x] Read QueryableDataset trait source (spareval 0.2.9) and oxigraph storage layout
- [x] README data shape section before code
- [x] store.rs: dictionary + 6 quad indexes (+ default-graph 3) + time index
- [x] QueryableDataset impl
- [x] generator (fixed seed)
- [x] bench harness: load, size, cold open, W1, W2 (sparql x2, native), W3 (sparql x2, native)
- [x] correctness: sorted row equality across engines
- [x] cargo tree: C/C++ check
- [x] benchmark-checklist vetting (limiter per query, rows returned)
- [x] README results + findings
