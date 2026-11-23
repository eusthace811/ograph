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
python3 scripts/eval/rcaeval_score.py --bin $B/ograph --cases data/rcaeval-cases --out data/rcaeval-final.json > $E/rcaeval-final.txt
$B/confirm_eval data/rcaeval-cases > $E/confirm.txt
python3 scripts/eval/similar_eval.py --bin $B/ograph --cases data/rcaeval-cases --out data/rcaeval-merged > $E/similar-eval.txt
C=data/rcaeval-cases/c-97bf1c72d2
$B/purekg_bench $C "$(python3 -c "import json;print(json.load(open('$C/incidents.json'))[0]['opened'])")" > $E/purekg.txt
tail -n 3 $E/*.txt
# The 10x store run takes about 20 minutes plus compaction:
# OGRAPH_CACHE_MB=4096 $B/store_bench --scale 10 --no-oxigraph --chunk 5000000 > $E/store-x10.txt
