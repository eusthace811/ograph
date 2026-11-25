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
python3 scripts/eval/ambiguous_census.py --bin $B/ograph --demo examples/demo --cases data/rcaeval-cases --stores examples/demo/.ograph --gateway $E/gateway.txt --work target/ambiguous-census > $E/ambiguous-census.txt
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
python3 scripts/eval/chain_delta_gate.py --bin $B/ograph --work target/chain-delta --demo examples/demo --cases data/rcaeval-cases --merged > $E/chain-delta.txt
# Against the store bench of an earlier build: python3 scripts/eval/store_ab.py OLD/store_bench $B/store_bench > $E/store-ab.txt
# The catalog gate needs a git repository holding release/kubernetes-manifests.yaml of
# GoogleCloudPlatform/microservices-demo (git init, add the file, commit).
[ -d data/online-boutique-manifests ] && python3 scripts/eval/catalog_eval.py --bin $B/ograph --cases data/rcaeval-cases --manifests data/online-boutique-manifests --work target/catalog-eval --edges data/opentelemetry-demo > $E/catalog.txt
# Phase 12 gates. These write stores, so they run on a mirror of the cases under target/.
M=target/rcaeval-cases
python3 scripts/eval/mirror_cases.py --cases data/rcaeval-cases --out $M >/dev/null
python3 scripts/eval/test_agent_rca_eval.py 2> target/test-agent-rca-eval.txt
python3 scripts/eval/rcaeval_score.py --bin $B/ograph --cases $M --out target/rcaeval-mirror.json >/dev/null
python3 scripts/eval/chain_scores_gate.py --bin $B/ograph --cases $M --reuse --also examples/demo > target/chain-scores-gate.txt
python3 scripts/eval/precedent_eval.py --bin $B/ograph --cases $M --out target/precedent > $E/precedent.txt
python3 scripts/eval/incident_conflicts_eval.py --bin $B/ograph --demo examples/demo --cases $M --work target/incident-conflicts > $E/incident-conflicts.txt
python3 scripts/eval/uses_gate.py --bin $B/ograph --demo examples/demo --cases data/rcaeval-cases --work target/uses-gate --merged data/rcaeval-merged > $E/uses.txt
# $E/lab-check.txt and $E/lab-browser.txt keep the lane's notes above these outputs. The browser gate needs Chrome.
for w in examples/demo data/rcaeval-merged; do n=$(basename $w); python3 scripts/eval/lab_check.py --bin $B/ograph --root $w --out target/lab-$n.html > target/lab-check-$n.txt; node scripts/eval/lab_browser.mjs target/lab-$n.html target/lab-$n > target/lab-browser-$n.txt; done
# Ontology tooling, on a copy of the demo with no model and no source that asks for one.
rm -rf target/demo-nomodel && cp -R examples/demo target/demo-nomodel && rm -f target/demo-nomodel/.ograph/store.redb
python3 -c "import sys; p=sys.argv[1]; t=open(p).read(); t=t[:t.index('[llm.extract]')].replace('[[source]]\nkind = \"discussions\"\npath = \"chat-actions\"\nactions = true\n',''); open(p,'w').write(t)" target/demo-nomodel/.ograph/config.toml
python3 scripts/eval/ontology_diff_eval.py --bin $B/ograph --demo target/demo-nomodel --work target/diff-eval > $E/ontology_diff.txt
git show 25f2bc7:ontology/shapes.ttl > target/shapes-before-phase-12.ttl
python3 scripts/eval/shapes_regression.py --bin $B/ograph --base target/shapes-before-phase-12.ttl --new ontology/shapes.ttl --demo target/demo-nomodel --cases $M --work target/shapes-regression > target/shapes-regression.txt
python3 scripts/eval/ontology_lint_eval.py --bin $B/ograph --demo target/demo-nomodel --work target/lint-eval --stores target/shapes-regression/new > target/ontology-lint.txt
# Against the ograph of an earlier build, with no writes under data/:
# python3 scripts/eval/ab_readonly.py OLD/ograph $B/ograph --cases data/rcaeval-cases --demo target/demo-nomodel --work target/ab > target/ab-readonly.txt
# LONG, about 20 minutes: every verdict replayed under every setting of the grid.
# python3 scripts/eval/replay_gate.py --bin $B/ograph --cases $M --jobs 4 > $E/replay-grid.txt
# MODEL-BACKED (pi logged in, each costs money):
# similar_writeups_eval.py asks the model only in its classify and both modes, and keeps the
# answers in WORK/llm.jsonl; --modes base,plain,telemetry-base,telemetry asks nothing.
# [ -d data/opentelemetry-demo ] && python3 scripts/eval/similar_writeups_eval.py --bin $B/ograph --base OLD/ograph --repo data/opentelemetry-demo --work target/similar-writeups > $E/similar-writeups.txt
# python3 scripts/eval/action_gate.py --bin $B/ograph --demo examples/demo > $E/action-gate.txt
# scripts/eval/coverage_decline_eval.py and scripts/eval/agent_rca_eval.py: see the usage in each file.
# Not rerunnable: ruled_out_eval.py is the evidence of a setting that was dropped (ruled_out_weight).
tail -n 3 $E/*.txt
# The 10x store run takes about 20 minutes plus compaction:
# OGRAPH_CACHE_MB=4096 $B/store_bench --scale 10 --no-oxigraph --chunk 5000000 > $E/store-x10.txt
