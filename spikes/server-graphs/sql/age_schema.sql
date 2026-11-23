LOAD 'age'; SET search_path = ag_catalog, "$user", public;
SELECT create_graph('chk');
SELECT create_vlabel('chk','app'); SELECT create_vlabel('chk','incident'); SELECT create_vlabel('chk','event');
SELECT create_elabel('chk','affects');
\echo '== 1. default AGE: wrong endpoint labels, missing property, bad severity'
SELECT * FROM cypher('chk', $$ CREATE (:event {id: 1})<-[:affects]-(:event {id: 2}) $$) AS (r agtype);
SELECT * FROM cypher('chk', $$ CREATE (:incident {id: 3}) $$) AS (r agtype);
SELECT * FROM cypher('chk', $$ CREATE (:incident {id: 4, severity: 'SEV9', opened_at: 0}) $$) AS (r agtype);
SELECT * FROM cypher('chk', $$ CREATE (:app {id: 5})-[:affects]->(:app {id: 6}) $$) AS (r agtype);
SELECT * FROM cypher('chk', $$ MATCH (a)-[r:affects]->(b) RETURN labels(a)[0], labels(b)[0] $$) AS (a agtype, b agtype);
\echo '== 2. add Postgres constraints to the label tables'
DELETE FROM chk.affects; DELETE FROM chk.incident; DELETE FROM chk.event; DELETE FROM chk.app;
ALTER TABLE chk.incident ADD CONSTRAINT incident_shape CHECK (
  agtype_access_operator(VARIADIC ARRAY[properties, '"severity"'::agtype]) IN ('"SEV1"'::agtype, '"SEV2"'::agtype, '"SEV3"'::agtype)
  AND agtype_access_operator(VARIADIC ARRAY[properties, '"opened_at"'::agtype]) IS NOT NULL);
ALTER TABLE chk.affects ADD CONSTRAINT affects_ends CHECK (
  _extract_label_id(start_id) = _label_id('chk','incident') AND _extract_label_id(end_id) = _label_id('chk','app'));
SELECT * FROM cypher('chk', $$ CREATE (:event {id: 1})<-[:affects]-(:event {id: 2}) $$) AS (r agtype);
SELECT * FROM cypher('chk', $$ CREATE (:incident {id: 3}) $$) AS (r agtype);
SELECT * FROM cypher('chk', $$ CREATE (:incident {id: 4, severity: 'SEV9', opened_at: 0}) $$) AS (r agtype);
SELECT * FROM cypher('chk', $$ CREATE (:incident {id: 7, severity: 'SEV1', opened_at: 0})-[:affects]->(:app {id: 8}) $$) AS (r agtype);
SELECT count(*) AS incidents_after FROM chk.incident;
\echo '== 3. provenance as edge properties, queried by confidence and source'
SELECT * FROM cypher('chk', $$ MATCH (i:incident {id: 7})-[r:affects]->(a:app)
  SET r.source = 'pagerduty', r.evidence_id = 'PD-123', r.confidence = 0.8, r.observed_at = 1791292887198 RETURN r $$) AS (r agtype);
CREATE INDEX affects_conf ON chk.affects (agtype_access_operator(VARIADIC ARRAY[properties, '"confidence"'::agtype]));
SELECT * FROM cypher('chk', $$ MATCH (i:incident)-[r:affects]->(a:app) WHERE r.confidence >= 0.7 AND r.source = 'pagerduty'
  RETURN i.id, a.id, r.evidence_id, r.observed_at $$) AS (i agtype, a agtype, e agtype, t agtype);
SELECT drop_graph('chk', true);
