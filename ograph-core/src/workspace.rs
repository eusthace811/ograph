//! A workspace is a `.ograph/` directory: `config.toml`, `store.redb`, the ontology and shapes
//! (copied in by `init`, so they are versioned with the data), and the model cache.

use crate::correlate::{self, Settings};
use anyhow::Context;
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
    pub verdict: VerdictPolicy,
    pub incidents: IncidentsSection,
    pub llm: Option<LlmSection>,
    /// How many days of removed facts `ograph compact` keeps for `--known-at` and `changes`.
    pub history_days: Option<u32>,
}

pub const HISTORY_DAYS: u32 = 90;

/// What evidence a verdict must cite. The defaults take any verdict, and one without evidence
/// is recorded with the evidence "(none given)".
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct VerdictPolicy {
    pub require_evidence: bool,
    /// Regular expressions; at least one piece of evidence must match one of them.
    pub evidence_patterns: Vec<String>,
    /// At least one piece of evidence must name something in the graph: an incident, an
    /// event, a document or any other node `ograph why` finds.
    pub evidence_in_store: bool,
}

impl VerdictPolicy {
    fn patterns(&self) -> anyhow::Result<Vec<regex::Regex>> {
        self.evidence_patterns.iter().map(|p| regex::Regex::new(p).map_err(|e| anyhow::anyhow!("[verdict] evidence_patterns: {p:?} is not a regular expression: {e}"))).collect()
    }

    /// The evidence a verdict records, or why the policy refuses it.
    fn admit(&self, snap: &crate::store::Snapshot, evidence: &[String]) -> anyhow::Result<Vec<String>> {
        let given: Vec<String> = evidence.iter().map(|e| e.trim()).filter(|e| !e.is_empty()).map(str::to_owned).collect();
        let shown = || if given.is_empty() { "no --evidence was given".to_owned() } else { format!("the evidence given was {}", given.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>().join(", ")) };
        anyhow::ensure!(!self.require_evidence || !given.is_empty(), "verdict not recorded: [verdict] require_evidence is set and no --evidence was given");
        let patterns = self.patterns()?;
        if !patterns.is_empty() && !given.iter().any(|e| patterns.iter().any(|p| p.is_match(e))) {
            anyhow::bail!("verdict not recorded: no evidence matches [verdict] evidence_patterns ({}); {}", self.evidence_patterns.join(", "), shown());
        }
        if self.evidence_in_store && !given.iter().any(|e| crate::query::lookup(snap, e).is_ok()) {
            anyhow::bail!("verdict not recorded: [verdict] evidence_in_store is set and no evidence names anything in the graph; {}. Cite an incident, event or document as `ograph why` names it", shown());
        }
        Ok(if given.is_empty() { vec!["(none given)".to_owned()] } else { given })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct IncidentsSection {
    pub merge: enrich::records::Merge,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    pub extract: Option<LlmConfig>,
}

pub const DEFAULT_CONFIG: &str = r#"# ograph configuration. Paths are relative to the directory holding .ograph/.

# Days of removed facts that `ograph compact` keeps, for --known-at and `ograph changes`.
# A top-level key: keep it above the first [[source]].
history_days = 90

# One [[source]] per input; harvest runs them in a fixed layer order whatever the order here.

# [[source]]
# kind = "catalog"        # runs `ogrep --entities . --json` in this repository
# path = "."
# [[source]]
# kind = "incidents"      # JSON or JSON lines: id, title, service, opened, severity, status
# path = "exports/incidents.json"
# Other kinds: ownership, docs, deployments, config_changes, code, alerts, logs, metrics,
# traces, errors, discussions, news. See docs/REFERENCE.md for each format.

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

/// `.ograph/<name>`, or `default` when there is no such file. Any other failure to read it is
/// an error: a workspace must not run on the built-in ontology because its own is unreadable.
pub fn read_setting(dir: &Path, name: &str, default: &str) -> anyhow::Result<String> {
    let path = dir.join(name);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default.to_owned()),
        r => r.with_context(|| path.display().to_string()),
    }
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

/// The file under `.ograph/` that holds when the last full harvest started, in Unix ms, on its
/// first line, then the files it read, one per line relative to the workspace root, each with its
/// modification time in Unix ms and its size, tab-separated, as they were when it began.
pub const HARVESTED: &str = "harvested";

pub use crate::store::now_ms;

/// A time as `--known-at` and `--since` take it: an `xsd:dateTime` with a zone, or how long ago.
pub fn when_ms(s: &str) -> anyhow::Result<i64> {
    if let Some(ms) = term::instant_millis(s) {
        return Ok(ms);
    }
    // A well-formed duration keeps its own error (`longer than a century`); anything else is told
    // both forms it may take.
    let duration_like = s.starts_with(|c: char| c.is_ascii_digit()) && !s.contains(['-', ':', 'T']);
    let ago = crate::query::duration_s(s).map_err(|e| if duration_like { e } else { anyhow::anyhow!("{s:?} is not a time: write an xsd:dateTime with a zone, such as 2026-11-25T09:30:00Z, or how long ago, such as 90m or 2d") })?;
    Ok(now_ms() - ago * 1000)
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
        Self::open_with(root, false)
    }

    /// For commands that only read: the store opens without writing its file when it can.
    pub fn open_reading(root: &Path) -> anyhow::Result<Self> {
        Self::open_with(root, true)
    }

    fn open_with(root: &Path, read_only: bool) -> anyhow::Result<Self> {
        let dir = root.join(".ograph");
        anyhow::ensure!(dir.is_dir(), "no .ograph/ in {}; run `ograph init`", root.display());
        let config_path = dir.join("config.toml");
        let config: Config = std::fs::read_to_string(&config_path)
            .map_err(anyhow::Error::from)
            .and_then(|t| Ok(toml::from_str(&t)?))
            .with_context(|| config_path.display().to_string())?;
        config.correlation.check()?;
        config.verdict.patterns()?;
        config.incidents.merge.check()?;
        let ontology = Ontology::load(&read_setting(&dir, "ontology.ttl", crate::ontology::DEFAULT_TTL)?, &read_setting(&dir, "shapes.ttl", crate::ontology::DEFAULT_SHAPES)?)?;
        let path = dir.join("store.redb");
        // A store from before the history tables cannot be read until an open has added them.
        let store = match read_only.then(|| Store::open_read_only(&path)) {
            Some(Ok(s)) if s.snapshot().is_ok() => s,
            _ => Store::open(&path)?,
        };
        Ok(Self { root: root.to_owned(), dir, config, ontology, store })
    }

    pub fn history_days(&self) -> u32 {
        self.config.history_days.unwrap_or(HISTORY_DAYS)
    }

    /// The store as it was at `when`, and a line that says what the view hides, what it brings
    /// back and how far back it can go. A time before the history starts is an error, since the
    /// answer would silently lack what was removed before then. A time written to the second
    /// may name the second the history starts in.
    pub fn known_at(&self, when: &str) -> anyhow::Result<(crate::store::Snapshot, String)> {
        let snap = self.store.snapshot()?;
        let (t, since) = self.history_time(&snap, when)?;
        let snap = snap.known_at(t)?;
        let (hidden, restored) = snap.as_of_counts().unwrap_or_default();
        let note = format!(
            "as known at {}: {hidden} quads recorded since are hidden, {restored} removed since are restored; the history starts at {}, and only episodes a harvest recorded and the history still holds are seen",
            term::format_millis(t),
            term::format_millis(since)
        );
        Ok((snap, note))
    }

    /// `when` in milliseconds, and when the history of `snap` starts; an error when the history
    /// does not reach back to `when`.
    pub fn history_time(&self, snap: &crate::store::Snapshot, when: &str) -> anyhow::Result<(i64, i64)> {
        let t = when_ms(when)?;
        let since = snap.history_since()?.ok_or_else(|| anyhow::anyhow!("this store has no history yet; it starts with the next harvest"))?;
        anyhow::ensure!(
            t >= since || (t.rem_euclid(1000) == 0 && t == since - since.rem_euclid(1000)),
            "no record of what was known at {}: the history of this store starts at {} (history_days = {})",
            term::format_millis(t),
            term::format_millis(since),
            self.history_days()
        );
        Ok((t, since))
    }

    pub fn llm(&self) -> Option<Llm> {
        let cfg = self.config.llm.as_ref()?.extract.clone()?;
        Some(Llm::new(cfg, self.dir.join("cache").join("llm.jsonl")))
    }

    /// Every configured `code` repository, as an absolute path.
    fn code_repos(&self) -> Vec<PathBuf> {
        self.config.sources.iter().filter(|s| matches!(s, Source::Code { .. })).map(|s| self.root.join(s.path())).collect()
    }

    /// The repositories whose operations files and history ogrep can read: every `catalog` and
    /// `code` source, in configuration order.
    fn service_repos(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for s in self.config.sources.iter().filter(|s| matches!(s, Source::Catalog { .. } | Source::Code { .. })) {
            let p = self.root.join(s.path());
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    /// `ograph code`: what the graph holds about the code behind an incident, then what ogrep
    /// finds for the incident's description in the code repositories.
    pub fn code(&self, snap: &crate::store::Snapshot, incident: &str, top: usize) -> anyhow::Result<crate::query::Code> {
        let (mut answer, text) = crate::query::code(snap, incident, &self.config.correlation)?;
        crate::query::code_search(&mut answer, &text, &self.root, &self.code_repos(), top);
        crate::query::code_history(&mut answer, &self.root, &self.code_repos());
        Ok(answer)
    }

    /// `ograph suspects`: the failing applications `target` names, then what ogrep ranks as
    /// their likely causes since `since` (by default the day before an incident, or 24 hours).
    /// The second value says why ogrep gave nothing.
    pub fn suspects(&self, snap: &crate::store::Snapshot, target: &str, since: Option<&str>, st: &Settings) -> anyhow::Result<(Vec<crate::query::Suspect>, Vec<String>)> {
        let failing = crate::query::failing(snap, target, st)?;
        Ok(crate::query::suspects(&self.root, &self.service_repos(), &failing.apps, since.unwrap_or(&failing.since), failing.until.as_deref()))
    }

    /// What `ograph chains` falls back to when telemetry ranked no application: the suspects
    /// for the incident. Only with a `code` source, so a workspace without one answers, and
    /// spawns, as it always did.
    pub fn fallback_suspects(&self, snap: &crate::store::Snapshot, analysis: &correlate::Analysis, incident: &str, st: &Settings) -> (Vec<crate::query::Suspect>, Vec<String>) {
        if !analysis.apps.is_empty() || self.code_repos().is_empty() {
            return (vec![], vec![]);
        }
        self.suspects(snap, incident, None, st).unwrap_or_else(|e| (vec![], vec![format!("{e:#}")]))
    }

    /// Runs every source in layer order, then correlation for every incident. Each source file
    /// is one batch through the gateway.
    pub fn harvest(&self, only: Option<&str>, mut progress: impl FnMut(&Outcome, f64)) -> anyhow::Result<Vec<Outcome>> {
        let _write = self.store.write_lock()?;
        let started = now_ms();
        let read = only.is_none().then(|| crate::status::inputs(self));
        let llm = self.llm();
        let code = self.code_repos();
        let mut sources = self.config.sources.clone();
        sources.sort();
        let ran: Vec<&Source> = sources.iter().filter(|s| only.is_none_or(|o| o == s.name() || o == kind(s))).collect();
        if let Some(o) = only.filter(|o| *o != "correlation" && ran.is_empty()) {
            let mut names: Vec<&str> = sources.iter().map(kind).collect();
            names.dedup();
            anyhow::bail!("no source of kind {o:?} is configured; configured kinds: {}, correlation", names.join(", "));
        }
        let graphs = self.graph_names()?;
        // A full harvest owns every harvest graph but the verdicts; `--only` owns its kind's.
        let mut names: Vec<String> = match only {
            None => prunable_sources(&graphs),
            Some(_) => ran.iter().map(|s| s.name().to_owned()).collect(),
        };
        // Any input can move a chain (events, incidents, the dependency scope), and correlation
        // is cheap, so every harvest re-correlates.
        if only.is_some() {
            names.push("correlation".to_owned());
        }
        let (chains, inputs): (Vec<String>, Vec<String>) = names.into_iter().partition(|n| n == "correlation");
        let mut kept: HashSet<NamedNode> = HashSet::new();
        let mut kept_below: Vec<String> = Vec::new();
        let mut out = Vec::new();
        let mut catalog = None;
        let mut apps = Vec::new();
        // The sources that name applications commit first, so the enrichers after them read the
        // catalog this harvest leaves, and one harvest asks the model what two would.
        for naming in [true, false] {
            if !naming {
                apps = gateway::applications(&self.store.snapshot()?)?;
            }
            let ctx = Ctx { root: &self.root, detect: &self.config.detect, llm: llm.as_ref(), ontology: &self.ontology, code: &code, merge: &self.config.incidents.merge, apps: &apps };
            let layer: Vec<&Source> = ran.iter().copied().filter(|s| gateway::NAMING_SOURCES.contains(&s.name()) == naming).collect();
            let batches = self.enrich(&layer, &graphs, &ctx)?;
            kept.extend(batches.iter().flat_map(|b| [b.graph(), b.quarantine_graph()]));
            kept_below.extend(batches.iter().filter(|b| b.failed()).flat_map(|b| [b.graph(), b.quarantine_graph()].map(|g| format!("{}/", g.as_str()))));
            // Input graphs whose files are gone are pruned before any batch or correlation reads
            // them, and chains after correlation writes them, so one harvest leaves the store as
            // two would.
            let owned: Vec<String> = inputs.iter().filter(|n| gateway::NAMING_SOURCES.contains(&n.as_str()) == naming).cloned().collect();
            self.prune(&owned, &kept, &kept_below)?;
            for batch in batches {
                let t = std::time::Instant::now();
                let o = gateway::submit_with(&self.store, &self.ontology, &batch, now_ms(), &mut catalog)?;
                progress(&o, t.elapsed().as_secs_f64());
                out.push(o);
            }
        }
        for (o, b) in self.correlate_all()? {
            progress(&o, 0.0);
            kept.extend([b.graph(), b.quarantine_graph()]);
            out.push(o);
        }
        self.prune(&chains, &kept, &kept_below)?;
        if let Some(read) = read {
            let lines: Vec<String> = std::iter::once(started.to_string()).chain(read.iter().map(crate::status::marker_line)).collect();
            std::fs::write(self.dir.join(HARVESTED), lines.join("\n") + "\n")?;
        }
        Ok(out)
    }

    /// The batches of `ran`. A source whose path cannot be listed (a mistyped path, an unmounted
    /// disk), or a directory that lists no files while the store holds graphs under it (an empty
    /// mount point), reports one failed batch and keeps every graph under its path.
    fn enrich(&self, ran: &[&Source], graphs: &[NamedNode], ctx: &Ctx<'_>) -> anyhow::Result<Vec<Batch>> {
        let mut batches = Vec::new();
        // Every incident source is one batch, so an incident's fields merge across all of them.
        let incidents: Vec<(&Path, bool)> = ran.iter().filter_map(|s| match s { Source::Incidents { path, classify } => Some((path.as_path(), *classify)), _ => None }).collect();
        let mut incidents_done = false;
        for src in ran {
            if let Source::Incidents { .. } = src {
                if !std::mem::replace(&mut incidents_done, true) {
                    batches.push(enrich::incidents(&incidents, &self.incident_files()?, ctx));
                }
                continue;
            }
            let p = self.root.join(src.path());
            let failed = |why: String| {
                let mut b = Batch::new(src.name(), &enrich::rel(&self.root, &p), "failed");
                b.degraded.push(why);
                b
            };
            if !p.is_file() && std::fs::read_dir(&p).is_err() {
                batches.push(failed(format!("cannot read {}", p.display())));
                continue;
            }
            let found = enrich::harvest(src, ctx);
            let held = || {
                let key = failed(String::new());
                let below = [key.graph(), key.quarantine_graph()].map(|g| format!("{}/", g.as_str()));
                graphs.iter().any(|n| below.iter().any(|b| n.as_str().starts_with(b.as_str())))
            };
            if found.is_empty() && p.is_dir() && held() {
                batches.push(failed(format!("{} lists no files; keeping what it held", p.display())));
            } else {
                batches.extend(found);
            }
        }
        Ok(batches)
    }

    /// The files the last incidents harvest read.
    fn incident_files(&self) -> anyhow::Result<Vec<String>> {
        let snap = self.store.snapshot()?;
        let graph = Batch::new("incidents", "all", "").graph();
        let (Some(g), Some(p)) = (snap.iri(graph.as_str())?, snap.iri(crate::vocab::harv::FILE)?) else { return Ok(vec![]) };
        let mut out = Vec::new();
        for o in snap.objects(g, p)? {
            if let oxrdf::Term::Literal(l) = snap.term(o)? {
                out.push(l.value().to_owned());
            }
        }
        Ok(out)
    }

    fn graph_names(&self) -> anyhow::Result<Vec<NamedNode>> {
        let snap = self.store.snapshot()?;
        let mut out = Vec::new();
        for (g, _) in snap.graphs()? {
            if let oxrdf::Term::NamedNode(n) = snap.term(g)? {
                out.push(n);
            }
        }
        Ok(out)
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
            self.store.transact(|tx| stale.into_iter().try_for_each(|g| tx.clear_graph(g).map(drop)))?;
        }
        Ok(())
    }

    /// Writes the chains of every incident, one correlation batch per incident.
    fn correlate_all(&self) -> anyhow::Result<Vec<(Outcome, Batch)>> {
        let snap = self.store.snapshot()?;
        let (Ok(ids), Some(c)) = (correlate::Ids::new(&snap), snap.iri(inc::INCIDENT)?) else { return Ok(vec![]) };
        let incidents = snap.subjects(ids.ty, c)?;
        let mut batches = Vec::new();
        let mut sigs = crate::query::Signatures::default();
        for i in incidents {
            // Stored chains carry ranks, not marks, so the past is read only when it can move a rank.
            let st = &self.config.correlation;
            let past = if st.precedent_weight > 0.0 { crate::query::past(&snap, &ids, i, st, &mut sigs)? } else { vec![] };
            let Ok(a) = correlate::analyze(&snap, &ids, i, st, &past) else { continue };
            batches.push(chain_batch(&a, st));
        }
        drop(snap);
        let mut catalog = None;
        batches.into_iter().map(|b| Ok((gateway::submit_with(&self.store, &self.ontology, &b, now_ms(), &mut catalog)?, b))).collect()
    }

    /// Records a human verdict on a chain. The verdict keeps what it judged (the chain's
    /// incident and root event, and on the verdict node the root's class, application, time,
    /// signal and the rank the chain had), so it means the same after the telemetry behind the
    /// root is retired or the chain's rank changes. The root event itself gets nothing: its
    /// type, time and application stay with its telemetry, so a retired root is no candidate for
    /// correlation.
    ///
    /// Judging a chain again adds a verdict and marks the one before `rca:supersededBy` it; the
    /// chain's graph holds every verdict, and only the current one writes `inc:cause`.
    pub fn verdict(&self, chain: &str, accept: bool, by: &str, evidence: &[String]) -> anyhow::Result<Outcome> {
        anyhow::ensure!(!by.trim().is_empty(), "a verdict needs --by: who judged it");
        let _write = self.store.write_lock()?;
        let snap = self.store.snapshot()?;
        let cid = find_chain(&snap, chain)?;
        let evidence = self.config.verdict.admit(&snap, evidence)?;
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
        let local = chain_iri.as_str().rsplit('/').next().unwrap_or("chain").to_owned();
        let mut b = Batch::new("verdicts", &local, "human");
        let history = crate::query::verdicts(&snap, cid)?;
        let mut earlier = Vec::new();
        if let Some(g) = snap.iri(b.graph().as_str())? {
            for &v in &history {
                for q in snap.quads([Some(v), None, None], crate::store::GraphSel::Named(g)) {
                    let [s, p, o, _] = q?;
                    if let (oxrdf::Term::NamedNode(s), oxrdf::Term::NamedNode(p)) = (snap.term(s)?, snap.term(p)?) {
                        earlier.push((s, p, snap.term(o)?));
                    }
                }
            }
        }
        // From the root's telemetry while it lasts, else from the current verdict.
        let latest = history.last().copied();
        let live = object(root_id, evt::AT)?.is_some();
        let class_of = |e: u64| -> anyhow::Result<Option<oxrdf::Term>> {
            let Some(ty) = snap.iri(RDF_TYPE)? else { return Ok(None) };
            let classes = snap.objects(e, ty)?.into_iter().map(|c| snap.term(c)).collect::<Result<Vec<_>, _>>()?;
            Ok(classes.into_iter().find(|c| !matches!(c, oxrdf::Term::NamedNode(n) if n.as_str() == evt::EVENT)))
        };
        let mut kept = Vec::new();
        for (p, q) in [(RDF_TYPE, rca::ROOT_CLASS), (evt::ON, rca::ROOT_ON), (evt::AT, rca::ROOT_AT), (evt::SIGNAL, rca::ROOT_SIGNAL)] {
            let o = match (live, latest) {
                (true, _) if p == RDF_TYPE => class_of(root_id)?,
                (true, _) => object(root_id, p)?,
                (false, Some(v)) => object(v, q)?,
                (false, None) => None,
            };
            if let Some(o) = o {
                kept.push((q, o));
            }
        }
        for (p, q) in [(corr::RANK, rca::RANK), (corr::SETTINGS, rca::SETTINGS)] {
            if let Some(o) = object(cid, p)? {
                kept.push((q, o));
            }
        }
        let previous = latest.map(|v| snap.term(v)).transpose()?;
        drop(snap);
        // The verdict describes its root so the root can leave the sources; without a
        // description there is nothing to keep.
        if !kept.iter().any(|(q, _)| *q == rca::ROOT_CLASS) {
            anyhow::bail!("the root of {chain} has no telemetry left and no earlier verdict; run `ograph harvest` and judge a current chain");
        }
        let m = Extraction::Human { who: by.to_owned() };
        let ev = b.record(format!("verdict:{local}"));
        let term_obj = |o: oxrdf::Term| match o {
            oxrdf::Term::NamedNode(n) => Some(obj(n)),
            oxrdf::Term::Literal(l) => Some(lit(l)),
            _ => None,
        };
        for (s, p, o) in earlier {
            if let Some(o) = term_obj(o) {
                b.add(EntityRef::Known(s), p.as_str(), o, &ev, &m);
            }
        }
        let n = history.len() + 1;
        let v = known("verdict", &if n == 1 { local } else { format!("{local}-v{n}") });
        if let (Some(oxrdf::Term::NamedNode(p)), EntityRef::Known(next)) = (previous, &v) {
            b.add(EntityRef::Known(p), rca::SUPERSEDED_BY, obj(next.clone()), &ev, &m);
        }
        b.add(v.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(rca::VERDICT)), &ev, &m);
        b.add(v.clone(), rca::ON_CHAIN, obj(chain_iri.clone()), &ev, &m);
        b.add(v.clone(), rca::STATE, lit(if accept { "accepted" } else { "rejected" }), &ev, &m);
        b.add(v.clone(), rca::BY, lit(by), &ev, &m);
        b.add(v.clone(), rca::AT, crate::enrich::dt(now_ms()), &ev, &m);
        for e in evidence {
            b.add(v.clone(), rca::EVIDENCE, lit(e.as_str()), &ev, &m);
        }
        let c = EntityRef::Known(chain_iri);
        b.add(c.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(corr::CHAIN)), &ev, &m);
        b.add(c.clone(), corr::LEADS_TO, obj(incident.clone()), &ev, &m);
        b.add(c, corr::ROOT_CAUSE, obj(root.clone()), &ev, &m);
        for (p, o) in kept {
            if let Some(o) = term_obj(o) {
                b.add(v.clone(), p, o, &ev, &m);
            }
        }
        if accept {
            b.add(EntityRef::Known(incident), inc::CAUSE, obj(root), &ev, &m);
        }
        // A verdict replaces its graph, so one the gateway would cut short is refused instead.
        let snap = self.store.snapshot()?;
        let replaced = snap.iri(b.graph().as_str())?;
        let (_, rejected, _) = gateway::check(&snap, &self.ontology, &b, replaced, &gateway::Catalog::from_snapshot(&snap, replaced)?);
        drop(snap);
        if !rejected.is_empty() {
            let why: Vec<String> = rejected.iter().map(|r| format!("{} ({})", r.fact, r.reason)).collect();
            anyhow::bail!("verdict not recorded: the ontology refuses {} of its facts: {}; run `ograph ontology check`", rejected.len(), why.join("; "));
        }
        gateway::submit(&self.store, &self.ontology, &b, now_ms())
    }
}

/// Every source name with a graph in `graphs`, except the verdicts people recorded.
fn prunable_sources(graphs: &[NamedNode]) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for n in graphs {
        if let Some(rest) = n.as_str().strip_prefix(vocab::GRAPH)
            && let Some(rest) = rest.strip_prefix("harvest/").or_else(|| rest.strip_prefix("quarantine/"))
            && let Some((source, _)) = rest.split_once('/')
            && source != "verdicts"
        {
            names.insert(source.to_owned());
        }
    }
    names.into_iter().collect()
}

/// A source's kind as `config.toml` names it, which `--only` accepts besides its graph name.
fn kind(s: &Source) -> &'static str {
    match s {
        Source::ConfigChanges { .. } => "config_changes",
        _ => s.name(),
    }
}

/// A chain by IRI, or `id:chain/<incident>-<rank>` for the chain at that rank now. A name in
/// the rank form that is not a chain, such as a chain id from before ids were stable, means
/// the rank.
pub(crate) fn find_chain(snap: &crate::store::Snapshot, chain: &str) -> anyhow::Result<u64> {
    let iri = resolve_iri(chain);
    let missing = || anyhow::anyhow!("no chain {chain}; `ograph chains INCIDENT` lists them");
    let leads = snap.iri(corr::LEADS_TO)?.ok_or_else(missing)?;
    if let Some(id) = snap.iri(&iri)?
        && snap.object(id, leads)?.is_some()
    {
        return Ok(id);
    }
    let local = iri.strip_prefix(&format!("{}chain/", vocab::DATA)).ok_or_else(missing)?;
    let (incident, rank) = local.rsplit_once('-').ok_or_else(missing)?;
    let rank: i64 = rank.parse().map_err(|_| missing())?;
    let (Some(rank_p), Some(i), Some(r)) = (
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

/// The best chain of each of the first `max` ranked applications as a correlation batch.
fn chain_batch(a: &correlate::Analysis, st: &Settings) -> Batch {
    let local = a.incident.rsplit('/').next().unwrap_or("inc").to_owned();
    let mut b = Batch::new("correlation", &local, "correlator-v1");
    let m = Extraction::Rule { extractor: "correlator-v1".into() };
    let ev = b.record(format!("query:chains {local}"));
    let settings = st.hash();
    for (rank, c) in a.apps.iter().take(st.max_chains).enumerate().filter_map(|(k, r)| Some((k, r.chain.as_ref()?))) {
        let node = EntityRef::Known(NamedNode::new_unchecked(&c.id));
        b.add(node.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(corr::CHAIN)), &ev, &m);
        b.add(node.clone(), corr::LEADS_TO, obj(NamedNode::new_unchecked(&a.incident)), &ev, &m);
        b.add(node.clone(), corr::ROOT_CAUSE, obj(NamedNode::new_unchecked(&c.root.iri)), &ev, &m);
        b.add(node.clone(), corr::CANDIDATE_APP, obj(NamedNode::new_unchecked(&c.root.app_iri)), &ev, &m);
        for l in &c.links {
            b.add(node.clone(), corr::LINK, obj(NamedNode::new_unchecked(&l.iri)), &ev, &m);
        }
        b.add(node.clone(), corr::CONFIDENCE, lit(term::decimal(c.confidence)), &ev, &m);
        b.add(node.clone(), corr::RANK, lit(oxrdf::Literal::from(rank as i64 + 1)), &ev, &m);
        b.add(node, corr::SETTINGS, lit(settings.as_str()), &ev, &m);
    }
    b
}

// These tests use shell scripts as stand-ins for ogrep and the model command.
#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::Workspace;
    use std::path::{Path, PathBuf};

    pub(crate) const T: i64 = 1_795_386_400;

    /// A workspace whose catalog is checkout -> payments-api -> ledger-db, with an incident on
    /// checkout at `T` and `sources` (kind, file, body) beside it.
    pub(crate) fn fixture(name: &str, sources: &[(&str, &str, &str)]) -> PathBuf {
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

    pub(crate) fn alert(app: &str, name: &str, at: i64) -> String {
        format!(r#"{{"labels":{{"alertname":"{name}","service":"{app}","severity":"critical"}},"startsAt":{at}}}"#)
    }

    fn sparql(root: &Path, q: &str) -> Vec<Vec<String>> {
        let ws = Workspace::open(root).unwrap();
        let rows = crate::sparql::run(&ws.store.snapshot().unwrap(), &crate::sparql::parse(q).unwrap(), None).unwrap();
        let rows: Vec<Vec<Option<String>>> = serde_json::from_value(serde_json::to_value(&rows).unwrap()["rows"].clone()).unwrap();
        rows.into_iter().map(|r| r.into_iter().map(Option::unwrap_or_default).collect()).collect()
    }

    pub(crate) fn harvest(root: &Path, only: Option<&str>) -> Vec<crate::gateway::Outcome> {
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
        assert_eq!(sparql(&root, "SELECT ?app WHERE { ?i inc:id \"INC-1\" ; inc:cause ?e . ?v rca:onChain [ corr:rootCause ?e ] ; rca:rootOn ?app }"), [["id:app/ledger-db"]]);
    }

    #[test]
    fn an_incident_resolved_before_it_opened_keeps_everything_but_the_resolution() {
        let inverted = format!(r#"[{{"id":"INC-2","service":"checkout","opened":{T},"resolved":{},"status":"resolved"}}]"#, T - 3600);
        let root = fixture("inverted", &[("incidents", "more.json", &inverted)]);
        let out = harvest(&root, None);
        let reasons: Vec<&str> = out.iter().flat_map(|o| &o.quarantined).map(|q| q.reason.as_str()).collect();
        assert_eq!(reasons, ["SHACL LessThanOrEqualsConstraintComponent: an incident cannot be resolved before it opened"]);
        assert_eq!(sparql(&root, "SELECT ?status WHERE { ?i inc:id \"INC-2\" ; evt:at ?at ; inc:status ?status FILTER NOT EXISTS { ?i inc:resolvedAt ?r } }"), [["resolved"]]);
        assert_eq!(violations(&root), 0);
    }

    #[test]
    fn similar_keeps_the_cause_of_a_verdict_whose_root_left_the_sources() {
        let root = fixture("similar", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600))), ("deployments", "deployments.jsonl", &format!("{{\"service\":\"payments-api\",\"version\":\"1\",\"at\":{}}}\n{{\"service\":\"payments-api\",\"version\":\"2\",\"at\":{}}}\n", T - 300, T + 86_100))]);
        std::fs::write(root.join("incidents.json"), format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T}}},{{"id":"INC-2","service":"checkout","opened":{}}}]"#, T + 86_400)).unwrap();
        harvest(&root, None);
        let chain = sparql(&root, "SELECT ?c WHERE { ?c corr:leadsTo ?i ; corr:candidateApp id:app\\/ledger-db . ?i inc:id \"INC-1\" }");
        Workspace::open(&root).unwrap().verdict(&chain[0][0], true, "oncall", &[]).unwrap();
        std::fs::write(root.join("alerts.json"), "[]").unwrap();
        harvest(&root, None);
        assert_eq!(violations(&root), 0);
        let ws = Workspace::open(&root).unwrap();
        let similar = crate::query::similar(&ws.store.snapshot().unwrap(), "INC-2", &ws.config.correlation, 5, &mut Default::default()).unwrap();
        assert_eq!(similar.iter().map(|s| (s.incident.as_str(), s.cause_apps.clone())).collect::<Vec<_>>(), [("id:inc/inc-1", vec!["ledger-db".to_owned()])]);
    }

    fn similar_to(root: &Path, incident: &str) -> Vec<(String, f64)> {
        let ws = Workspace::open(root).unwrap();
        crate::query::similar(&ws.store.snapshot().unwrap(), incident, &ws.config.correlation, 5, &mut Default::default()).unwrap().into_iter().map(|s| (s.incident, s.similarity)).collect()
    }

    fn incidents(rows: &[(&str, &str, i64, &str)]) -> String {
        let rows: Vec<String> = rows.iter().map(|(id, app, at, fault)| format!(r#"{{"id":"{id}","service":"{app}","opened":{at},"fault":"{fault}"}}"#)).collect();
        format!("[{}]", rows.join(","))
    }

    #[test]
    fn an_incident_without_telemetry_is_compared_by_what_it_states() {
        let root = fixture("stated", &[]);
        let d = 86_400;
        std::fs::write(root.join("incidents.json"), incidents(&[("INC-1", "checkout", T, "pool"), ("INC-2", "checkout", T + d, "disk"), ("INC-3", "payments-api", T + 2 * d, "pool"), ("INC-4", "checkout", T + 3 * d, "pool")])).unwrap();
        harvest(&root, None);
        assert_eq!(similar_to(&root, "INC-4"), [("id:inc/inc-1".to_owned(), 1.0), ("id:inc/inc-3".to_owned(), 0.5), ("id:inc/inc-2".to_owned(), 0.5)]);
    }

    #[test]
    fn what_an_incident_states_orders_equal_telemetry_matches() {
        let d = 86_400;
        let alerts: Vec<String> = [T, T + d, T + 2 * d].iter().map(|t| alert("ledger-db", "PoolSaturated", t - 600)).collect();
        let root = fixture("fused", &[("alerts", "alerts.json", &format!("[{}]", alerts.join(",")))]);
        std::fs::write(root.join("incidents.json"), incidents(&[("INC-1", "checkout", T, "pool"), ("INC-2", "checkout", T + d, "disk"), ("INC-3", "checkout", T + 2 * d, "pool")])).unwrap();
        harvest(&root, None);
        assert_eq!(similar_to(&root, "INC-3"), [("id:inc/inc-1".to_owned(), 1.0), ("id:inc/inc-2".to_owned(), 1.0)]);
    }

    #[test]
    fn the_model_classifies_titles_and_a_class_it_cannot_quote_is_unknown() {
        let root = fixture("classify", &[]);
        std::fs::write(root.join("incidents.json"), format!(r#"[{{"id":"INC-1","title":"checkout: name not resolved","service":"checkout","opened":{T}}},{{"id":"INC-2","title":"checkout errors","service":"checkout","opened":{}}}]"#, T + 60)).unwrap();
        let model = r#"if grep -q resolved; then echo '{"fault_class":"DNS","quote":"name not resolved","confidence":0.9}'; else echo '{"fault_class":"DISK","quote":"disk full","confidence":0.9}'; fi"#;
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap().replace("path = \"incidents.json\"\n", "path = \"incidents.json\"\nclassify = true\n");
        std::fs::write(root.join(".ograph/config.toml"), format!("{config}[llm.extract]\ncommand = [\"sh\", \"-c\", {model:?}]\nmodel = \"stand-in\"\n")).unwrap();
        harvest(&root, None);
        assert_eq!(sparql(&root, "SELECT ?id ?c WHERE { ?i inc:id ?id ; inc:faultClass ?c } ORDER BY ?id"), [["INC-1", "DNS"], ["INC-2", "UNKNOWN"]]);
        assert_eq!(violations(&root), 0);
        std::fs::write(root.join(".ograph/config.toml"), config).unwrap();
        let out = harvest(&root, None);
        let degraded: Vec<&String> = out.iter().flat_map(|o| &o.degraded).collect();
        assert_eq!(degraded, ["classify = true needs an [llm.extract] model; 2 incident titles have no fault class"]);
    }

    #[test]
    fn a_judged_root_that_left_the_sources_is_no_longer_a_candidate() {
        let root = fixture("retired", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", false, "oncall", &[]).unwrap();
        std::fs::write(root.join("alerts.json"), "[]").unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        assert!(crate::query::before(&ws.store.snapshot().unwrap(), "INC-1", 1800, 4, 1800).unwrap().events.is_empty());
        assert!(sparql(&root, "SELECT ?c WHERE { ?c corr:rank ?r }").is_empty());
        assert_eq!(violations(&root), 0);
    }

    fn chains(root: &Path, known_at: Option<i64>) -> String {
        let ws = Workspace::open(root).unwrap();
        let snap = match known_at {
            Some(t) => ws.known_at(&crate::term::format_millis(t)).unwrap().0,
            None => ws.store.snapshot().unwrap(),
        };
        let ids = crate::correlate::Ids::new(&snap).unwrap();
        let a = crate::correlate::analyze(&snap, &ids, crate::query::lookup(&snap, "INC-1").unwrap(), &ws.config.correlation, &[]).unwrap();
        serde_json::to_string(&a).unwrap()
    }

    fn pause() -> i64 {
        std::thread::sleep(std::time::Duration::from_millis(3));
        let t = super::now_ms();
        std::thread::sleep(std::time::Duration::from_millis(3));
        t
    }

    #[test]
    fn a_rank_shift_names_the_file_that_moved_the_ranking_and_not_the_one_that_did_not() {
        let root = fixture("rank-shift", &[("alerts", "alerts.json", &format!("[{}]", alert("payments-api", "ErrorRate", T - 300))), ("alerts", "more.json", "[]"), ("ownership", "ownership.jsonl", "{\"app\":\"ledger-db\",\"process\":\"billing\"}\n")]);
        harvest(&root, None);
        let t1 = pause();
        std::fs::write(root.join("ownership.jsonl"), "{\"app\":\"ledger-db\",\"process\":\"settlement\"}\n").unwrap();
        harvest(&root, None);
        let t2 = pause();
        std::fs::write(root.join("more.json"), format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600))).unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let shift = |t: i64| crate::query::rank_shift(&ws.store.snapshot().unwrap(), t, "INC-1", &ws.config.correlation, 5).unwrap();
        let apps = |r: &[crate::query::RankedApp]| r.iter().map(|a| a.app.clone()).collect::<Vec<_>>();
        let s = shift(t1);
        assert_eq!((apps(&s.then), apps(&s.now), s.changed_graphs), (vec!["payments-api".to_owned()], vec!["ledger-db".to_owned(), "payments-api".to_owned()], 2));
        assert_eq!(s.causes, [crate::query::RankCause { graph: "g:harvest/alerts/more.json".into(), source: Some("alerts".into()), needed: true, enough: true }]);
        std::fs::write(root.join("more.json"), "[]").unwrap();
        harvest(&root, None);
        let s = shift(t2);
        assert_eq!((apps(&s.then), apps(&s.now), s.changed_graphs, s.causes.len()), (vec!["payments-api".to_owned()], vec!["payments-api".to_owned()], 0, 0));
    }

    #[test]
    fn chains_known_at_an_earlier_time_are_the_chains_of_that_time() {
        let alerts = |more: &str| format!("[{}{more}]", alert("payments-api", "ErrorRate", T - 300));
        let root = fixture("known-at", &[("alerts", "alerts.json", &alerts("")), ("ownership", "ownership.jsonl", "{\"app\":\"ledger-db\",\"process\":\"billing\"}\n")]);
        let t0 = pause();
        harvest(&root, None);
        let then = chains(&root, None);
        assert!(then.contains("alert-payments-api-errorrate"), "{then}");
        let t1 = pause();
        std::fs::write(root.join("alerts.json"), alerts(&format!(",{}", alert("ledger-db", "PoolSaturated", T - 600)))).unwrap();
        std::fs::write(root.join("ownership.jsonl"), "{\"app\":\"ledger-db\",\"process\":\"settlement\"}\n").unwrap();
        harvest(&root, None);
        assert_ne!(chains(&root, None), then);
        assert_eq!(chains(&root, Some(t1)), then);

        let ws = Workspace::open(&root).unwrap();
        let changes = crate::query::changes(&ws.known_at(&crate::term::format_millis(t1)).unwrap().0, &ws.store.snapshot().unwrap()).unwrap();
        let ev = format!("id:ev/alert-ledger-db-poolsaturated-{}000", T - 600);
        let listed: Vec<(&str, &str, &str, &str, Option<&str>)> = changes.iter().map(|c| (c.change, c.subject.as_str(), c.predicate.as_str(), c.object.as_str(), c.was.as_deref())).collect();
        assert_eq!(listed[0], ("changed", "id:app/ledger-db", "ops:supports", "id:process/settlement", Some("id:process/billing")));
        assert!(listed.contains(&("added", ev.as_str(), "evt:on", "id:app/ledger-db", None)), "{listed:?}");
        assert!(listed.iter().all(|c| c.1 == "id:app/ledger-db" || c.1 == "id:process/settlement" || c.1 == "id:process/billing" || c.1 == ev), "{listed:?}");
        assert_eq!((changes[0].source.as_deref(), changes[0].extractor.is_some()), (Some("ownership"), true));

        let too_early = ws.known_at(&crate::term::format_millis(t0)).err().unwrap().to_string();
        assert!(too_early.starts_with("no record of what was known at "), "{too_early}");
    }

    #[test]
    fn an_incident_recorded_after_the_time_asked_about_was_not_known_then() {
        let root = fixture("known-at-later", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        let incidents = std::fs::read_to_string(root.join("incidents.json")).unwrap();
        std::fs::write(root.join("incidents.json"), "[]").unwrap();
        harvest(&root, None);
        let t1 = pause();
        std::fs::write(root.join("incidents.json"), incidents).unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let then = ws.known_at(&crate::term::format_millis(t1)).unwrap().0;
        assert_eq!(crate::query::lookup(&then, "INC-1").err().unwrap().to_string(), "INC-1 was not known at that time: the store recorded it later");
        let shift = crate::query::rank_shift(&ws.store.snapshot().unwrap(), t1, "INC-1", &ws.config.correlation, 5).unwrap();
        let apps = |r: &[crate::query::RankedApp]| r.iter().map(|a| a.app.clone()).collect::<Vec<_>>();
        assert_eq!((apps(&shift.then), apps(&shift.now)), (vec![], vec!["ledger-db".to_owned()]));
    }

    #[test]
    fn a_time_takes_any_xsd_date_time_with_a_zone() {
        let ms = 1_795_442_000_123;
        assert_eq!(super::when_ms("2026-11-23T13:53:20.123Z").unwrap(), ms);
        assert_eq!(super::when_ms("2026-11-23T13:53:20.123456Z").unwrap(), ms);
        assert_eq!(super::when_ms("2026-11-23T14:53:20.123999+01:00").unwrap(), ms);
        assert!(super::when_ms("2026-11-23T13:53:20.123").is_err());
    }

    #[test]
    fn known_at_accepts_the_second_the_history_starts_in() {
        let root = fixture("known-at-second", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let since = ws.store.snapshot().unwrap().history_since().unwrap().unwrap();
        let second = |ms: i64| crate::term::format_millis(ms - ms.rem_euclid(1000));
        assert!(!second(since).contains('.'), "{}", second(since));
        assert!(ws.known_at(&second(since)).is_ok());
        assert!(ws.known_at(&second(since - 1000)).is_err());
        if since.rem_euclid(1000) > 0 {
            assert!(ws.known_at(&crate::term::format_millis(since - 1)).is_err());
        }
    }

    /// The other side of `a_judged_root_that_left_the_sources_is_no_longer_a_candidate`: the
    /// view of a time when the telemetry was held still ranks its root.
    #[test]
    fn a_root_that_left_the_sources_is_a_candidate_as_known_before_it_left() {
        let root = fixture("known-retired", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let then = chains(&root, None);
        let t1 = pause();
        std::fs::write(root.join("alerts.json"), "[]").unwrap();
        harvest(&root, None);
        assert!(!chains(&root, None).contains("poolsaturated"));
        assert_eq!(chains(&root, Some(t1)), then);
    }

    #[test]
    fn the_rank_a_verdict_resolves_is_the_rank_chains_prints() {
        let alerts = [alert("ledger-db", "PoolSaturated", T - 600), alert("ledger-db", "SlowQueries", T - 500), alert("payments-api", "ErrorRate", T - 300)];
        let root = fixture("ranks", &[("alerts", "alerts.json", &format!("[{}]", alerts.join(",")))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let ids = crate::correlate::Ids::new(&snap).unwrap();
        let a = crate::query::chains(&snap, &ids, crate::query::lookup(&snap, "INC-1").unwrap(), &ws.config.correlation, &mut Default::default()).unwrap();
        let printed: Vec<Vec<String>> = a.apps.iter().enumerate().map(|(k, r)| vec![crate::vocab::compact(&r.chain.as_ref().unwrap().id), (k + 1).to_string()]).collect();
        assert_eq!(printed.len(), 2);
        assert_eq!(sparql(&root, "SELECT ?c ?r WHERE { ?c corr:rank ?r } ORDER BY ?r"), printed);
    }

    const HISTORY: &str = "SELECT ?v ?state ?next WHERE { ?v rca:state ?state . OPTIONAL { ?v rca:supersededBy ?next } } ORDER BY ?v";

    #[test]
    fn judging_a_chain_again_keeps_every_verdict_and_the_latest_decides() {
        let root = fixture("history", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let chain = sparql(&root, "SELECT ?c WHERE { ?c corr:rank 1 }")[0][0].clone();
        let v = chain.replace("id:chain/", "id:verdict/");
        let cause = "SELECT ?e WHERE { ?i inc:id \"INC-1\" ; inc:cause ?e }";
        Workspace::open(&root).unwrap().verdict(&chain, true, "oncall", &["PM-1".into()]).unwrap();
        assert_eq!(sparql(&root, cause).len(), 1);
        Workspace::open(&root).unwrap().verdict(&chain, false, "lead", &["rollback changed nothing".into()]).unwrap();
        harvest(&root, None);
        assert_eq!(sparql(&root, HISTORY), [[v.clone(), "accepted".into(), format!("{v}-v2")], [format!("{v}-v2"), "rejected".into(), String::new()]]);
        assert!(sparql(&root, cause).is_empty());
        assert_eq!(violations(&root), 0);
        let ws = Workspace::open(&root).unwrap();
        let why = crate::query::why(&ws.store.snapshot().unwrap(), &chain, Some(1800)).unwrap();
        let seen: Vec<_> = why.verdicts.iter().map(|e| (e.verdict.as_str(), e.state.as_str(), e.by.as_str(), e.evidence.clone(), e.rank.as_deref(), e.superseded_by.as_deref())).collect();
        let v2 = format!("{v}-v2");
        assert_eq!(seen, [(v.as_str(), "accepted", "oncall", vec!["PM-1".to_owned()], Some("1"), Some(v2.as_str())), (v2.as_str(), "rejected", "lead", vec!["rollback changed nothing".to_owned()], Some("1"), None)]);
        ws.verdict(&chain, true, "lead", &["PM-1 rev 2".into()]).unwrap();
        assert_eq!(sparql(&root, cause).len(), 1);
        assert_eq!(sparql(&root, HISTORY).iter().map(|r| r[2].clone()).collect::<Vec<_>>(), [format!("{v}-v2"), format!("{v}-v3"), String::new()]);
    }

    #[test]
    fn uses_lists_what_a_file_feeds_and_the_chains_and_verdicts_that_rest_on_it() {
        let root = fixture("uses", &[
            ("alerts", "db.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600))),
            ("alerts", "api.json", &format!("[{}]", alert("payments-api", "ErrorRate", T - 300))),
            ("incidents", "more.json", &format!(r#"[{{"id":"INC-1","title":"Checkout down"}},{{"id":"INC-2","service":"checkout","opened":{}}}]"#, T + 86_400)),
        ]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let st = &ws.config.correlation;
        let rank1 = sparql(&root, "SELECT ?c WHERE { ?c corr:rank 1 ; corr:leadsTo [ inc:id \"INC-1\" ] }")[0][0].clone();
        ws.verdict(&rank1, true, "oncall", &["PM-1".into()]).unwrap();

        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, st, "db.json").unwrap();
        assert_eq!(u.graphs, ["g:harvest/alerts/db.json"]);
        let events: Vec<_> = u.events.iter().map(|e| (e.app.as_deref(), e.signal.as_str(), e.record.as_deref())).collect();
        assert_eq!(events, [(Some("ledger-db"), "ALERT", Some("db.json#1"))]);
        assert!(u.facts.iter().all(|f| f.also_in.is_empty() && f.graph == "g:harvest/alerts/db.json"));
        let chains: Vec<_> = u.chains.iter().map(|c| (c.change, c.incident.as_str(), c.rank, c.rank_after, c.uses.len())).collect();
        assert_eq!(chains, [("gone", "INC-1", Some(1), None, 1), ("moved", "INC-1", Some(2), Some(1), 0)]);
        assert_eq!(u.chains[0].chain, rank1);
        let verdicts: Vec<_> = u.verdicts.iter().map(|v| (v.chain.as_str(), v.state.as_str(), v.by.as_str())).collect();
        assert_eq!(verdicts, [(rank1.as_str(), "accepted", "oncall")]);

        // INC-1 is stated in both incident files, so it stays without one; INC-2 only in more.json.
        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, st, "more.json").unwrap();
        assert_eq!(u.shared, ["id:inc/inc-1"]);
        assert_eq!(u.events.iter().map(|e| e.node.as_str()).collect::<Vec<_>>(), ["id:inc/inc-2"]);
        let inc1: Vec<_> = u.facts.iter().filter(|f| f.subject == "id:inc/inc-1").map(|f| (f.predicate.as_str(), f.object.as_str(), f.also_in.is_empty())).collect();
        assert_eq!(inc1, [("evt:sourceRecord", "more.json#1", true), ("inc:title", "Checkout down", true)]);
        assert!(u.chains.iter().all(|c| c.incident == "INC-2"));

        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, st, "incidents.json#1").unwrap();
        // Without its service and time, INC-1 is quarantined, so it goes with its chains.
        assert!(u.shared.is_empty());
        assert_eq!(u.events.iter().map(|e| e.node.as_str()).collect::<Vec<_>>(), ["id:inc/inc-1"]);
        assert_eq!(u.chains.iter().map(|c| (c.change, c.incident.as_str())).collect::<Vec<_>>(), [("gone", "INC-1"), ("gone", "INC-1")]);
        assert!(crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, st, "nothing.json").unwrap().facts.is_empty());
    }

    #[test]
    fn a_discussions_source_asks_for_actions_unless_it_says_actions_false() {
        let thread = format!(r#"[{{"ts":"{}.000100","channel":"ops","text":"INC-1: rolled back checkout to 4.0.9"}}]"#, T - 100);
        let root = fixture("actions-default", &[("discussions", "ops.json", &thread)]);
        let answer = r#"{"category":"UNKNOWN","sentiment":"urgent","apps":["checkout"],"calls":[],"actions":[{"kind":"ROLLBACK","app":"checkout","message":1,"quote":"rolled back checkout"}],"confidence":0.9}"#;
        std::fs::write(root.join("answer.json"), answer).unwrap();
        let cfg = root.join(".ograph/config.toml");
        let model = format!("[llm.extract]\ncommand = [\"sh\", \"-c\", \"cat > '{}'; cat '{}'\"]\nmodel = \"fixture\"\n", root.join("prompt.txt").display(), root.join("answer.json").display());
        let config = std::fs::read_to_string(&cfg).unwrap();
        let actions = "SELECT ?a WHERE { ?a a evt:OperatorAction }";
        let run = |config: &str| {
            std::fs::write(&cfg, config.to_owned() + &model).unwrap();
            let out = harvest(&root, None);
            let degraded = out.iter().find(|o| o.graph.contains("/discussions/")).unwrap().degraded.clone();
            (std::fs::read_to_string(root.join("prompt.txt")).unwrap(), sparql(&root, actions).len(), degraded)
        };

        let (prompt, n, degraded) = run(&config);
        assert!(prompt.contains("[m1] INC-1: rolled back checkout") && prompt.contains("\"actions\""), "{prompt}");
        assert_eq!((n, degraded.len()), (1, 0));

        let (prompt, n, _) = run(&config.replace("path = \"ops.json\"\n", "path = \"ops.json\"\nactions = false\n"));
        assert!(!prompt.contains("[m1]") && !prompt.contains("\"actions\""), "{prompt}");
        assert_eq!(n, 0);

        let (_, n, degraded) = run(&config.replace("path = \"ops.json\"\n", "path = \"ops.json\"\nknown_apps = true\n"));
        assert_eq!(n, 1);
        assert_eq!(degraded, ["known_apps = true is used only with actions = false; the threads were sent without the catalog list"]);
    }

    #[test]
    fn uses_names_the_facts_of_other_files_that_would_lose_what_they_point_at() {
        let thread = format!(r#"[{{"ts":"{}.000100","channel":"ops","text":"INC-10: rolled back checkout to 4.0.9"}}]"#, T - 100);
        let root = fixture("uses-cascade", &[("discussions", "ops.json", &thread), ("incidents", "more.json", &format!(r#"[{{"id":"INC-10","service":"checkout","opened":{T}}}]"#))]);
        let answer = r#"{"category":"UNKNOWN","sentiment":"urgent","apps":[],"calls":[],"actions":[{"kind":"ROLLBACK","app":"checkout","message":1,"quote":"rolled back checkout"}],"confidence":0.9}"#;
        std::fs::write(root.join("answer.json"), answer).unwrap();
        let cfg = root.join(".ograph/config.toml");
        let model = format!("[llm.extract]\ncommand = [\"sh\", \"-c\", \"cat >/dev/null; cat '{}'\"]\nmodel = \"fixture\"\n", root.join("answer.json").display());
        let config = std::fs::read_to_string(&cfg).unwrap().replace("path = \"ops.json\"\n", "path = \"ops.json\"\nactions = true\n");
        std::fs::write(&cfg, config.clone() + &model).unwrap();
        harvest(&root, None);
        let during = "SELECT ?a WHERE { ?a evt:during ?i }";
        assert_eq!(sparql(&root, during).len(), 1);
        let ws = Workspace::open(&root).unwrap();
        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, &ws.config.correlation, "more.json").unwrap();
        drop(ws);
        let elsewhere: Vec<_> = u.facts.iter().filter(|f| f.because.is_some()).map(|f| (f.predicate.as_str(), f.object.as_str(), f.graph.as_str())).collect();
        assert_eq!(elsewhere, [("evt:during", "id:inc/inc-10", "g:harvest/discussions/ops.json")]);

        std::fs::remove_file(root.join("more.json")).unwrap();
        std::fs::write(&cfg, config.replace("[[source]]\nkind = \"incidents\"\npath = \"more.json\"\n", "") + &model).unwrap();
        harvest(&root, None);
        assert!(sparql(&root, during).is_empty());
    }

    #[test]
    fn uses_takes_a_field_that_only_its_file_states_on_an_incident_another_file_states() {
        let root = fixture("uses-fields", &[
            ("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T + 3000))),
            ("incidents", "inc-a.json", r#"[{"id":"INC-9","title":"Checkout errors"}]"#),
            ("incidents", "inc-b.json", &format!(r#"[{{"id":"INC-9","service":"checkout","opened":{}}}]"#, T + 3600)),
        ]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, &ws.config.correlation, "inc-b.json").unwrap();
        drop(ws);
        let only: Vec<_> = u.facts.iter().filter(|f| f.subject == "id:inc/inc-9" && f.also_in.is_empty() && f.because.is_none()).map(|f| (f.predicate.as_str(), f.object.as_str())).collect();
        assert_eq!(only, [("evt:at", "2026-11-22T23:26:40Z"), ("evt:on", "id:app/checkout"), ("evt:sourceRecord", "inc-b.json#1")]);
        let rechecked: Vec<_> = u.facts.iter().filter(|f| f.subject == "id:inc/inc-9" && f.because.is_some()).map(|f| f.predicate.as_str()).collect();
        assert_eq!(rechecked, ["evt:signal", "evt:sourceRecord", "inc:id", "inc:title", "rdf:type"]);
        assert_eq!(u.chains.iter().map(|c| (c.incident.as_str(), c.change)).collect::<Vec<_>>(), [("INC-9", "gone")]);
        assert_eq!((u.shared.len(), u.events.iter().map(|e| e.node.as_str()).collect::<Vec<_>>()), (0, vec!["id:inc/inc-9"]));
        assert!(u.warnings.is_empty(), "{:?}", u.warnings);

        std::fs::remove_file(root.join("inc-b.json")).unwrap();
        let cfg = root.join(".ograph/config.toml");
        std::fs::write(&cfg, std::fs::read_to_string(&cfg).unwrap().replace("[[source]]\nkind = \"incidents\"\npath = \"inc-b.json\"\n", "")).unwrap();
        harvest(&root, None);
        assert!(sparql(&root, "SELECT ?p WHERE { id:inc\\/inc-9 ?p ?o }").is_empty());
        assert!(sparql(&root, "SELECT ?c WHERE { ?c corr:leadsTo id:inc\\/inc-9 }").is_empty());
    }

    #[test]
    fn uses_of_a_chat_record_traces_its_thread_and_the_quarantine_it_caused() {
        let thread = format!(
            r#"[{{"ts":"{a}.000100","channel":"ops","text":"checkout is slow"}},{{"ts":"{b}.000100","channel":"ops","text":"billing queue backs up"}},{{"ts":"{c}.000100","thread_ts":"{b}.000100","channel":"ops","text":"still backing up"}}]"#,
            a = T - 300,
            b = T - 200,
            c = T - 100
        );
        let root = fixture("uses-chat", &[("discussions", "ops.json", &thread)]);
        std::fs::write(root.join("answer.json"), r#"{"category":"UNKNOWN","sentiment":"urgent","apps":["billingx"],"calls":[],"confidence":0.9}"#).unwrap();
        let cfg = root.join(".ograph/config.toml");
        let model = format!("[llm.extract]\ncommand = [\"sh\", \"-c\", \"cat >/dev/null; cat '{}'\"]\nmodel = \"fixture\"\n", root.join("answer.json").display());
        std::fs::write(&cfg, std::fs::read_to_string(&cfg).unwrap() + &model).unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let st = &ws.config.correlation;
        let thread_a = format!("id:thread/ops-{}-000100", T - 300);
        let thread_b = format!("id:thread/ops-{}-000100", T - 200);

        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, st, "ops.json#1").unwrap();
        let types: Vec<_> = u.facts.iter().filter(|f| f.predicate == "rdf:type").map(|f| (f.subject.as_str(), f.object.as_str())).collect();
        assert!(types.contains(&(thread_a.as_str(), "disc:Thread")), "{types:?}");
        assert!(u.shared.is_empty());
        let quarantined: Vec<_> = u.quarantined.iter().map(|q| q.fact.as_str()).collect();
        assert_eq!(quarantined, [format!("{thread_a} disc:about ops:Application?\"billingx\"")]);

        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, st, "ops.json#3").unwrap();
        assert_eq!(u.shared, [thread_b.as_str()]);
        assert!(u.quarantined.is_empty());
        assert_eq!(u.warnings, [format!("{thread_b} keeps its other messages; without this one its text, and so the model's answers on it, change in ways this answer does not predict")]);
    }

    #[test]
    fn uses_warns_when_the_incidents_were_harvested_before_they_named_their_records() {
        let root = fixture("uses-old", &[("incidents", "more.json", &format!(r#"[{{"id":"INC-2","service":"checkout","opened":{}}}]"#, T + 86_400))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let rp = snap.iri(crate::vocab::evt::SOURCE_RECORD).unwrap().unwrap();
        let old: Vec<[crate::term::Id; 4]> = snap.quads([None, Some(rp), None], crate::store::GraphSel::AnyNamed).map(Result::unwrap).filter(|q| crate::correlate::literal(&snap, q[3]).is_some_and(|g| g.contains("/incidents/"))).collect();
        drop(snap);
        ws.store.transact(|tx| tx.remove(old)).unwrap();
        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, &ws.config.correlation, "more.json").unwrap();
        assert!(u.facts.is_empty());
        assert_eq!(u.warnings, ["g:harvest/incidents/all names no evt:sourceRecord, so its incidents cannot be traced to their files; harvest once to record them"]);
    }

    #[test]
    fn chains_since_a_time_an_incident_had_no_application_ranks_nothing_then() {
        let root = fixture("since-custom", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T + 86_400 - 600))), ("incidents", "more.json", r#"[{"id":"INC-2","title":"Checkout slow"}]"#)]);
        let shapes = root.join(".ograph/shapes.ttl");
        std::fs::write(&shapes, std::fs::read_to_string(&shapes).unwrap().replace("evt:OperatorAction, inc:Incident ;", "evt:OperatorAction ;")).unwrap();
        harvest(&root, None);
        let t1 = pause();
        std::fs::write(root.join("more.json"), format!(r#"[{{"id":"INC-2","title":"Checkout slow","service":"checkout","opened":{}}}]"#, T + 86_400)).unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let shift = crate::query::rank_shift(&ws.store.snapshot().unwrap(), t1, "INC-2", &ws.config.correlation, 5).unwrap();
        let apps = |r: &[crate::query::RankedApp]| r.iter().map(|a| a.app.clone()).collect::<Vec<_>>();
        assert_eq!((apps(&shift.then), apps(&shift.now)), (vec![], vec!["ledger-db".to_owned()]));
    }

    #[test]
    fn an_incident_that_left_the_graph_is_not_found_and_one_in_quarantine_says_so() {
        let root = fixture("gone-incident", &[("incidents", "more.json", &format!(r#"[{{"id":"INC-2","service":"checkout","opened":{}}},{{"id":"INC-3","title":"no time"}}]"#, T + 86_400))]);
        harvest(&root, None);
        std::fs::write(root.join("more.json"), r#"[{"id":"INC-3","title":"no time"}]"#).unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        assert_eq!(crate::query::lookup(&snap, "INC-2").unwrap_err().to_string(), "nothing in the graph is called INC-2");
        // Which missing field SHACL reports first varies; both are a missing time or application.
        let err = crate::query::lookup(&snap, "INC-3").unwrap_err().to_string();
        assert!(err.starts_with("INC-3 is in quarantine: SHACL MinCountConstraintComponent: an event needs "), "{err}");
    }

    #[test]
    fn blast_at_a_time_lists_only_the_incidents_open_then() {
        let root = fixture("blast-at", &[("incidents", "more.json", &format!(r#"[{{"id":"INC-2","service":"checkout","opened":{},"resolved":{},"status":"resolved"}}]"#, T - 1000, T - 500))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let open = |at: Option<i64>| crate::query::blast(&snap, "ledger-db", 8, at).unwrap().open_incidents;
        assert_eq!(open(None), ["id:inc/inc-1"]);
        assert_eq!(open(Some((T - 700) * 1000)), ["id:inc/inc-2"]);
        assert_eq!(open(Some((T - 100) * 1000)), Vec::<String>::new());
        assert_eq!(open(Some(T * 1000)), ["id:inc/inc-1"]);
    }

    #[test]
    fn a_source_path_is_keyed_as_the_harvest_keys_it() {
        use crate::query::{source_key, split_record};
        assert_eq!(split_record("runbooks/db.md:12"), ("runbooks/db.md", Some(":12")));
        assert_eq!(split_record("logs.jsonl@1795386000000"), ("logs.jsonl", Some("@1795386000000")));
        assert_eq!(split_record("notes#draft"), ("notes#draft", None));
        let (root, cwd) = (Path::new("/w"), Path::new("/w/logs"));
        assert_eq!(source_key(root, cwd, "a.jsonl#3"), "logs/a.jsonl#3");
        assert_eq!(source_key(root, cwd, "/data/case/metrics.csv"), "/data/case/metrics.csv");
        assert_eq!(source_key(Path::new("."), Path::new("/w"), "/w/runbooks/"), "runbooks");
        assert_eq!(source_key(root, cwd, "../shop"), "shop");
        assert_eq!(source_key(root, cwd, "sub/../alerts.json#2"), "logs/alerts.json#2");
        assert_eq!(source_key(root, cwd, "./a.jsonl"), "logs/a.jsonl");
        assert_eq!(source_key(Path::new("."), Path::new("/w"), "."), "");
        assert_eq!(source_key(Path::new("../w"), Path::new("/x"), "../w/logs/../alerts.json"), "alerts.json");
    }

    #[test]
    fn uses_of_the_workspace_root_lists_every_source() {
        let root = fixture("uses-root", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let key = crate::query::source_key(&root, &root.join("sub"), "..");
        let u = crate::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, &ws.config.correlation, &key).unwrap();
        assert_eq!(u.graphs, ["g:harvest/catalog/", "g:harvest/alerts/alerts.json", "g:harvest/incidents/all"]);
    }

    #[test]
    fn similar_reports_only_the_current_verdict() {
        let root = fixture("similar-latest", &[("alerts", "alerts.json", &format!("[{},{}]", alert("ledger-db", "PoolSaturated", T - 600), alert("ledger-db", "PoolSaturated", T + 86_400 - 600)))]);
        std::fs::write(root.join("incidents.json"), format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T}}},{{"id":"INC-2","service":"checkout","opened":{}}}]"#, T + 86_400)).unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let similar = |ws: &Workspace| crate::query::similar(&ws.store.snapshot().unwrap(), "INC-2", &ws.config.correlation, 5, &mut Default::default()).unwrap().into_iter().map(|s| s.cause_apps).collect::<Vec<_>>();
        ws.verdict("id:chain/inc-1-1", true, "oncall", &[]).unwrap();
        assert_eq!(similar(&ws), [vec!["ledger-db".to_owned()]]);
        ws.verdict("id:chain/inc-1-1", false, "oncall", &[]).unwrap();
        assert_eq!(similar(&ws), [Vec::<String>::new()]);
    }

    #[test]
    fn replay_lists_the_verdicts_other_settings_would_contradict() {
        let alerts = [alert("ledger-db", "PoolSaturated", T - 600), alert("payments-api", "ErrorRate", T - 300)];
        let root = fixture("replay", &[("alerts", "alerts.json", &format!("[{}]", alerts.join(",")))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        ws.verdict("id:chain/inc-1-1", true, "oncall", &[]).unwrap();
        ws.verdict("id:chain/inc-1-2", false, "oncall", &[]).unwrap();
        let replay = |st: &crate::correlate::Settings| {
            let r = crate::query::replay(&ws.store.snapshot().unwrap(), st, &ws.config.correlation).unwrap();
            let rows: Vec<_> = r.verdicts.iter().map(|v| (v.state.clone(), v.app.clone(), v.judged_rank.clone(), v.before.app_rank, v.after.app_rank, v.flip)).collect();
            (r.flips, r.fixed, r.contradicted, r.stale_chains, rows)
        };
        let mut st = ws.config.correlation.clone();
        let (one, two) = (Some("1".to_owned()), Some("2".to_owned()));
        let (flips, fixed, contradicted, stale, mut rows) = replay(&st);
        rows.sort();
        assert_eq!((flips, fixed, contradicted, stale), (0, 0, 0, 0));
        assert_eq!(rows, [("accepted".into(), "ledger-db".into(), one.clone(), Some(1), Some(1), None), ("rejected".into(), "payments-api".into(), two.clone(), Some(2), Some(2), None)]);
        st.window_s = 400;
        let (flips, fixed, contradicted, stale, mut rows) = replay(&st);
        rows.sort();
        assert_eq!((flips, fixed, contradicted, stale), (2, 0, 2, 0));
        assert_eq!(rows, [("accepted".into(), "ledger-db".into(), one, Some(1), None, Some("dropped out")), ("rejected".into(), "payments-api".into(), two, Some(2), Some(1), Some("rose to rank 1"))]);
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), format!("{config}[correlation]\nwindow_s = 400\n")).unwrap();
        let ws = Workspace::open(&root).unwrap();
        let r = crate::query::replay(&ws.store.snapshot().unwrap(), &crate::correlate::Settings::default(), &ws.config.correlation).unwrap();
        assert_eq!((r.stale_chains, r.flips, r.fixed, r.contradicted), (2, 0, 2, 0));
    }

    fn with_policy(root: &Path, policy: &str) {
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        let base = config.split("[verdict]").next().unwrap();
        std::fs::write(root.join(".ograph/config.toml"), format!("{base}[verdict]\n{policy}")).unwrap();
    }

    fn refusal(root: &Path, evidence: &[&str]) -> String {
        let ev: Vec<String> = evidence.iter().map(|e| e.to_string()).collect();
        format!("{:#}", Workspace::open(root).unwrap().verdict("id:chain/inc-1-1", true, "oncall", &ev).unwrap_err())
    }

    #[test]
    fn without_a_policy_a_verdict_needs_no_evidence() {
        let root = fixture("policy-none", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", true, "oncall", &[" ".into()]).unwrap();
        assert_eq!(sparql(&root, "SELECT ?e WHERE { ?v rca:evidence ?e }"), [["(none given)"]]);
    }

    #[test]
    fn the_evidence_policy_refuses_a_verdict_and_says_why() {
        let root = fixture("policy", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        with_policy(&root, "require_evidence = true\n");
        assert_eq!(refusal(&root, &[]), "verdict not recorded: [verdict] require_evidence is set and no --evidence was given");
        with_policy(&root, "evidence_patterns = ['^PM-\\d+$', '^https://tickets/']\n");
        assert_eq!(refusal(&root, &["looked bad", "PM-x"]), r#"verdict not recorded: no evidence matches [verdict] evidence_patterns (^PM-\d+$, ^https://tickets/); the evidence given was "looked bad", "PM-x""#);
        assert_eq!(refusal(&root, &[]), r"verdict not recorded: no evidence matches [verdict] evidence_patterns (^PM-\d+$, ^https://tickets/); no --evidence was given");
        with_policy(&root, "evidence_in_store = true\n");
        assert_eq!(refusal(&root, &["PM-9"]), r#"verdict not recorded: [verdict] evidence_in_store is set and no evidence names anything in the graph; the evidence given was "PM-9". Cite an incident, event or document as `ograph why` names it"#);
        assert!(sparql(&root, "SELECT ?v WHERE { ?v a rca:Verdict }").is_empty());
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", true, "oncall", &["PM-9".into(), "INC-1".into()]).unwrap();
        assert_eq!(sparql(&root, "SELECT ?e WHERE { ?v rca:evidence ?e } ORDER BY ?e"), [["INC-1"], ["PM-9"]]);
        with_policy(&root, "evidence_patterns = ['(']\n");
        assert!(Workspace::open(&root).is_err());
    }

    #[test]
    fn the_lab_page_marks_every_event_and_link_once_and_draws_every_application() {
        let alerts = [alert("ledger-db", "PoolSaturated", T - 600), alert("payments-api", "ErrorRate", T - 300), alert("checkout", "Latency", T - 60)];
        let root = fixture("lab", &[("alerts", "alerts.json", &format!("[{}]", alerts.join(",")))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        ws.verdict("id:chain/inc-1-1", true, "oncall", &["PM-7".into()]).unwrap();
        let page = crate::lab::page(&ws.store.snapshot().unwrap(), &ws.config.correlation).unwrap();
        let svg = |class: &str| page[page.find(&format!("<svg class=\"{class}\"")).unwrap()..].split("</svg>").next().unwrap().to_owned();
        let (timeline, hood) = (svg("timeline"), svg("hood"));
        for (app, name) in [("ledger-db", "poolsaturated"), ("payments-api", "errorrate"), ("checkout", "latency")] {
            let mark = format!("data-ev=\"id:ev/alert-{app}-{name}-");
            assert_eq!(timeline.matches(&mark).count(), 1, "{app}");
            assert_eq!(hood.matches(&format!("data-app=\"{app}\"")).count(), 1, "{app}");
        }
        assert_eq!(timeline.matches("data-ev=").count(), 3);
        let hops: Vec<&str> = timeline.match_indices("data-hop=\"").map(|(i, _)| &timeline[i + 10..i + 11]).collect();
        assert_eq!(hops, ["0", "1"]);
        assert_eq!(hood.matches("class=\"dep hot\"").count(), 1);
        assert!(page.contains("Verdict: accepted by oncall at "));
        assert!(page.contains("evidence: PM-7"));
        assert!(page.contains("ograph verdict id:chain/inc-1-"));
        let links: Vec<&str> = page.match_indices("<br>link to ").map(|(i, _)| page[i + 12..].split('<').next().unwrap().trim_end()).collect();
        let (to_checkout, to_incident) = ("checkout:ALERT: score 0.920 (time 0.87, severity 1.00)", "incident: score 0.900 (known pair)");
        assert_eq!(links, [to_checkout, to_incident, "payments-api:ALERT: score 0.900 (time 0.83, severity 1.00)", to_checkout, to_incident, to_incident]);
        let weakest: Vec<&str> = page.match_indices("<p>Weakest link: ").map(|(i, _)| page[i + 17..].split('<').next().unwrap()).collect();
        assert_eq!(weakest, ["checkout:ALERT to incident, score 0.900", "ledger-db:ALERT to payments-api:ALERT, score 0.900"]);
        assert!(!page.contains("<script src"));
    }

    #[test]
    fn the_lab_page_shows_the_current_verdict_of_a_rejudged_chain() {
        let root = fixture("lab-rejudged", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        ws.verdict("id:chain/inc-1-1", true, "oncall", &["PM-7".into()]).unwrap();
        ws.verdict("id:chain/inc-1-1", false, "lead", &["rollback changed nothing".into()]).unwrap();
        let page = crate::lab::page(&ws.store.snapshot().unwrap(), &ws.config.correlation).unwrap();
        assert!(page.contains("Verdict: rejected by lead at "), "{}", &page[page.find("Verdict:").unwrap_or(0)..][..80]);
        assert!(!page.contains("Verdict: accepted"));
    }

    #[test]
    fn a_leftover_iri_in_the_rank_form_does_not_shadow_the_rank() {
        let root = fixture("leftover", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        ws.store.transact(|tx| tx.intern(&oxrdf::NamedNode::new_unchecked("https://ograph.dev/id/chain/inc-1-1").into()).map(drop)).unwrap();
        ws.verdict("id:chain/inc-1-1", true, "oncall", &[]).unwrap();
        assert_eq!(sparql(&root, VERDICT_ROOT), [[format!("id:ev/alert-ledger-db-poolsaturated-{}000", T - 600)]]);
    }

    #[test]
    fn a_verdict_outside_the_chain_namespace_is_an_error() {
        let root = fixture("typo", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        assert!(Workspace::open(&root).unwrap().verdict("https://example.org/typo/inc-1-1", true, "oncall", &[]).is_err());
        assert!(sparql(&root, "SELECT ?v WHERE { ?v a rca:Verdict }").is_empty());
    }

    #[test]
    fn a_chain_cannot_hold_two_roots() {
        let root = fixture("two-roots", &[("alerts", "alerts.json", &format!("[{},{}]", alert("ledger-db", "A", T - 600), alert("ledger-db", "B", T - 600)))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let mut b = crate::model::Batch::new("correlation", "inc-1", "test");
        let m = crate::model::Extraction::Rule { extractor: "test".into() };
        let ev = b.record("test");
        let c = crate::model::known("chain", "inc-1-0");
        let n = |s: &str| crate::model::obj(oxrdf::NamedNode::new_unchecked(s));
        b.add(c.clone(), crate::vocab::RDF_TYPE, n(crate::vocab::corr::CHAIN), &ev, &m);
        b.add(c.clone(), crate::vocab::corr::LEADS_TO, n("https://ograph.dev/id/inc/inc-1"), &ev, &m);
        for name in ["a", "b"] {
            b.add(c.clone(), crate::vocab::corr::ROOT_CAUSE, n(&format!("https://ograph.dev/id/ev/alert-ledger-db-{name}-{}000", T - 600)), &ev, &m);
        }
        let o = crate::gateway::submit(&ws.store, &ws.ontology, &b, 0).unwrap();
        assert!(!o.quarantined.is_empty(), "{o:?}");
    }

    #[test]
    fn an_unreadable_source_keeps_only_its_own_graphs() {
        let root = fixture("siblings", &[]);
        for (f, name) in [("east/1.json", "E1"), ("west/1.json", "W1"), ("west/2.json", "W2")] {
            std::fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
            std::fs::write(root.join(f), format!("[{}]", alert("ledger-db", name, T - 600))).unwrap();
        }
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), format!("{config}[[source]]\nkind = \"alerts\"\npath = \"./east\"\n[[source]]\nkind = \"alerts\"\npath = \"west\"\n")).unwrap();
        harvest(&root, None);
        std::fs::rename(root.join("east"), root.join("offline")).unwrap();
        std::fs::remove_file(root.join("west/2.json")).unwrap();
        harvest(&root, None);
        assert_eq!(sparql(&root, "SELECT ?n WHERE { ?e a evt:Alert ; evt:summary ?n } ORDER BY ?n"), [["E1"], ["W1"]]);
    }

    #[test]
    fn an_empty_source_directory_keeps_its_graphs() {
        let root = fixture("empty-dir", &[]);
        std::fs::create_dir(root.join("al")).unwrap();
        std::fs::write(root.join("al/1.json"), format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600))).unwrap();
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), format!("{config}[[source]]\nkind = \"alerts\"\npath = \"al\"\n")).unwrap();
        harvest(&root, None);
        std::fs::remove_file(root.join("al/1.json")).unwrap();
        let out = harvest(&root, None);
        assert!(out.iter().any(|o| !o.degraded.is_empty()), "{out:?}");
        assert_eq!(sparql(&root, "SELECT ?e WHERE { ?e a evt:Alert }").len(), 1);
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
        let before = crate::query::before(&ws.store.snapshot().unwrap(), "INC-1", 600, 4, 1800).unwrap();
        assert_eq!(before.events.iter().map(|e| (e.app.as_str(), e.signal.as_str())).collect::<Vec<_>>(), [("ledger-db", "METRIC_ANOMALY")]);
        assert!(before.events[0].source_record.as_deref().is_some_and(|r| r.starts_with("metrics.csv")));
    }

    #[test]
    fn a_series_that_keeps_firing_before_the_incident_is_chronic() {
        let mut csv = String::from("time,ledger-db_diskio,ledger-db_latency\n");
        for i in 0..240 {
            let disk = if [150, 180, 210].iter().any(|s| (*s..s + 6).contains(&i)) { 5000.0 } else { 0.0 };
            let latency = if i < 225 { 100.0 + (i % 7) as f64 * 0.3 } else { 1000.0 };
            csv += &format!("{},{disk},{latency}\n", T - 3600 + 15 * i);
        }
        let root = fixture("chronic", &[("metrics", "metrics.csv", &csv)]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let before = crate::query::before(&snap, "INC-1", 1800, 4, 1800).unwrap();
        let marks: Vec<(&str, bool)> = before.events.iter().map(|e| (e.event.rsplit_once('-').unwrap().0, e.chronic.is_some())).collect();
        assert_eq!(marks, [("id:ev/metric-ledger-db-latency", false), ("id:ev/metric-ledger-db-diskio", true), ("id:ev/metric-ledger-db-diskio", true), ("id:ev/metric-ledger-db-diskio", true)]);
        assert!(before.events[1].chronic.as_deref().unwrap().starts_with("ledger-db diskio fired 3 times in the 30 min before the incident"));
        // The marks come only from the lookback before the incident: one episode in it is not enough.
        let short = crate::query::before(&snap, "INC-1", 1800, 4, 300).unwrap();
        assert!(short.events.iter().all(|e| e.chronic.is_none()));
        let why = crate::query::why(&snap, &before.events[3].event, Some(1800)).unwrap();
        assert!(why.chronic.is_some());
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
        let typos = [
            "[corelation]\n",
            "[correlation]\nwindw_s = 600\n",
            "[[sources]]\nkind = \"logs\"\npath = \"x\"\n",
            "[[source]]\nkind = \"logs\"\npth = \"x\"\npath = \"x\"\n",
            "[detect]\nmetric_zz = 3\n",
            "[[source]]\nkind = \"cod\"\npath = \"x\"\n",
            "[[source]]\nkind = \"code\"\npath = \"x\"\nsinse = \"7d\"\n",
            "[[source]]\nkind = \"errors\"\npath = \"x\"\nsince = \"7d\"\n",
        ];
        for typo in typos {
            std::fs::write(root.join(".ograph/config.toml"), format!("{config}{typo}")).unwrap();
            assert!(Workspace::open(&root).is_err(), "accepted {typo:?}");
        }
    }

    #[test]
    fn a_code_source_takes_since_and_an_errors_source_only_a_path() {
        use crate::enrich::Source;
        let config: super::Config = toml::from_str("[[source]]\nkind = \"errors\"\npath = \"e.jsonl\"\n[[source]]\nkind = \"code\"\npath = \"../shop\"\n[[source]]\nkind = \"code\"\npath = \"lib\"\nsince = \"2026-11-01\"\n").unwrap();
        assert_eq!(config.sources, [Source::Errors { path: "e.jsonl".into() }, Source::Code { path: "../shop".into(), since: "30d".into() }, Source::Code { path: "lib".into(), since: "2026-11-01".into() }]);
        assert_eq!(config.sources.iter().map(Source::name).collect::<Vec<_>>(), ["errors", "code", "code"]);
        let mut order = [Source::Traces { path: "t".into() }, Source::Errors { path: "e".into() }, Source::Alerts { path: "a".into() }, Source::Code { path: "c".into(), since: "30d".into() }, Source::ConfigChanges { path: "g".into() }];
        order.sort();
        assert_eq!(order.iter().map(Source::name).collect::<Vec<_>>(), ["config", "code", "alerts", "traces", "errors"]);
    }

    /// INC-1 opened at `T` with SEV1 in incidents.json; a later export says 40 minutes later and
    /// SEV3. The cause is an alert on ledger-db 10 minutes before `T`; a decoy alert on
    /// payments-api falls inside the window of the later time only.
    fn disagreeing_exports(name: &str, merge: &str) -> (PathBuf, Vec<crate::gateway::Rejection>, String) {
        let alerts = format!("[{},{}]", alert("ledger-db", "PoolSaturated", T - 600), alert("payments-api", "ErrorRate", T + 1500));
        let later = format!(r#"[{{"id":"INC-1","service":"checkout","opened":{},"severity":"SEV3","status":"open"}}]"#, T + 2400);
        let root = fixture(name, &[("alerts", "alerts.json", &alerts), ("incidents", "later.json", &later)]);
        let cfg = root.join(".ograph/config.toml");
        std::fs::write(&cfg, std::fs::read_to_string(&cfg).unwrap() + merge).unwrap();
        let out = harvest(&root, None);
        let quarantined = out.iter().find(|o| o.graph.contains("/incidents/")).unwrap().quarantined.clone();
        let ws = Workspace::open(&root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let ids = crate::correlate::Ids::new(&snap).unwrap();
        let top = crate::correlate::analyze(&snap, &ids, crate::query::lookup(&snap, "INC-1").unwrap(), &ws.config.correlation, &[]).unwrap().apps[0].app.clone();
        (root, quarantined, top)
    }

    fn reasons(q: &[crate::gateway::Rejection]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = q.iter().map(|r| (r.fact.clone(), r.reason.clone())).collect();
        out.sort();
        out
    }

    #[test]
    fn disagreeing_exports_keep_the_last_value_and_quarantine_the_other() {
        let (root, q, top) = disagreeing_exports("conflict-last", "");
        assert_eq!(
            reasons(&q),
            [
                ("id:inc/inc-1 evt:at \"2026-11-22T22:26:40Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>".to_owned(), "incidents.json#1 disagrees with later.json#1 (kept by policy last)".to_owned()),
                ("id:inc/inc-1 evt:severity \"SEV1\"".to_owned(), "incidents.json#1 disagrees with later.json#1 (kept by policy last)".to_owned()),
            ]
        );
        assert_eq!(top, "payments-api");
        let ws = Workspace::open(&root).unwrap();
        let why = crate::query::why(&ws.store.snapshot().unwrap(), "INC-1", Some(1800)).unwrap();
        let at: Vec<&str> = why.facts.iter().filter(|f| f.predicate == "evt:at").map(|f| f.object.as_str()).collect();
        assert_eq!(at, ["2026-11-22T23:06:40Z"]);
        assert_eq!(why.quarantined.len(), 2);
    }

    #[test]
    fn the_earliest_open_time_recovers_the_cause_the_later_export_hides() {
        let (_, q, top) = disagreeing_exports("conflict-earliest", "[incidents]\nmerge = { at = \"earliest\", severity = \"max\" }\n");
        assert_eq!(
            reasons(&q),
            [
                ("id:inc/inc-1 evt:at \"2026-11-22T23:06:40Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>".to_owned(), "later.json#1 disagrees with incidents.json#1 (kept by policy earliest)".to_owned()),
                ("id:inc/inc-1 evt:severity \"SEV3\"".to_owned(), "later.json#1 disagrees with incidents.json#1 (kept by policy max)".to_owned()),
            ]
        );
        assert_eq!(top, "ledger-db");
    }

    #[test]
    fn records_that_agree_are_no_conflict() {
        let same = format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T},"severity":"SEV1","title":"Checkout down"}}]"#);
        let root = fixture("conflict-none", &[("incidents", "later.json", &same)]);
        let out = harvest(&root, None);
        assert_eq!(out.iter().find(|o| o.graph.contains("/incidents/")).unwrap().quarantined.len(), 0);
    }

    #[test]
    fn the_merge_policy_picks_among_valid_values_and_an_invalid_one_is_quarantined_for_itself() {
        let later = format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T},"severity":"SEV3"}}]"#);
        let root = fixture("conflict-invalid", &[("incidents", "later.json", &later)]);
        std::fs::write(root.join("incidents.json"), format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T},"severity":"critical","status":"open"}}]"#)).unwrap();
        let cfg = root.join(".ograph/config.toml");
        std::fs::write(&cfg, std::fs::read_to_string(&cfg).unwrap() + "[incidents]\nmerge = { severity = \"max\" }\n").unwrap();
        let out = harvest(&root, None);
        let q = out.iter().find(|o| o.graph.contains("/incidents/")).unwrap().quarantined.clone();
        assert_eq!(reasons(&q), [("id:inc/inc-1 evt:severity \"critical\"".to_owned(), "\"critical\" is not in the closed vocabulary of evt:severity".to_owned())]);
        assert_eq!(sparql(&root, "SELECT ?s WHERE { ?i inc:id \"INC-1\" ; evt:severity ?s }"), [["SEV3"]]);
    }

    #[test]
    fn service_names_that_differ_only_in_case_are_no_conflict() {
        let same = format!(r#"[{{"id":"INC-1","service":"Checkout","opened":{T},"severity":"SEV1"}}]"#);
        let root = fixture("conflict-case", &[("incidents", "later.json", &same)]);
        let out = harvest(&root, None);
        assert_eq!(reasons(&out.iter().find(|o| o.graph.contains("/incidents/")).unwrap().quarantined), []);
    }

    #[test]
    fn a_withheld_value_counts_as_proposed() {
        let later = format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T},"severity":"SEV3"}}]"#);
        let root = fixture("conflict-count", &[("incidents", "later.json", &later)]);
        let out = harvest(&root, None);
        let o = out.iter().find(|o| o.graph.contains("/incidents/")).unwrap();
        assert_eq!((o.proposed, o.accepted, o.quarantined.len()), (12, 11, 1));
    }

    #[test]
    fn a_pick_that_does_not_fit_its_field_is_refused() {
        let root = fixture("conflict-badpick", &[]);
        let cfg = root.join(".ograph/config.toml");
        std::fs::write(&cfg, std::fs::read_to_string(&cfg).unwrap() + "[incidents]\nmerge = { service = \"earliest\" }\n").unwrap();
        assert_eq!(Workspace::open(&root).err().unwrap().to_string(), "[incidents] merge: service = \"earliest\" does not apply to service");
    }

    #[test]
    fn a_done_action_takes_its_message_time_and_the_rest_is_quarantined() {
        let msg = |at: i64, text: &str| format!(r#"{{"ts":"{at}.000100","thread_ts":"{}.000100","channel":"ops","text":"{text}"}}"#, T - 400);
        let thread = format!("[{},{},{}]", msg(T - 400, "checkout errors spiking INC-10, should we roll back checkout?"), msg(T - 100, "rolled back checkout to 4.0.9"), msg(T + 60, "errors gone"));
        let alerts = format!("[{},{}]", alert("checkout", "Latency", T - 300), alert("checkout", "Latency", T + 86_100));
        let root = fixture("actions", &[("discussions", "ops.json", &thread), ("alerts", "alerts.json", &alerts)]);
        std::fs::write(root.join("incidents.json"), format!(r#"[{{"id":"INC-10","service":"checkout","opened":{T},"resolved":{}}},{{"id":"INC-11","service":"checkout","opened":{}}}]"#, T + 600, T + 86_400)).unwrap();
        let action = |kind: &str, app: &str, n: u32, quote: &str| format!(r#"{{"kind":"{kind}","app":"{app}","message":{n},"quote":"{quote}"}}"#);
        let actions = [
            action("ROLLBACK", "checkout", 2, "rolled back checkout"),
            action("ROLLBACK", "checkout", 1, "roll back checkout"),
            action("RESTART", "checkout", 3, "restarted checkout"),
            action("SCALE", "checkout", 7, "scaled checkout"),
            action("FAILOVER", "made-up", 2, "rolled back checkout"),
            action("REBOOT", "checkout", 2, "rolled back checkout"),
            action("RESTART", "checkout", 3, "errors gone"),
        ];
        let answer = format!(r#"{{"category":"UNKNOWN","sentiment":"urgent","apps":[],"calls":[],"actions":[{}],"confidence":0.9}}"#, actions.join(","));
        std::fs::write(root.join("answer.json"), answer).unwrap();
        let cfg = root.join(".ograph/config.toml");
        let model = format!("[llm.extract]\ncommand = [\"sh\", \"-c\", \"cat >/dev/null; cat '{}'\"]\nmodel = \"fixture\"\n", root.join("answer.json").display());
        let config = std::fs::read_to_string(&cfg).unwrap().replace("path = \"ops.json\"\n", "path = \"ops.json\"\nactions = true\n");
        std::fs::write(&cfg, config + &model).unwrap();
        let out = harvest(&root, None);
        let mut reasons: Vec<String> = out.iter().find(|o| o.graph.contains("/discussions/")).unwrap().quarantined.iter().map(|r| r.reason.clone()).collect();
        reasons.sort();
        assert_eq!(
            reasons,
            [
                "\"REBOOT\" is not in the closed vocabulary of evt:actionKind",
                "\"errors gone\" does not say what a RESTART does",
                "\"restarted checkout\" is not in message 3",
                "\"roll back checkout\" is a question, not a done action",
            ]
            .into_iter()
            .chain(["SHACL MinCountConstraintComponent: an action needs its kind"; 8])
            .chain(["message 7 is not in the thread", "the thread does not name \"made-up\""])
            .collect::<Vec<_>>()
        );
        assert_eq!(violations(&root), 0);
        assert_eq!(
            sparql(&root, "SELECT ?k ?app ?at ?i WHERE { ?a a evt:OperatorAction ; evt:actionKind ?k ; evt:on ?app ; evt:at ?at ; evt:during ?i }"),
            [["ROLLBACK", "id:app/checkout", "2026-11-22T22:25:00Z", "id:inc/inc-10"]]
        );
        let ws = Workspace::open(&root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let before = crate::query::before(&snap, "INC-10", 1800, 4, 1800).unwrap();
        assert_eq!(before.events.iter().map(|e| e.signal.as_str()).collect::<Vec<_>>(), ["OPERATOR_ACTION", "ALERT"]);
        let ids = crate::correlate::Ids::new(&snap).unwrap();
        let chains = crate::correlate::analyze(&snap, &ids, crate::query::lookup(&snap, "INC-10").unwrap(), &ws.config.correlation, &[]).unwrap();
        assert_eq!(chains.apps.iter().map(|a| a.chain.as_ref().unwrap().root.kind.as_str()).collect::<Vec<_>>(), ["ALERT"]);
        let similar = crate::query::similar(&snap, "INC-11", &ws.config.correlation, 5, &mut Default::default()).unwrap();
        assert_eq!(similar[0].mitigations, ["ROLLBACK checkout at 2026-11-22T22:25:00Z: rolled back checkout"]);
    }

    fn two_chains() -> PathBuf {
        let alerts = [alert("ledger-db", "PoolSaturated", T - 600), alert("payments-api", "ErrorRate", T - 300)];
        let root = fixture(&format!("cite-{}", std::thread::current().name().unwrap_or("x").replace("::", "-")), &[("alerts", "alerts.json", &format!("[{}]", alerts.join(",")))]);
        harvest(&root, None);
        root
    }

    fn cite(root: &Path, text: &str) -> Vec<(String, String, String)> {
        let ws = Workspace::open(root).unwrap();
        let c = crate::cite::check(&ws.store.snapshot().unwrap(), text).unwrap();
        c.iter().map(|c| (format!("{:?}", c.kind), c.cited.clone(), format!("{:?}", c.status))).collect()
    }

    #[test]
    fn cite_check_tells_present_stale_and_unknown_citations() {
        let root = two_chains();
        let ranked = sparql(&root, "SELECT ?c WHERE { ?c corr:rank ?r } ORDER BY ?r");
        let (first, second) = (&ranked[0][0], &ranked[1][0]);
        let text = format!("1. ledger-db {first}\nINC-1 and INC-9 (and INC-9F) on payments-api, ledger-service and id:app/checkout\n{second} is rank 1\nid:chain/inc-1-0123456789abcdef and id:chain/inc-1-2\n");
        let s = |k: &str, c: &str, st: &str| (k.to_owned(), c.to_owned(), st.to_owned());
        assert_eq!(cite(&root, &text), [
            s("App", "ledger-db", "Present"),
            s("Chain", first, "Present"),
            s("Incident", "INC-1", "Present"),
            s("Incident", "INC-9", "Unknown"),
            s("Incident", "INC-9F", "Unknown"),
            s("App", "payments-api", "Present"),
            s("App", "ledger-service", "Unknown"),
            s("App", "id:app/checkout", "Present"),
            s("Chain", second, "Stale"),
            s("Chain", "id:chain/inc-1-0123456789abcdef", "Unknown"),
            s("Chain", "id:chain/inc-1-2", "Present"),
        ]);
        // A narrower window drops the payments-api chain while its alert stays in the graph.
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), config + "[correlation]\nwindow_s = 400\n").unwrap();
        harvest(&root, None);
        let pa = sparql(&root, "SELECT ?c WHERE { ?c corr:rank ?r }");
        assert_eq!(pa.len(), 1);
        let gone = if &pa[0][0] == first { second } else { first };
        assert_eq!(cite(&root, gone), [s("Chain", gone, "Stale")]);
    }

    #[test]
    fn cite_check_binds_a_rank_only_to_the_chain_it_is_written_next_to() {
        let root = two_chains();
        let ranked = sparql(&root, "SELECT ?c WHERE { ?c corr:rank ?r } ORDER BY ?r");
        let (first, second) = (&ranked[0][0], &ranked[1][0]);
        let status = |text: &str| cite(&root, text).into_iter().filter(|c| c.0 == "Chain").map(|c| (c.1, c.2)).collect::<Vec<_>>();
        let s = |c: &str, st: &str| (c.to_owned(), st.to_owned());
        assert_eq!(status(&format!("3. Check {first}")), [s(first, "Present")]);
        // As a SPARQL query writes it: `/` and `.` escaped.
        let escaped = first.replace("id:chain/", "id:chain\\/").replace('.', "\\.");
        assert_eq!(status(&format!("SELECT ?p WHERE {{ {escaped} ?p ?o }}")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} is rank 1 and {second} is rank 2")), [s(first, "Present"), s(second, "Present")]);
        assert_eq!(status(&format!("{first} is ranked 2")), [s(first, "Stale")]);
        assert_eq!(status(&format!("{first} (ranked #2)")), [s(first, "Stale")]);
        assert_eq!(status(&format!("{first} is ranked 1")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was ranked 2 before the deploy and is now ranked 1.")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 before the deploy and is now ranked 1.")), [s(first, "Present")]);
        assert_eq!(status(&format!("rank 1: {first}, rank 1: {second}")), [s(first, "Present"), s(second, "Stale")]);
        assert_eq!(status(&format!("{first} (rank 2), then {second}")), [s(first, "Stale"), s(second, "Present")]);
        assert_eq!(status(&format!("{second}, rank 2: {first}")), [s(second, "Present"), s(first, "Stale")]);
        // The rank after the first chain is its own, not the next one's too.
        assert_eq!(status(&format!("{first} is rank 1, {second} was ruled out")), [s(first, "Present"), s(second, "Present")]);
        // A rank before a chain is still its own, at the start of a line or after another chain's.
        assert_eq!(status(&format!("Rank 2 is {first}")), [s(first, "Stale")]);
        assert_eq!(status(&format!("{first} (rank 1), rank 1: {second}")), [s(first, "Present"), s(second, "Stale")]);
        assert_eq!(status(&format!("{first} is rank 1; rank 1 is {second}")), [s(first, "Present"), s(second, "Stale")]);
        // Only a chain takes a rank, and a rank that introduces the next chain is that chain's.
        assert_eq!(status(&format!("For id:app/ledger-db, rank 2 is {first}")), [s(first, "Stale")]);
        assert_eq!(status(&format!("{second}, rank 1 is {first}")), [s(second, "Present"), s(first, "Present")]);
        assert_eq!(status(&format!("{second}, rank 2 = {first}")), [s(second, "Present"), s(first, "Stale")]);
        assert_eq!(status(&format!("The rank 2 chain is {first}, rank 2 is {second}")), [s(first, "Stale"), s(second, "Present")]);
        // A dash separates list items; the rank before it stays with its chain.
        assert_eq!(status(&format!("- {first} is rank 1 - {second} is rank 2")), [s(first, "Present"), s(second, "Present")]);
        // Markdown, as agents write it.
        assert_eq!(status(&format!("1. `{first}` — rank 2, `{second}` (rank 2)")), [s(first, "Stale"), s(second, "Present")]);
        assert_eq!(status(&format!("- **Rank 2**: `{first}`")), [s(first, "Stale")]);
        assert_eq!(status(&format!("**Rank 2:** `{first}`")), [s(first, "Stale")]);
        assert_eq!(status(&format!("Previously at rank 2, `{first}` is now **rank 1**")), [s(first, "Present")]);
        assert_eq!(status(&format!("The rank-2 chain is {first}")), [s(first, "Stale")]);
        assert_eq!(status(&format!("{first} is rank 2: ledger-db pool saturation")), [s(first, "Stale")]);
        // A colon points forward only to a chain that follows on the line.
        assert_eq!(status(&format!("- {first} is rank 2:")), [s(first, "Stale")]);
        // ...and to a chain that states no rank of its own.
        assert_eq!(status(&format!("{second} is rank 1: {first} is rank 1")), [s(second, "Stale"), s(first, "Present")]);
        assert_eq!(status(&format!("{first} is rank 1: {second} follows it")), [s(first, "Present"), s(second, "Present")]);
        assert_eq!(status(&format!("{first}, the rank 2 chain is {second}")), [s(first, "Present"), s(second, "Present")]);
        assert_eq!(status(&format!("{second}, rank 2 is the chain: {first}")), [s(second, "Present"), s(first, "Stale")]);
        assert_eq!(status(&format!("Rank 1 chain: {first}, rank 2 chain: {second}")), [s(first, "Present"), s(second, "Present")]);
        assert_eq!(status(&format!("{second}, rank 2 chain is the {first}")), [s(second, "Present"), s(first, "Stale")]);
        assert_eq!(status(&format!("{second}, rank 2 is: {first}")), [s(second, "Present"), s(first, "Stale")]);
        assert_eq!(status(&format!("{first} was rank 2 before the reharvest and is now rank 1")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 (confidence 0.91) and is now rank 1.")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} is rank 1, and the pool chain was rank 3 but is now rank 2.")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 before the deploy, and now rank 1 is {second}.")), [s(first, "Stale"), s(second, "Stale")]);
        assert_eq!(status(&format!("{first} was rank 2 before the deploy and is now rank 1:")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was at rank 2 and is now at rank 1")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was rank 1, and the payments chain was rank 3 but is now rank 2.")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 before the Frankfurt failover and is now rank 1")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 in the old ranking and is now rank 1")), [s(first, "Present")]);
        // `moved from rank 2 to rank 1` already says the rank it has now.
        assert_eq!(status(&format!("{first} was moved from rank 2 to rank 1, and another was rank 1 but is now rank 4.")), [s(first, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2, but now rank 1 is held by {first}")), [s(second, "Present"), s(first, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 and is now rank 1, ahead of {second}")), [s(first, "Present"), s(second, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 and is now rank 1 isolated from {second}")), [s(first, "Present"), s(second, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2 and the chain that is now rank 1, {first}")), [s(second, "Present"), s(first, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2, behind the chain that is now rank 1.")), [s(second, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2, but now rank 1 is held by the pool chain.")), [s(second, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2, now rank 1 belongs to {first}")), [s(second, "Present"), s(first, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2 but is not now rank 1")), [s(second, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2 but isn't now rank 1")), [s(second, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 and now is rank 1")), [s(first, "Present")]);
        assert_eq!(status(&format!("{second} was rank 2 and isn’t now rank 1")), [s(second, "Present")]);
        assert_eq!(status(&format!("{first} was rank 2 and is now rank 1: {second} (rank 2) follows")), [s(first, "Present"), s(second, "Present")]);
        assert_eq!(status(&format!("{first} moved from rank 1 to rank 2")), [s(first, "Stale")]);
        assert_eq!(status(&format!("| `{first}` | **rank 2** |")), [s(first, "Stale")]);
        assert_eq!(status(&format!("{first}, not rank 2")), [s(first, "Present")]);
        // What a chain was is not what it is: only `which is` gives a rank.
        assert_eq!(status(&format!("{first}, which was rank 2 before the redeploy, is now rank 1")), [s(first, "Present")]);
        assert_eq!(status(&format!("{first}, which is rank 2")), [s(first, "Stale")]);
    }

    #[test]
    fn status_is_silent_only_when_the_harvest_is_fresh_and_every_chain_judged() {
        let root = fixture("status", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        let status = || crate::status::status(&Workspace::open(&root).unwrap()).unwrap();
        let before = status();
        assert_eq!(before.harvested_at, None);
        assert_eq!(before.changed, [".ograph/config.toml", ".ograph/ontology.ttl", ".ograph/shapes.ttl", "alerts.json", "incidents.json"]);
        harvest(&root, None);
        assert!(status().changed.is_empty());
        assert_eq!(status().unjudged, ["INC-1"]);
        assert_eq!(status().note().unwrap(), "ograph: 1 incidents have chains but no verdict (INC-1).");
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", true, "oncall", &["PM-1".into()]).unwrap();
        assert_eq!(status().note(), None);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("alerts.json"), format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600))).unwrap();
        assert_eq!(status().changed, ["alerts.json"]);
        assert!(status().note().unwrap().starts_with("ograph: the graph is stale: 1 source files changed since the harvest at "));
        harvest(&root, None);
        assert_eq!(status().note(), None);
    }

    #[test]
    fn status_compares_each_file_with_what_the_harvest_read_not_with_the_harvest_time() {
        let root = fixture("status-stamp", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        let alerts = root.join("alerts.json");
        let set_mtime = |t: std::time::SystemTime| std::fs::File::options().write(true).open(&alerts).unwrap().set_modified(t).unwrap();
        let hour = std::time::Duration::from_secs(3600);
        // A file dated ahead of the clock is read, and is not stale once read.
        set_mtime(std::time::SystemTime::now() + hour);
        harvest(&root, None);
        let changed = || crate::status::status(&Workspace::open(&root).unwrap()).unwrap().changed;
        assert!(changed().is_empty());
        // A copy that keeps an older timestamp, as `cp -p` or `rsync -a` do, is still a change.
        let then = std::fs::metadata(&alerts).unwrap().modified().unwrap() - hour * 2;
        std::fs::write(&alerts, format!("[{},{}]", alert("ledger-db", "PoolSaturated", T - 600), alert("ledger-db", "Latency", T - 500))).unwrap();
        set_mtime(then);
        assert_eq!(changed(), ["alerts.json"]);
    }

    #[test]
    fn status_reads_a_marker_that_lists_only_names() {
        let root = fixture("status-names", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let marker = root.join(".ograph").join(super::HARVESTED);
        let old: String = std::fs::read_to_string(&marker).unwrap().lines().map(|l| l.split('\t').next().unwrap().to_owned() + "\n").collect();
        std::fs::write(&marker, old).unwrap();
        let changed = || crate::status::status(&Workspace::open(&root).unwrap()).unwrap().changed;
        assert!(changed().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("alerts.json"), format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600))).unwrap();
        assert_eq!(changed(), ["alerts.json"]);
    }

    #[test]
    fn status_reports_a_source_file_deleted_or_renamed_since_the_harvest() {
        let root = fixture("status-gone", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        std::fs::create_dir_all(root.join("more")).unwrap();
        std::fs::write(root.join("more/a.json"), format!("[{}]", alert("payments-api", "ErrorRate", T - 300))).unwrap();
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), config + "[[source]]\nkind = \"alerts\"\npath = \"more\"\n").unwrap();
        harvest(&root, None);
        let changed = || crate::status::status(&Workspace::open(&root).unwrap()).unwrap().changed;
        assert!(changed().is_empty());
        std::fs::rename(root.join("more/a.json"), root.join("more/b.json")).unwrap();
        assert_eq!(changed(), ["more/a.json", "more/b.json"]);
        harvest(&root, None);
        assert!(changed().is_empty());
        std::fs::remove_file(root.join("alerts.json")).unwrap();
        assert_eq!(changed(), ["alerts.json"]);
    }

    #[test]
    fn ontology_diff_reports_verdict_facts_apart_from_harvest_quarantine() {
        let root = fixture("diff-verdict", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        ws.verdict("id:chain/inc-1-1", true, "oncall", &["PM-1".into()]).unwrap();
        let ttl: String = crate::ontology::DEFAULT_TTL.lines().filter(|l| !l.starts_with("rca:by a ")).map(|l| format!("{l}\n")).collect();
        let newer = crate::ontology::Ontology::load(&ttl, crate::ontology::DEFAULT_SHAPES).unwrap();
        let i = crate::gateway::impact(&ws.store.snapshot().unwrap(), &ws.ontology, &newer).unwrap();
        let graphs = |v: &[crate::gateway::Impact]| v.iter().map(|q| (q.graph.clone(), q.facts)).collect::<Vec<_>>();
        assert_eq!(graphs(&i.quarantine), []);
        let chain = sparql(&root, "SELECT ?c WHERE { ?c corr:rank 1 }")[0][0].replace("id:chain/", "g:harvest/verdicts/");
        assert_eq!(graphs(&i.verdicts), [(chain, 1)]);
        std::fs::write(root.join(".ograph/ontology.ttl"), &ttl).unwrap();
        let refused = format!("{:#}", Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", false, "lead", &["PM-2".into()]).unwrap_err());
        assert!(refused.contains("rca:by"), "{refused}");
    }

    #[test]
    fn resolve_follows_the_gateway_and_stops_at_five() {
        let root = two_chains();
        let ws = Workspace::open(&root).unwrap();
        let m: Vec<String> = ["ledger-db", "ledgr-db", "ledger-db-canary", "nothing-like-it", "id:app/checkout", "sixth"].map(str::to_owned).to_vec();
        let r = crate::query::resolve(&ws.store.snapshot().unwrap(), &m).unwrap();
        let got: Vec<(&str, String, Option<&str>)> = r.results.iter().map(|x| (x.mention.as_str(), format!("{:?}", x.status), x.app.as_deref())).collect();
        assert_eq!(got, [
            ("ledger-db", "Resolved".into(), Some("id:app/ledger-db")),
            ("ledgr-db", "Resolved".into(), Some("id:app/ledger-db")),
            ("ledger-db-canary", "Ambiguous".into(), None),
            ("nothing-like-it", "Unresolved".into(), None),
            ("id:app/checkout", "Resolved".into(), Some("id:app/checkout")),
        ]);
        assert_eq!(r.results[2].rationale, "only fuzzily matches id:app/ledger-db (0.914, under 0.93)");
        assert_eq!(r.skipped, ["sixth"]);
    }
}
