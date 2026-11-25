#!/usr/bin/env bash
# Writes docs/evidence/chain-scores.txt: the old binary against the new one on everything
# ab_compare.sh checks, the answers that gained fields with those fields removed, the cost of
# reading past verdicts in `chains`, then the link-score gate on the RCAEval cases and on the
# demo ab_compare.sh harvested.
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
  echo "\$ same_but_for.py old new FIELDS"
  for f in chains_INC-5377.json chains_INC-5377_--rank_propagation.json; do
    python3 scripts/eval/same_but_for.py "$W/old/$f" "$W/new/$f" scores weakest
  done
  python3 scripts/eval/same_but_for.py "$W/old/similar_INC-5377.json" "$W/new/similar_INC-5377.json" ruled_out
  echo
  echo "\$ chains on the store similar_eval.py left: 90 incidents, 90 accepted verdicts"
  python3 - "$OLD" "$NEW" "$D/rcaeval-merged" "$D/rcaeval-cases/labels.json" <<'P'
import json, statistics, subprocess, sys, time
bins = {"old": sys.argv[1], "new": sys.argv[2]}; root = sys.argv[3]
incidents = sorted(l["incident"] for l in json.load(open(sys.argv[4])).values())[:30]
ts = {"old": [], "new": []}; marked = 0
for n in range(5):
    for i in incidents:
        for k in (("old", "new") if n % 2 else ("new", "old")):
            t = time.perf_counter(); p = subprocess.run([bins[k], "--root", root, "--json", "chains", i], capture_output=True, text=True); ts[k].append((time.perf_counter() - t) * 1000)
            marked += n == 0 and k == "new" and any("precedent" in a for a in json.loads(p.stdout)["apps"])
print(f"median of {len(ts['old'])} runs over {len(incidents)} incidents, the binaries taking turns: old {statistics.median(ts['old']):.2f} ms   new {statistics.median(ts['new']):.2f} ms; the new answer carries a precedent for {marked} of the {len(incidents)}")
P
  echo
  echo "\$ chain_scores_gate.py --bin NEW --cases rcaeval-cases --reuse --also demo"
  python3 scripts/eval/chain_scores_gate.py --bin "$NEW" --cases "$D/rcaeval-cases" --jobs 3 --reuse --also "$W/new/demo"
} > "$OUT" 2>&1
cat "$OUT"
