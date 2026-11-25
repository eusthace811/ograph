#!/usr/bin/env bash
# Writes docs/evidence/chain-scores.txt: the old binary against the new one on everything
# ab_compare.sh checks, the two chains answers with the new fields removed, then the link-score
# gate on the RCAEval cases and on the demo ab_compare.sh harvested.
#
#   chain_scores_evidence.sh OLD_BIN NEW_BIN CASES_PARENT WORKDIR
#
# CASES_PARENT holds rcaeval-cases (a mirror made by link_cases.py keeps the stores out of data/).
set -euo pipefail
cd "$(dirname "$0")/../.."
OLD="$1"; NEW="$2"; D="$3"; W="$4"; OUT=docs/evidence/chain-scores.txt
{
  echo "\$ ab_compare.sh OLD NEW   (OLD is the commit before the change)"
  OGRAPH_DATA="$D" bash scripts/eval/ab_compare.sh "$OLD" "$NEW" "$W"
  echo
  echo "\$ same_but_for.py old new scores weakest"
  for f in chains_INC-5377.json chains_INC-5377_--rank_propagation.json; do
    python3 scripts/eval/same_but_for.py "$W/old/$f" "$W/new/$f" scores weakest
  done
  echo
  echo "\$ chain_scores_gate.py --bin NEW --cases rcaeval-cases --reuse --also demo"
  python3 scripts/eval/chain_scores_gate.py --bin "$NEW" --cases "$D/rcaeval-cases" --jobs 3 --reuse --also "$W/new/demo"
} > "$OUT" 2>&1
cat "$OUT"
