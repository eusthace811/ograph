//! A workspace is a `.ograph/` directory: `config.toml`, `store.redb`, the ontology and shapes
//! (copied in by `init`, so they are versioned with the data), and the model cache.

use crate::correlate::{self, Settings};
use crate::enrich::{self, Ctx, Detect, Source};
use crate::gateway::{self, Outcome};
use crate::llm::{Llm, LlmConfig};
use crate::model::{Batch, EntityRef, Extraction, known, lit, obj, slug};
use crate::ontology::Ontology;
use crate::store::Store;
use crate::term;
use crate::vocab::{self, RDF_TYPE, corr, evt, inc, rca};
use oxrdf::NamedNode;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "source")]
    pub sources: Vec<Source>,
    pub detect: Detect,
    pub correlation: Settings,
    pub llm: Option<LlmSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    pub extract: Option<LlmConfig>,
}

pub const DEFAULT_CONFIG: &str = r#"# ograph configuration. Paths are relative to the directory holding .ograph/.
# One [[source]] per input; harvest runs them in a fixed layer order whatever the order here.

# [[source]]
# kind = "catalog"        # runs `ogrep --entities . --json` in this repository
# path = "."
# [[source]]
# kind = "incidents"      # JSON or JSON lines: id, title, service, opened, severity, status
# path = "exports/incidents.json"
# Other kinds: ownership, deployments, config_changes, alerts, logs, metrics, traces,
# discussions, news. See docs/REFERENCE.md for each format.

[detect]
metric_z = 5.0
metric_baseline_s = 300

[correlation]
window_s = 1800         # 30 minutes; Netflix uses 10 (see docs/EVIDENCE.md)
min_confidence = 0.6
w_time = 0.6
w_severity = 0.4
rank = "chain"          # Netflix scoring; "propagation" suits a short window

# Model calls go through pi, which owns authentication. Remove this section to harvest
# without a model.
# [llm.extract]
# command = ["pi", "-p", "--no-tools", "--no-session", "--no-extensions", "--no-skills",
#            "--no-context-files", "--offline", "--thinking", "off", "--model", "openai/gpt-6-luna"]
# model = "pi:openai/gpt-6-luna"
# parallel = 4
"#;

pub struct Workspace {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub config: Config,
    pub ontology: Ontology,
    pub store: Store,
}

pub fn find(start: &Path) -> Option<PathBuf> {
    let mut d = Some(start);
    while let Some(x) = d {
        if x.join(".ograph").is_dir() {
            return Some(x.to_owned());
        }
        d = x.parent();
    }
    None
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl Workspace {
    pub fn init(root: &Path) -> anyhow::Result<PathBuf> {
        let dir = root.join(".ograph");
        std::fs::create_dir_all(&dir)?;
        for (name, body) in [("config.toml", DEFAULT_CONFIG), ("ontology.ttl", crate::ontology::DEFAULT_TTL), ("shapes.ttl", crate::ontology::DEFAULT_SHAPES)] {
            let p = dir.join(name);
            if !p.exists() {
                std::fs::write(&p, body)?;
            }
        }
        Ok(dir)
    }

    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let dir = root.join(".ograph");
        anyhow::ensure!(dir.is_dir(), "no .ograph/ in {}; run `ograph init`", root.display());
        let config: Config = toml::from_str(&std::fs::read_to_string(dir.join("config.toml"))?)?;
        let read = |n: &str, d: &str| std::fs::read_to_string(dir.join(n)).unwrap_or_else(|_| d.to_owned());
        let ontology = Ontology::load(&read("ontology.ttl", crate::ontology::DEFAULT_TTL), &read("shapes.ttl", crate::ontology::DEFAULT_SHAPES))?;
        let store = Store::open(&dir.join("store.redb"))?;
        Ok(Self { root: root.to_owned(), dir, config, ontology, store })
    }

    pub fn llm(&self) -> Option<Llm> {
        let cfg = self.config.llm.as_ref()?.extract.clone()?;
        Some(Llm::new(cfg, self.dir.join("cache").join("llm.jsonl")))
    }

    /// Runs every source in layer order, then correlation for every incident. Each source file
    /// is one batch through the gateway.
    pub fn harvest(&self, only: Option<&str>, mut progress: impl FnMut(&Outcome, f64)) -> anyhow::Result<Vec<Outcome>> {
        let _write = self.store.write_lock()?;
        let llm = self.llm();
        let ctx = Ctx { root: &self.root, detect: &self.config.detect, llm: llm.as_ref(), ontology: &self.ontology };
        let mut sources = self.config.sources.clone();
        sources.sort();
        let ran: Vec<&Source> = sources.iter().filter(|s| only.is_none_or(|o| o == s.name() || o == kind(s))).collect();
        if let Some(o) = only.filter(|o| *o != "correlation" && ran.is_empty()) {
            let mut names: Vec<&str> = sources.iter().map(kind).collect();
            names.dedup();
            anyhow::bail!("no source of kind {o:?} is configured; configured kinds: {}, correlation", names.join(", "));
        }
        // A source whose path cannot be listed (a mistyped path, an unmounted disk) reports one
        // failed batch and keeps every graph it had.
        let mut unreadable = Vec::new();
        let mut batches = Vec::new();
        for src in &ran {
            let p = self.root.join(src.path());
            if p.is_file() || std::fs::read_dir(&p).is_ok() {
                batches.extend(enrich::harvest(src, &ctx));
            } else {
                unreadable.push(src.name());
                let mut b = Batch::new(src.name(), &src.path().to_string_lossy(), "failed");
                b.degraded.push(format!("cannot read {}", p.display()));
                batches.push(b);
            }
        }
        let mut kept: HashSet<NamedNode> = batches.iter().flat_map(|b| [b.graph(), b.quarantine_graph()]).collect();
        let kept_below: Vec<String> = batches.iter().filter(|b| b.failed()).flat_map(|b| [b.graph(), b.quarantine_graph()].map(|g| format!("{}/", g.as_str()))).collect();
        // A full harvest owns every harvest graph but the verdicts; `--only` owns its kind's.
        let mut names: Vec<String> = match only {
            None => self.prunable_sources()?,
            Some(_) => ran.iter().map(|s| s.name().to_owned()).collect(),
        };
        names.retain(|n| !unreadable.contains(&n.as_str()));
        let correlate = only.is_none_or(|o| o == "correlation");
        if correlate && only.is_some() {
            names.push("correlation".to_owned());
        }
        // Input graphs whose files are gone are pruned before any batch or correlation reads
        // them, and chains after correlation writes them, so one harvest leaves the store as
        // two would.
        let (chains, inputs): (Vec<String>, Vec<String>) = names.into_iter().partition(|n| n == "correlation");
        self.prune(&inputs, &kept, &kept_below)?;
        let mut out = Vec::new();
        let mut catalog = None;
        for batch in batches {
            let t = std::time::Instant::now();
            let o = gateway::submit_with(&self.store, &self.ontology, &batch, now_ms(), false, &mut catalog)?;
            progress(&o, t.elapsed().as_secs_f64());
            out.push(o);
        }
        if correlate {
            for (o, b) in self.correlate_all()? {
                progress(&o, 0.0);
                kept.extend([b.graph(), b.quarantine_graph()]);
                out.push(o);
            }
        }
        self.prune(&chains, &kept, &kept_below)?;
        self.store.flush()?;
        Ok(out)
    }

    /// Every source name with a graph in the store, except the verdicts people recorded.
    fn prunable_sources(&self) -> anyhow::Result<Vec<String>> {
        let snap = self.store.snapshot()?;
        let mut names = std::collections::BTreeSet::new();
        for (g, _) in snap.graphs()? {
            if let oxrdf::Term::NamedNode(n) = snap.term(g)?
                && let Some(rest) = n.as_str().strip_prefix(vocab::GRAPH)
                && let Some(rest) = rest.strip_prefix("harvest/").or_else(|| rest.strip_prefix("quarantine/"))
                && let Some((source, _)) = rest.split_once('/')
                && source != "verdicts"
            {
                names.insert(source.to_owned());
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Clears the graphs of `sources` that this run did not write: files that were deleted or
    /// renamed, and incidents that no longer have chains. Graphs under `kept_below`, the keys of
    /// unreadable directories, stay.
    fn prune(&self, sources: &[String], kept: &HashSet<NamedNode>, kept_below: &[String]) -> anyhow::Result<()> {
        let prefixes: Vec<String> = sources
            .iter()
            .flat_map(|s| [format!("{}harvest/{}/", vocab::GRAPH, slug(s)), format!("{}/{}/", vocab::QUARANTINE, slug(s))])
            .collect();
        let snap = self.store.snapshot()?;
        let mut stale = Vec::new();
        for (g, _) in snap.graphs()? {
            if let oxrdf::Term::NamedNode(n) = snap.term(g)?
                && prefixes.iter().any(|p| n.as_str().starts_with(p.as_str()))
                && !kept.contains(&n)
                && !kept_below.iter().any(|p| n.as_str().starts_with(p.as_str()))
            {
                stale.push(g);
            }
        }
        drop(snap);
        if !stale.is_empty() {
            self.store.transact_with(false, |tx| stale.into_iter().try_for_each(|g| tx.clear_graph(g).map(drop)))?;
        }
        Ok(())
    }

    /// Writes the chains of every incident, one correlation batch per incident, non-durably:
    /// the caller flushes.
    fn correlate_all(&self) -> anyhow::Result<Vec<(Outcome, Batch)>> {
        let snap = self.store.snapshot()?;
        let (Ok(ids), Some(c)) = (correlate::Ids::new(&snap), snap.iri(inc::INCIDENT)?) else { return Ok(vec![]) };
        let incidents = snap.subjects(ids.ty, c)?;
        let mut batches = Vec::new();
        for i in incidents {
            let Ok(a) = correlate::analyze(&snap, &ids, i, &self.config.correlation) else { continue };
            batches.push(chain_batch(&a));
        }
        drop(snap);
        let mut catalog = None;
        batches.into_iter().map(|b| Ok((gateway::submit_with(&self.store, &self.ontology, &b, now_ms(), false, &mut catalog)?, b))).collect()
    }

    /// Records a human verdict on a chain. The verdict keeps what it judged (the chain's
    /// incident and root event, and the root's class, application, time and signal), so it
    /// means the same after the telemetry behind the root is retired or the chain's rank
    /// changes. An accepted chain also writes `inc:cause`.
    pub fn verdict(&self, chain: &str, accept: bool, by: &str, evidence: &[String]) -> anyhow::Result<Outcome> {
        let _write = self.store.write_lock()?;
        let snap = self.store.snapshot()?;
        let cid = find_chain(&snap, chain)?;
        let object = |s: u64, p: &str| -> anyhow::Result<Option<oxrdf::Term>> {
            let Some(p) = snap.iri(p)? else { return Ok(None) };
            Ok(snap.object(s, p)?.map(|o| snap.term(o)).transpose()?)
        };
        let named = |t: Option<oxrdf::Term>| match t {
            Some(oxrdf::Term::NamedNode(n)) => Ok(n),
            _ => Err(anyhow::anyhow!("{chain} is not a chain with an incident and a root cause")),
        };
        let chain_iri = named(Some(snap.term(cid)?))?;
        let incident = named(object(cid, corr::LEADS_TO)?)?;
        let root = named(object(cid, corr::ROOT_CAUSE)?)?;
        let root_id = snap.iri(root.as_str())?.expect("a chain's root is in the store");
        let mut kept = Vec::new();
        for p in [RDF_TYPE, evt::ON, evt::AT, evt::SIGNAL] {
            if let Some(o) = object(root_id, p)? {
                kept.push((p, o));
            }
        }
        drop(snap);
        let local = chain_iri.as_str().rsplit('/').next().unwrap_or("chain").to_owned();
        let mut b = Batch::new("verdicts", &local, "human");
        let m = Extraction::Human { who: by.to_owned() };
        let ev = b.record(format!("verdict:{local}"));
        let v = known("verdict", &local);
        b.add(v.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(rca::VERDICT)), &ev, &m);
        b.add(v.clone(), rca::ON_CHAIN, obj(chain_iri.clone()), &ev, &m);
        b.add(v.clone(), rca::STATE, lit(if accept { "accepted" } else { "rejected" }), &ev, &m);
        b.add(v.clone(), rca::BY, lit(by), &ev, &m);
        b.add(v.clone(), rca::AT, crate::enrich::dt(now_ms()), &ev, &m);
        for e in if evidence.is_empty() { vec!["(none given)".to_owned()] } else { evidence.to_vec() } {
            b.add(v.clone(), rca::EVIDENCE, lit(e.as_str()), &ev, &m);
        }
        let c = EntityRef::Known(chain_iri);
        b.add(c.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(corr::CHAIN)), &ev, &m);
        b.add(c.clone(), corr::LEADS_TO, obj(incident.clone()), &ev, &m);
        b.add(c, corr::ROOT_CAUSE, obj(root.clone()), &ev, &m);
        for (p, o) in kept {
            let o = match o {
                oxrdf::Term::NamedNode(n) => obj(n),
                oxrdf::Term::Literal(l) => lit(l),
                _ => continue,
            };
            b.add(EntityRef::Known(root.clone()), p, o, &ev, &m);
        }
        if accept {
            b.add(EntityRef::Known(incident), inc::CAUSE, obj(root), &ev, &m);
        }
        gateway::submit(&self.store, &self.ontology, &b, now_ms())
    }
}

/// A source's kind as `config.toml` names it, which `--only` accepts besides its graph name.
fn kind(s: &Source) -> &'static str {
    match s {
        Source::ConfigChanges { .. } => "config_changes",
        _ => s.name(),
    }
}

/// A chain by IRI, or `id:chain/<incident>-<rank>` for the chain at that rank now.
fn find_chain(snap: &crate::store::Snapshot, chain: &str) -> anyhow::Result<u64> {
    let iri = resolve_iri(chain);
    if let Some(id) = snap.iri(&iri)? {
        return Ok(id);
    }
    let missing = || anyhow::anyhow!("no chain {chain}; `ograph chains INCIDENT` lists them");
    let (incident, rank) = iri.rsplit('/').next().and_then(|l| l.rsplit_once('-')).ok_or_else(missing)?;
    let rank: i64 = rank.parse().map_err(|_| missing())?;
    let (Some(leads), Some(rank_p), Some(i), Some(r)) = (
        snap.iri(corr::LEADS_TO)?,
        snap.iri(corr::RANK)?,
        snap.iri(crate::model::entity("inc", incident).as_str())?,
        snap.id(&oxrdf::Literal::from(rank).into())?,
    ) else {
        return Err(missing());
    };
    snap.subjects(leads, i)?.into_iter().find(|&c| snap.contains([c, rank_p, r]).unwrap_or(false)).ok_or_else(missing)
}

pub fn resolve_iri(s: &str) -> String {
    let s = s.trim_matches(['<', '>']);
    if s.starts_with("http") {
        return s.to_owned();
    }
    for (p, ns) in crate::vocab::PREFIXES {
        if let Some(local) = s.strip_prefix(&format!("{p}:")) {
            return format!("{ns}{local}");
        }
    }
    s.to_owned()
}

/// The chains of one analysis as a correlation batch.
pub fn chain_batch(a: &correlate::Analysis) -> Batch {
    let local = a.incident.rsplit('/').next().unwrap_or("inc").to_owned();
    let mut b = Batch::new("correlation", &local, "correlator-v1");
    let m = Extraction::Rule { extractor: "correlator-v1".into() };
    let ev = b.record(format!("query:chains {local}"));
    for (rank, c) in a.chains.iter().enumerate() {
        let node = EntityRef::Known(NamedNode::new_unchecked(&c.id));
        b.add(node.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(corr::CHAIN)), &ev, &m);
        b.add(node.clone(), corr::LEADS_TO, obj(NamedNode::new_unchecked(&a.incident)), &ev, &m);
        b.add(node.clone(), corr::ROOT_CAUSE, obj(NamedNode::new_unchecked(&c.root.iri)), &ev, &m);
        b.add(node.clone(), corr::CANDIDATE_APP, obj(NamedNode::new_unchecked(&c.root.app_iri)), &ev, &m);
        for l in &c.links {
            b.add(node.clone(), corr::LINK, obj(NamedNode::new_unchecked(&l.iri)), &ev, &m);
        }
        b.add(node.clone(), corr::CONFIDENCE, lit(term::decimal(c.confidence)), &ev, &m);
        b.add(node, corr::RANK, lit(oxrdf::Literal::from(rank as i64 + 1)), &ev, &m);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::Workspace;
    use std::path::{Path, PathBuf};

    const T: i64 = 1_795_386_400;

    /// A workspace whose catalog is checkout -> payments-api -> ledger-db, with an incident on
    /// checkout at `T` and `sources` (kind, file, body) beside it.
    fn fixture(name: &str, sources: &[(&str, &str, &str)]) -> PathBuf {
        static OGREP: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let ogrep = OGREP.get_or_init(|| {
            let p = std::env::temp_dir().join(format!("ograph-test-ogrep-{}", std::process::id()));
            let line = |id: &str, to: &str| {
                let edges = if to.is_empty() { String::new() } else { format!(r#"{{"evidence":{{"file":"c.yaml","line":1}},"from":"{id}","provenance":"declared","relation":"depends_on","to":"{to}"}}"#) };
                format!(r#"{{"data":{{"edges":[{edges}],"entity":{{"id":"{id}","kind":"service","locations":[{{"file":"c.yaml","line":1}}]}}}}}}"#)
            };
            let body = [line("checkout", "payments-api"), line("payments-api", "ledger-db"), line("ledger-db", "")].join("\n");
            std::fs::write(&p, format!("#!/bin/sh\ncat <<'X'\n{body}\nX\n")).unwrap();
            std::process::Command::new("chmod").args(["+x", p.to_str().unwrap()]).status().unwrap();
            // SAFETY: every test sets the same value once, before any harvest reads it.
            unsafe { std::env::set_var("OGRAPH_OGREP", &p) };
            p
        });
        assert!(ogrep.exists());
        let root = std::env::temp_dir().join(format!("ograph-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Workspace::init(&root).unwrap();
        let incident = format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T},"severity":"SEV1","status":"open"}}]"#);
        let mut config = String::from("[[source]]\nkind = \"catalog\"\npath = \".\"\n");
        for (kind, file, body) in [("incidents", "incidents.json", incident.as_str())].iter().chain(sources) {
            std::fs::write(root.join(file), body).unwrap();
            config += &format!("[[source]]\nkind = \"{kind}\"\npath = \"{file}\"\n");
        }
        std::fs::write(root.join(".ograph/config.toml"), config).unwrap();
        root
    }

    fn alert(app: &str, name: &str, at: i64) -> String {
        format!(r#"{{"labels":{{"alertname":"{name}","service":"{app}","severity":"critical"}},"startsAt":{at}}}"#)
    }

    fn sparql(root: &Path, q: &str) -> Vec<Vec<String>> {
        let ws = Workspace::open(root).unwrap();
        let rows = crate::sparql::run(&ws.store.snapshot().unwrap(), &crate::sparql::parse(q).unwrap(), None).unwrap();
        serde_json::from_value(serde_json::to_value(&rows).unwrap()["rows"].clone()).unwrap()
    }

    fn harvest(root: &Path, only: Option<&str>) -> Vec<crate::gateway::Outcome> {
        Workspace::open(root).unwrap().harvest(only, |_, _| {}).unwrap()
    }

    fn violations(root: &Path) -> usize {
        let ws = Workspace::open(root).unwrap();
        crate::gateway::audit(&ws.store.snapshot().unwrap(), &ws.ontology).len()
    }

    const VERDICT_ROOT: &str = "SELECT ?root WHERE { ?v rca:onChain ?c . ?c corr:rootCause ?root }";

    #[test]
    fn a_verdict_outlives_the_telemetry_of_its_cause() {
        let root = fixture("outlives", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", true, "oncall", &["PM-1".into()]).unwrap();
        std::fs::write(root.join("alerts.json"), "[]").unwrap();
        let out = harvest(&root, None);
        let incidents = out.iter().find(|o| o.graph.contains("/incidents/")).unwrap();
        assert_eq!(incidents.quarantined.len(), 0);
        assert_eq!(violations(&root), 0);
        assert_eq!(sparql(&root, "SELECT ?app WHERE { ?i inc:id \"INC-1\" ; inc:cause ?e . ?e evt:on ?app }"), [["id:app/ledger-db"]]);
    }

    #[test]
    fn a_verdict_stays_on_the_root_it_judged() {
        let alerts = |extra: &str| format!("[{}{extra}]", alert("payments-api", "ErrorRate", T - 300));
        let root = fixture("judged", &[("alerts", "alerts.json", &alerts(""))]);
        harvest(&root, None);
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", true, "oncall", &[]).unwrap();
        let judged = sparql(&root, VERDICT_ROOT);
        assert_eq!(judged[0][0], format!("id:ev/alert-payments-api-errorrate-{}000", T - 300));
        std::fs::write(root.join("alerts.json"), alerts(&format!(",{}", alert("ledger-db", "PoolSaturated", T - 600)))).unwrap();
        harvest(&root, None);
        assert_eq!(sparql(&root, VERDICT_ROOT), judged);
        assert_ne!(sparql(&root, "SELECT ?r WHERE { ?v rca:onChain ?c . ?c corr:rank ?r }"), [["1"]]);
    }

    #[test]
    fn one_harvest_after_removing_a_source_equals_two() {
        let root = fixture("once", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600))), ("deployments", "deployments.jsonl", &format!(r#"{{"service":"payments-api","version":"1.2","at":{}}}"#, T - 300))]);
        harvest(&root, None);
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), config.replace("[[source]]\nkind = \"alerts\"\npath = \"alerts.json\"\n", "")).unwrap();
        let top = "SELECT ?root WHERE { ?c corr:rank 1 ; corr:rootCause ?root }";
        harvest(&root, None);
        let once = (sparql(&root, top), violations(&root));
        harvest(&root, None);
        assert_eq!(once, (sparql(&root, top), violations(&root)));
        assert_eq!(once.1, 0);
    }

    #[test]
    fn a_deleted_file_is_pruned_before_the_files_left_are_checked() {
        let root = fixture("deleted", &[("incidents", "more", "")]);
        std::fs::remove_file(root.join("more")).unwrap();
        std::fs::create_dir(root.join("more")).unwrap();
        std::fs::write(root.join("more/a.json"), format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T},"status":"resolved"}}]"#)).unwrap();
        std::fs::write(root.join("more/b.json"), format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T},"status":"open"}}]"#)).unwrap();
        std::fs::write(root.join("incidents.json"), "[]").unwrap();
        harvest(&root, None);
        std::fs::remove_file(root.join("more/a.json")).unwrap();
        let out = harvest(&root, None);
        assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
        assert_eq!(sparql(&root, "SELECT ?s WHERE { ?i inc:id \"INC-1\" ; inc:status ?s }"), [["open"]]);
    }

    #[test]
    fn an_episode_under_way_before_the_window_is_a_candidate() {
        let mut csv = String::from("time,ledger-db_latency\n");
        for i in 0..480 {
            csv += &format!("{},{}\n", T - 7200 + 15 * i, if i < 320 { 100.0 + (i % 7) as f64 * 0.3 } else { 1000.0 });
        }
        let root = fixture("episode", &[("metrics", "metrics.csv", &csv)]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let before = crate::query::before(&ws.store.snapshot().unwrap(), "INC-1", 600, 4).unwrap();
        assert_eq!(before.events.iter().map(|e| (e.app.as_str(), e.signal.as_str())).collect::<Vec<_>>(), [("ledger-db", "METRIC_ANOMALY")]);
        assert!(before.events[0].source_record.as_deref().is_some_and(|r| r.starts_with("metrics.csv")));
    }

    #[test]
    fn two_applications_claiming_one_alias_get_neither() {
        let root = fixture("alias", &[("ownership", "ownership.jsonl", "{\"app\":\"ledger-db\",\"alias\":\"shared\"}\n{\"app\":\"checkout\",\"alias\":\"shared\"}\n")]);
        harvest(&root, None);
        for _ in 0..2 {
            let out = harvest(&root, Some("ownership"));
            let reasons: Vec<&str> = out[0].quarantined.iter().map(|r| r.reason.as_str()).collect();
            assert_eq!(reasons, ["alias is claimed by id:app/checkout and id:app/ledger-db in one batch"; 2]);
            assert!(sparql(&root, "SELECT ?a WHERE { ?a ops:alias \"shared\" }").is_empty());
        }
    }

    #[test]
    fn config_typos_are_errors_and_kinds_select_sources() {
        let root = fixture("config", &[("config_changes", "config.jsonl", &format!(r#"{{"service":"ledger-db","key":"max_connections","value":"50","at":{}}}"#, T - 60))]);
        assert_eq!(harvest(&root, Some("config_changes")).len(), 1);
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        for typo in ["[corelation]\n", "[correlation]\nwindw_s = 600\n", "[[sources]]\nkind = \"logs\"\npath = \"x\"\n", "[[source]]\nkind = \"logs\"\npth = \"x\"\npath = \"x\"\n", "[detect]\nmetric_zz = 3\n"] {
            std::fs::write(root.join(".ograph/config.toml"), format!("{config}{typo}")).unwrap();
            assert!(Workspace::open(&root).is_err(), "accepted {typo:?}");
        }
    }
}
