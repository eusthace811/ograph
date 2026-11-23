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
use crate::vocab::{self, RDF_TYPE, corr, inc, rca};
use oxrdf::NamedNode;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "source")]
    pub sources: Vec<Source>,
    pub detect: Detect,
    pub correlation: Settings,
    pub llm: Option<LlmSection>,
}

#[derive(Debug, Deserialize)]
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
        let mut out = Vec::new();
        let mut kept = HashSet::new();
        let ran: Vec<&Source> = sources.iter().filter(|s| only.is_none_or(|o| o == s.name())).collect();
        if let Some(o) = only.filter(|o| *o != "correlation" && ran.is_empty()) {
            let mut names: Vec<&str> = sources.iter().map(Source::name).collect();
            names.dedup();
            anyhow::bail!("no source of kind {o:?} is configured; configured kinds: {}, correlation", names.join(", "));
        }
        // A source whose path cannot be listed (a mistyped path, an unmounted disk) reports one
        // failed batch and keeps every graph it had.
        let mut unreadable = Vec::new();
        for src in &ran {
            let p = self.root.join(src.path());
            let batches = if p.is_file() || std::fs::read_dir(&p).is_ok() {
                enrich::harvest(src, &ctx)
            } else {
                unreadable.push(src.name());
                let mut b = Batch::new(src.name(), &src.path().to_string_lossy(), "failed");
                b.degraded.push(format!("cannot read {}", p.display()));
                vec![b]
            };
            for batch in batches {
                let t = std::time::Instant::now();
                let o = gateway::submit_with(&self.store, &self.ontology, &batch, now_ms(), false)?;
                progress(&o, t.elapsed().as_secs_f64());
                kept.extend([batch.graph(), batch.quarantine_graph()]);
                out.push(o);
            }
        }
        // A full harvest owns every harvest graph but the verdicts; `--only` owns its kind's.
        let mut names: Vec<String> = match only {
            None => self.prunable_sources()?,
            Some(_) => ran.iter().map(|s| s.name().to_owned()).collect(),
        };
        names.retain(|n| !unreadable.contains(&n.as_str()));
        if only.is_none_or(|o| o == "correlation") {
            self.store.flush()?;
            for (o, b) in self.correlate_all()? {
                progress(&o, 0.0);
                kept.extend([b.graph(), b.quarantine_graph()]);
                out.push(o);
            }
            if only.is_some() {
                names.push("correlation".to_owned());
            }
        }
        self.prune(&names, &kept)?;
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
    /// renamed, and incidents that no longer have chains.
    fn prune(&self, sources: &[String], kept: &HashSet<NamedNode>) -> anyhow::Result<()> {
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

    /// Writes the chains of every incident, one correlation batch per incident.
    pub fn correlate_all(&self) -> anyhow::Result<Vec<(Outcome, Batch)>> {
        let snap = self.store.snapshot()?;
        let (Ok(ids), Some(c)) = (correlate::Ids::new(&snap), snap.iri(inc::INCIDENT)?) else { return Ok(vec![]) };
        let incidents = snap.subjects(ids.ty, c)?;
        let mut batches = Vec::new();
        for i in incidents {
            let Ok(a) = correlate::analyze(&snap, &ids, i, &self.config.correlation) else { continue };
            batches.push(chain_batch(&a));
        }
        drop(snap);
        batches.into_iter().map(|b| Ok((gateway::submit(&self.store, &self.ontology, &b, now_ms())?, b))).collect()
    }

    /// Records a human verdict on a chain. An accepted chain also writes `inc:cause`.
    pub fn verdict(&self, chain: &str, accept: bool, by: &str, evidence: &[String]) -> anyhow::Result<Outcome> {
        let _write = self.store.write_lock()?;
        let snap = self.store.snapshot()?;
        let chain_iri = resolve_iri(chain);
        let cid = snap.iri(&chain_iri)?.ok_or_else(|| anyhow::anyhow!("no chain {chain}"))?;
        let leads = snap.iri(corr::LEADS_TO)?.and_then(|p| snap.object(cid, p).ok().flatten());
        let root = snap.iri(corr::ROOT_CAUSE)?.and_then(|p| snap.object(cid, p).ok().flatten());
        let term_iri = |id: Option<u64>| id.and_then(|i| match snap.term(i).ok()? {
            oxrdf::Term::NamedNode(n) => Some(n),
            _ => None,
        });
        let (incident, cause) = (term_iri(leads), term_iri(root));
        drop(snap);
        let local = chain_iri.rsplit('/').next().unwrap_or("chain").to_owned();
        let mut b = Batch::new("verdicts", &local, "human");
        let m = Extraction::Human { who: by.to_owned() };
        let ev = b.record(format!("verdict:{local}"));
        let v = known("verdict", &local);
        b.add(v.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(rca::VERDICT)), &ev, &m);
        b.add(v.clone(), rca::ON_CHAIN, obj(NamedNode::new_unchecked(&chain_iri)), &ev, &m);
        b.add(v.clone(), rca::STATE, lit(if accept { "accepted" } else { "rejected" }), &ev, &m);
        b.add(v.clone(), rca::BY, lit(by), &ev, &m);
        b.add(v.clone(), rca::AT, crate::enrich::dt(now_ms()), &ev, &m);
        for e in if evidence.is_empty() { vec!["(none given)".to_owned()] } else { evidence.to_vec() } {
            b.add(v.clone(), rca::EVIDENCE, lit(e.as_str()), &ev, &m);
        }
        if let (true, Some(i), Some(c)) = (accept, incident, cause) {
            b.add(EntityRef::Known(i), inc::CAUSE, obj(c), &ev, &m);
        }
        gateway::submit(&self.store, &self.ontology, &b, now_ms())
    }
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
        let node = known("chain", &format!("{local}/{}", rank + 1));
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
