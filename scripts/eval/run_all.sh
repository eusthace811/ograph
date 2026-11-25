#!/usr/bin/env bash
# Reruns every gate in docs/EVIDENCE.md and writes the outputs to docs/evidence/.
# Needs: ogrep on PATH, pi logged in (model gate), node (pi contract), the RCAEval export in
# data/rcaeval-cases (scripts/eval/rcaeval_export.py) and PYTHONPATH with pyarrow only for that export.
set -euo pipefail
cd "$(dirname "$0")/../.."
E=docs/evidence; B=target/release
cargo build --release
python3 scripts/eval/shacl_suite.py $B/shacl_case data/shacl-tests/repo/data-shapes-test-suite/tests/core > $E/shacl-core.txt
$B/gateway_eval > $E/gateway.txt
$B/store_bench --scale 1 --chunk 4000000 > $E/store-x1.txt
(cd examples/demo && python3 generate.py >/dev/null && rm -f .ograph/store.redb && ../../$B/ograph harvest >/dev/null)
python3 scripts/eval/llm_gate.py --bin $B/ograph --demo examples/demo > $E/llm-gate.txt
python3 scripts/eval/alias_eval.py --bin $B/ograph --demo examples/demo > $E/alias.txt
python3 scripts/eval/coverage_eval.py --bin $B/ograph --demo examples/demo > $E/coverage.txt
python3 scripts/eval/coverage_eval.py --bin $B/ograph --demo examples/demo --questions questions-heldout-b.txt --labels questions-heldout-b-labels.json > $E/coverage-heldout-b.txt
python3 scripts/eval/sparql_agent_eval.py --bin $B/ograph --demo examples/demo --repeat 3 > $E/sparql-agent.txt
python3 scripts/eval/sparql_agent_eval.py --bin $B/ograph --demo examples/demo --repeat 3 --no-skill >> $E/sparql-agent.txt
python3 scripts/agent/install.py --bin $B/ograph --root examples/demo --out target/ograph-pi.ts >/dev/null && node scripts/agent/test_pi.mjs target/ograph-pi.ts "$PWD/examples/demo" "$PWD/$B/ograph" > $E/pi-contract.txt
python3 scripts/eval/resolve_eval.py --bin $B/ograph --demo examples/demo --boutique data/fixtures/online-boutique --work target/resolve-eval > $E/resolve.txt
python3 scripts/eval/hook_eval.py --bin $B/ograph --demo examples/demo --case data/rcaeval-cases/c-011f3b9bc3 --work target/hook-eval > $E/hook.txt
# $E/cite-check.txt keeps the first, failing run of set "first" above these two
for s in first fresh; do python3 scripts/eval/cite_eval.py --bin $B/ograph --demo examples/demo --work target/cite-eval --set $s > target/cite-$s.txt; done
python3 scripts/eval/rcaeval_score.py --bin $B/ograph --cases data/rcaeval-cases --out data/rcaeval-final.json > $E/rcaeval-final.txt
$B/confirm_eval data/rcaeval-cases > $E/confirm.txt
python3 scripts/eval/similar_eval.py --bin $B/ograph --cases data/rcaeval-cases --out data/rcaeval-merged > $E/similar-eval.txt
C=data/rcaeval-cases/c-97bf1c72d2
$B/purekg_bench $C "$(python3 -c "import json;print(json.load(open('$C/incidents.json'))[0]['opened'])")" > $E/purekg.txt
# The code gates need a clone of a repository with services, code and history, for example
# git clone https://github.com/open-telemetry/opentelemetry-demo data/opentelemetry-demo
[ -d data/opentelemetry-demo ] && python3 scripts/eval/code_eval.py --bin $B/ograph --repo data/opentelemetry-demo --work target/code-eval > $E/code.txt
[ -d data/opentelemetry-demo ] && python3 scripts/eval/code_history_eval.py --bin $B/ograph --repo data/opentelemetry-demo --work target/code-history-eval > $E/code-history.txt
python3 scripts/eval/lognew_eval.py --bin $B/ograph --cases data/rcaeval-cases > $E/log-new.txt
python3 scripts/eval/known_at_gate.py --bin $B/ograph --work target/known-at-gate --cases target/known-at-cases --cases-from data/rcaeval-cases > $E/known-at.txt
# Against the store bench of an earlier build: python3 scripts/eval/store_ab.py OLD/store_bench $B/store_bench > $E/store-ab.txt
# The catalog gate needs a git repository holding release/kubernetes-manifests.yaml of
# GoogleCloudPlatform/microservices-demo (git init, add the file, commit).
[ -d data/online-boutique-manifests ] && python3 scripts/eval/catalog_eval.py --bin $B/ograph --cases data/rcaeval-cases --manifests data/online-boutique-manifests --work target/catalog-eval --edges data/opentelemetry-demo > $E/catalog.txt
tail -n 3 $E/*.txt
# The 10x store run takes about 20 minutes plus compaction:
# OGRAPH_CACHE_MB=4096 $B/store_bench --scale 10 --no-oxigraph --chunk 5000000 > $E/store-x10.txt
