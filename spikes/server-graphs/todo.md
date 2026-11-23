# Prototype playbook

- [x] 1. Scope the decision: should AGE or HugeGraph be the engine, a supported backend, or an export target, measured on the redb-quadstore dataset and workloads.
- [x] 2. Gather references. skip: direction set by the brief; docs, source and issues read for indexing, schema, licenses, activity.
- [x] 3. Build throwaway in an isolated scratch dir (spikes/server-graphs).
- [x] 4. Compare alternatives behind one switcher: `bench` subcommands, variants per workload (graph, hybrid, VLE, CTE, UNION, Gremlin, kneighbor).
- [x] 5. Verify on the matching surface: sorted rows against redb expected rows, 7 timed rounds, server-side times.
- [x] 6. Present alternatives, tradeoffs, and a recommendation (README).
