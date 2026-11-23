-- step graph: full graph, events as vertices
SELECT create_graph('obs');
SELECT create_vlabel('obs','app');
SELECT create_vlabel('obs','incident');
SELECT create_vlabel('obs','event');
SELECT create_elabel('obs','depends_on');
SELECT create_elabel('obs','affects');
SELECT create_elabel('obs','about');
SELECT load_labels_from_file('obs','app','app.csv', true, true);
SELECT load_labels_from_file('obs','incident','incident.csv', true, true);
SELECT load_labels_from_file('obs','event','event.csv', true, true);
SELECT load_edges_from_file('obs','depends_on','depends_on.csv', true);
SELECT load_edges_from_file('obs','affects','affects.csv', true);
SELECT load_edges_from_file('obs','about','about.csv', true);
-- step graph indexes
CREATE INDEX app_props_gin ON obs.app USING gin (properties);
CREATE INDEX app_id_btree ON obs.app (ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, '"id"'::agtype]));
CREATE INDEX incident_id_btree ON obs.incident (ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, '"id"'::agtype]));
CREATE INDEX event_at_btree ON obs.event (ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, '"at"'::agtype]));
VACUUM ANALYZE;
-- step hybrid: graph without events, events in a plain table
SELECT create_graph('obs_h');
SELECT create_vlabel('obs_h','app');
SELECT create_vlabel('obs_h','incident');
SELECT create_elabel('obs_h','depends_on');
SELECT create_elabel('obs_h','affects');
SELECT load_labels_from_file('obs_h','app','app.csv', true, true);
SELECT load_labels_from_file('obs_h','incident','incident.csv', true, true);
SELECT load_edges_from_file('obs_h','depends_on','depends_on.csv', true);
SELECT load_edges_from_file('obs_h','affects','affects.csv', true);
CREATE TABLE public.events (id bigint PRIMARY KEY, kind text NOT NULL, app_id integer NOT NULL, at bigint NOT NULL);
CREATE TEMP TABLE ev_stage (id bigint, kind text, at bigint);
CREATE TEMP TABLE ab_stage (start_id bigint, sv text, end_id integer, ev text);
COPY ev_stage FROM '/tmp/age/event.csv' WITH (FORMAT csv, HEADER true);
COPY ab_stage FROM '/tmp/age/about.csv' WITH (FORMAT csv, HEADER true);
INSERT INTO public.events SELECT e.id, e.kind, a.end_id, e.at FROM ev_stage e JOIN ab_stage a ON a.start_id = e.id;
-- step hybrid indexes
CREATE INDEX events_app_at ON public.events (app_id, at);
CREATE INDEX happ_props_gin ON obs_h.app USING gin (properties);
CREATE INDEX happ_id_btree ON obs_h.app (ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, '"id"'::agtype]));
CREATE INDEX hincident_id_btree ON obs_h.incident (ag_catalog.agtype_access_operator(VARIADIC ARRAY[properties, '"id"'::agtype]));
VACUUM ANALYZE;
CHECKPOINT
