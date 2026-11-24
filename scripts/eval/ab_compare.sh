#!/usr/bin/env bash
# Checks that a new ograph binary answers exactly as an old one where nothing new is configured:
# the demo's harvest and queries byte for byte, RCAEval's per-case rankings, the similar-incident
# scores, and query latency with the two binaries taking turns.
#
#   ab_compare.sh OLD_BIN NEW_BIN WORKDIR      (RCAEval cases in $OGRAPH_DATA, default data/)
set -euo pipefail
cd "$(dirname "$0")/../.."
OLD="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"; NEW="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"; W="$3"; D="${OGRAPH_DATA:-data}"
QUERIES=("before INC-5377" "chains INC-5377" "chains INC-5377 --rank propagation" "similar INC-5377" "blast ledger-db" "news" "validate" "why INC-5377")
snap() {  # snap BIN OUT
  rm -rf "$2"; mkdir -p "$2"; cp -R examples/demo "$2/demo"
  (cd "$2/demo" && rm -f .ograph/store.redb && "$1" harvest | sed -E 's/ \([0-9.]+ s\)//; s/in [0-9.]+ s/in X s/' > ../harvest.txt
   for q in "${QUERIES[@]}"; do "$1" $q --json 2>&1 | sed -E 's/"harvested_at":"[^"]*"//g' > "../$(echo "$q" | tr ' ' '_').json" || true; done)
  python3 scripts/eval/rcaeval_score.py --bin "$1" --cases "$D/rcaeval-cases" --out "$2/rcaeval.json" | grep -v "cases in\|harvest per case" > "$2/rcaeval.txt"
  python3 scripts/eval/similar_eval.py --bin "$1" --cases "$D/rcaeval-cases" --out "$D/rcaeval-merged" | grep -v "merged harvest" > "$2/similar.txt"
  python3 -c "import json,sys; d=json.load(open(sys.argv[1]))['results']; json.dump({k:{m:r for m,r in v.items() if m!='harvest_s'} for k,v in d.items()}, open(sys.argv[1],'w'), sort_keys=True)" "$2/rcaeval.json"
}
(cd examples/demo && python3 generate.py > /dev/null)
snap "$OLD" "$W/old"; snap "$NEW" "$W/new"
same=0; total=0
for f in "$W"/old/*.json "$W"/old/*.txt; do
  n="$(basename "$f")"; total=$((total + 1))
  if cmp -s "$f" "$W/new/$n"; then same=$((same + 1)); echo "identical  $n"; else echo "DIFFERENT  $n"; fi
done
echo "$same of $total outputs identical"
grep -h -m1 "| chain" "$W/new/rcaeval.txt"; grep -h "top-1 similar" "$W/new/similar.txt"
cd "$W/new/demo"
python3 - "$OLD" "$NEW" <<'P'
import statistics, subprocess, sys, time
bins = {"old": sys.argv[1], "new": sys.argv[2]}
print("latency, median of 150 runs each, the binaries taking turns:")
for q in (["before", "INC-5377"], ["chains", "INC-5377"], ["similar", "INC-5377"], ["blast", "ledger-db"]):
    ts = {"old": [], "new": []}
    for i in range(150):
        for k in (("old", "new") if i % 2 else ("new", "old")):
            t = time.perf_counter(); subprocess.run([bins[k]] + q + ["--json"], capture_output=True); ts[k].append((time.perf_counter() - t) * 1000)
    print(f"  {' '.join(q):<18} old {statistics.median(ts['old']):.2f} ms   new {statistics.median(ts['new']):.2f} ms")
P
