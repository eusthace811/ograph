#!/bin/sh
# Schema enforcement and provenance checks against the loaded HugeGraph (ids >= 99,000,000 are scratch).
HG=http://127.0.0.1:18080/graphs/hugegraph
g() { printf '%s\n' "$1" | python3 -c 'import json,sys; print(json.dumps({"gremlin": sys.stdin.read(), "bindings": {}, "language": "gremlin-groovy", "aliases": {"graph": "DEFAULT-hugegraph", "g": "__g_DEFAULT-hugegraph"}}))' \
  | curl -s --compressed -X POST http://127.0.0.1:18080/gremlin -H 'Content-Type: application/json' -d @- \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print("  ->", d.get("result",{}).get("data", d.get("message")))'; }
post() { echo "  POST $1 $2"; curl -s --compressed -X POST "$HG/$1" -H 'Content-Type: application/json' -d "$2" | head -c 400; echo; }

echo "== 1. edge 'affects' from an event (schema says incident -> app)"
post graph/edges '{"label":"affects","outV":20000000,"inV":0,"outVLabel":"event","inVLabel":"app","properties":{}}'
echo "== 1b. same through Gremlin"
g "g.V(20000000L).next().addEdge('affects', g.V(0L).next()); 'accepted'"
echo "== 2. incident missing openedAt (not in nullableKeys)"
post graph/vertices '{"label":"incident","id":99000001,"properties":{"severity":"SEV1"}}'
echo "== 3. severity outside SEV1..SEV3 (no enum type exists)"
post graph/vertices '{"label":"incident","id":99000002,"properties":{"severity":"SEV9","openedAt":0}}'
echo "== 3b. wrong data type for openedAt"
post graph/vertices '{"label":"incident","id":99000003,"properties":{"severity":"SEV1","openedAt":"yesterday"}}'
echo "== 3c. undeclared property"
post graph/vertices '{"label":"incident","id":99000004,"properties":{"severity":"SEV1","openedAt":0,"color":"red"}}'
echo "== 4. provenance: append nullable edge properties and a range index on confidence"
g "def s = graph.schema()
s.propertyKey('source').asText().ifNotExist().create()
s.propertyKey('evidenceId').asText().ifNotExist().create()
s.propertyKey('confidence').asDouble().ifNotExist().create()
s.propertyKey('observedAt').asLong().ifNotExist().create()
s.edgeLabel('affects').properties('source','evidenceId','confidence','observedAt').nullableKeys('source','evidenceId','confidence','observedAt').append()
s.indexLabel('affectsByConfidence').onE('affects').by('confidence').range().ifNotExist().create()
'ok'"
post graph/vertices '{"label":"incident","id":99000005,"properties":{"severity":"SEV2","openedAt":1791292887198}}'
post graph/edges '{"label":"affects","outV":99000005,"inV":0,"outVLabel":"incident","inVLabel":"app","properties":{"source":"pagerduty","evidenceId":"PD-123","confidence":0.8,"observedAt":1791292887198}}'
echo "== 4b. query by two provenance properties (only confidence is indexed)"
g "g.E().hasLabel('affects').has('confidence', P.gte(0.7d)).has('source','pagerduty').count()"
echo "== 4c. query by the indexed one, filter the other in the traversal"
g "g.E().hasLabel('affects').has('confidence', P.gte(0.7d)).filter(values('source').is('pagerduty')).project('inc','app','evidence').by(outV().id()).by(inV().id()).by('evidenceId')"
echo "== cleanup"
g "g.V(99000001L, 99000002L, 99000003L, 99000004L, 99000005L).drop().iterate(); 'dropped'"
g "g.V().hasLabel('app').inE('affects').count()"
