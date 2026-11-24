//! The investigation verbs on native paths: `before`, `blast`, `similar`, `why`, `news`, `code`.
//! SPARQL stays for ad hoc questions; these run on the indexes directly.

use crate::correlate::{self, Ids, Settings, app_name, literal};
use crate::enrich::{ogrep, rel};
use crate::store::{GraphSel, Snapshot};
use crate::term::{self, Id};
use crate::vocab::{self, disc, evt, harv, inc, ops};
use oxrdf::Term;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Finds a node from an IRI, a prefixed name, an incident id (`INC-5377`), or an app's name or alias.
pub fn lookup(s: &Snapshot, name: &str) -> anyhow::Result<Id> {
    let iri = crate::workspace::resolve_iri(name);
    if let Some(id) = s.iri(&iri)? {
        return Ok(id);
    }
    for kind in ["inc", "app", "chain", "ev", "vendor", "process"] {
        if let Some(id) = s.iri(crate::model::entity(kind, name).as_str())? {
            return Ok(id);
        }
    }
    for p in [inc::ID, ops::NAME, ops::ALIAS] {
        if let (Some(p), Some(v)) = (s.iri(p)?, s.id(&oxrdf::Literal::new_simple_literal(name).into())?)
            && let Some(&i) = s.subjects(p, v)?.first()
        {
            return Ok(i);
        }
    }
    anyhow::bail!("nothing in the graph is called {name}")
}

#[derive(Serialize)]
pub struct Before {
    pub incident: String,
    pub app: String,
    pub at: String,
    pub scope: Vec<(String, usize)>,
    pub events: Vec<BeforeEvent>,
}

#[derive(Serialize)]
pub struct BeforeEvent {
    pub at: String,
    pub minutes_before: f64,
    pub app: String,
    pub distance: usize,
    pub signal: String,
    pub severity: f64,
    pub event: String,
    pub source_record: Option<String>,
}

/// Q1: which recent events could explain this incident.
pub fn before(s: &Snapshot, incident: &str, window_s: i64, depth: usize) -> anyhow::Result<Before> {
    let ids = Ids::new(s)?;
    let i = lookup(s, incident)?;
    let st = Settings { window_s, depth, ..Settings::default() };
    let (app, t, cands) = correlate::candidates(s, &ids, i, &st)?;
    let src = s.iri(evt::SOURCE_RECORD)?;
    let mut scope: Vec<(String, usize)> = correlate::downstream(s, &ids, app, depth)?.into_iter().map(|(a, d)| (app_name(s, &ids, a), d)).collect();
    scope.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    Ok(Before {
        incident: vocab::compact(&literal(s, i).unwrap_or_default()),
        app: app_name(s, &ids, app),
        at: term::format_millis(t),
        scope,
        events: cands
            .iter()
            .rev()
            .map(|c| BeforeEvent {
                at: term::format_millis(c.at_ms),
                minutes_before: ((t - c.at_ms) as f64 / 60_000.0 * 10.0).round() / 10.0,
                app: c.app_name.clone(),
                distance: c.distance,
                signal: c.kind.clone(),
                severity: (c.severity * 100.0).round() / 100.0,
                event: vocab::compact(&c.iri),
                source_record: src.and_then(|p| s.object(c.id, p).ok().flatten()).and_then(|v| literal(s, v)),
            })
            .collect(),
    })
}

#[derive(Serialize)]
pub struct Blast {
    pub app: String,
    pub dependents: Vec<(String, usize)>,
    pub processes: Vec<(String, Vec<String>)>,
    pub open_incidents: Vec<String>,
}

/// Q2: which services and business processes this failure might affect.
pub fn blast(s: &Snapshot, app: &str, depth: usize) -> anyhow::Result<Blast> {
    let ids = Ids::new(s)?;
    let a = lookup(s, app)?;
    let ups = correlate::upstream(s, &ids, a, depth)?;
    let mut processes: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let mut open = Vec::new();
    let supports = s.iri(ops::SUPPORTS)?;
    let status = s.iri(inc::STATUS)?;
    let resolved_at = s.iri(inc::RESOLVED_AT)?;
    for &(x, _) in ups.iter().chain([(a, 0)].iter()) {
        if let Some(p) = supports {
            for proc_ in s.objects(x, p)? {
                processes.entry(app_name(s, &ids, proc_)).or_default().push(app_name(s, &ids, x));
            }
        }
        if let Some(c) = ids.incident {
            for i in s.subjects(ids.on, x)? {
                if s.contains([i, ids.ty, c])? {
                    let state = match status {
                        Some(st) => s.object(i, st)?.and_then(|v| literal(s, v)),
                        None => None,
                    };
                    // A stated status decides, since a reopened incident can keep an old
                    // resolved time; without one, a resolved time means closed.
                    let closed = match state.as_deref() {
                        Some(st) => st.eq_ignore_ascii_case("resolved") || st.eq_ignore_ascii_case("closed"),
                        None => match resolved_at {
                            Some(r) => s.object(i, r)?.is_some(),
                            None => false,
                        },
                    };
                    if !closed {
                        open.push(vocab::compact(&literal(s, i).unwrap_or_default()));
                    }
                }
            }
        }
    }
    let mut processes: Vec<(String, Vec<String>)> = processes.into_iter().collect();
    processes.sort();
    open.sort();
    Ok(Blast { app: app_name(s, &ids, a), dependents: ups.into_iter().map(|(x, d)| (app_name(s, &ids, x), d)).collect(), processes, open_incidents: open })
}

#[derive(Serialize, Clone)]
pub struct Similar {
    pub incident: String,
    pub similarity: f64,
    pub app: String,
    pub at: String,
    pub causes: Vec<String>,
    pub cause_apps: Vec<String>,
    pub mitigations: Vec<String>,
    pub title: Option<String>,
}

/// An incident's signature: (application, signal) pairs seen before it, summed over their events'
/// strengths. Strength saturates at 1, so a max made every strongly anomalous application look
/// alike; the sum keeps how much of the onset each application carries.
pub fn signature(s: &Snapshot, ids: &Ids, i: Id, st: &Settings) -> anyhow::Result<FxHashMap<(String, String), f64>> {
    let st = Settings { window_s: st.signature_window_s, ..st.clone() };
    let (_, _, cands) = correlate::candidates(s, ids, i, &st)?;
    let mut sig: FxHashMap<(String, String), f64> = FxHashMap::default();
    for c in cands {
        *sig.entry((c.app_name, c.kind)).or_default() += 0.3 + 0.7 * c.severity;
    }
    Ok(sig)
}

fn cosine(a: &FxHashMap<(String, String), f64>, b: &FxHashMap<(String, String), f64>) -> f64 {
    let dot: f64 = a.iter().filter_map(|(k, v)| b.get(k).map(|w| v * w)).sum();
    let na: f64 = a.values().map(|v| v * v).sum::<f64>().sqrt();
    let nb: f64 = b.values().map(|v| v * v).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// The application of a cause: from its telemetry, or from the verdict that judged it once the
/// telemetry is gone.
fn cause_app(s: &Snapshot, ids: &Ids, e: Id) -> anyhow::Result<Option<Id>> {
    if let Some(a) = s.object(e, ids.on)? {
        return Ok(Some(a));
    }
    let (Some(root), Some(on_chain), Some(root_on)) = (s.iri(vocab::corr::ROOT_CAUSE)?, s.iri(vocab::rca::ON_CHAIN)?, s.iri(vocab::rca::ROOT_ON)?) else { return Ok(None) };
    for c in s.subjects(root, e)? {
        for v in s.subjects(on_chain, c)? {
            if let Some(a) = s.object(v, root_on)? {
                return Ok(Some(a));
            }
        }
    }
    Ok(None)
}

/// Q3: past incidents that resemble this one, with their accepted causes and mitigations.
pub fn similar(s: &Snapshot, incident: &str, st: &Settings, limit: usize) -> anyhow::Result<Vec<Similar>> {
    let ids = Ids::new(s)?;
    let i = lookup(s, incident)?;
    let (_, t) = correlate::incident_anchor(s, &ids, i)?;
    let mine = signature(s, &ids, i, st)?;
    let Some(c) = ids.incident else { return Ok(vec![]) };
    let (cause_p, mit_p, title_p) = (s.iri(inc::CAUSE)?, s.iri(inc::MITIGATION)?, s.iri(inc::TITLE)?);
    let mut out = Vec::new();
    for other in s.subjects(ids.ty, c)? {
        if other == i {
            continue;
        }
        let Ok((app, at)) = correlate::incident_anchor(s, &ids, other) else { continue };
        if at >= t {
            continue;
        }
        let similarity = (cosine(&mine, &signature(s, &ids, other, st)?).max(0.0) * 1000.0).round() / 1000.0;
        if similarity == 0.0 {
            continue;
        }
        let causes: Vec<Id> = cause_p.map(|p| s.objects(other, p)).transpose()?.unwrap_or_default();
        out.push(Similar {
            incident: vocab::compact(&literal(s, other).unwrap_or_default()),
            similarity,
            app: app_name(s, &ids, app),
            at: term::format_millis(at),
            cause_apps: causes.iter().map(|&e| cause_app(s, &ids, e)).collect::<anyhow::Result<Vec<_>>>()?.into_iter().flatten().map(|a| app_name(s, &ids, a)).collect(),
            causes: causes.iter().filter_map(|&e| literal(s, e)).map(|x| vocab::compact(&x)).collect(),
            mitigations: mit_p.map(|p| s.objects(other, p)).transpose()?.unwrap_or_default().into_iter().filter_map(|v| literal(s, v)).collect(),
            title: title_p.and_then(|p| s.object(other, p).ok().flatten()).and_then(|v| literal(s, v)),
        });
    }
    out.sort_by(|a, b| b.similarity.total_cmp(&a.similarity).then(b.at.cmp(&a.at)));
    out.truncate(limit);
    Ok(out)
}

#[derive(Serialize)]
pub struct Provenance {
    pub node: String,
    pub facts: Vec<FactProvenance>,
    pub quarantined: Vec<(String, String)>,
}

#[derive(Serialize)]
pub struct FactProvenance {
    pub predicate: String,
    pub object: String,
    pub graphs: Vec<GraphInfo>,
    pub model: Option<(String, String, String)>,
}

#[derive(Serialize, Clone)]
pub struct GraphInfo {
    pub graph: String,
    pub source: Option<String>,
    pub extractor: Option<String>,
    pub harvested_at: Option<String>,
    pub status: Option<String>,
}

fn graph_info(s: &Snapshot, g: Id, cache: &mut FxHashMap<Id, GraphInfo>) -> GraphInfo {
    if let Some(i) = cache.get(&g) {
        return i.clone();
    }
    let get = |p: &str| s.iri(p).ok().flatten().and_then(|pid| s.quads([Some(g), Some(pid), None], GraphSel::Named(g)).next()?.ok()).and_then(|q| literal(s, q[2]));
    let info = GraphInfo { graph: vocab::compact(&literal(s, g).unwrap_or_default()), source: get(harv::SOURCE), extractor: get(harv::EXTRACTOR), harvested_at: get(harv::AT), status: get(harv::STATUS) };
    cache.insert(g, info.clone());
    info
}

/// Provenance of every fact about a node: which harvest graphs assert it, their source,
/// extractor and time, model provenance for model-proposed facts, and quarantined proposals.
pub fn why(s: &Snapshot, node: &str) -> anyhow::Result<Provenance> {
    let n = lookup(s, node)?;
    let me = literal(s, n).unwrap_or_default();
    let mut cache = FxHashMap::default();
    let fact_p = s.iri(harv::FACT)?;
    let model_p = s.iri(harv::MODEL)?;
    let hash_p = s.iri(harv::PROMPT_HASH)?;
    let conf_p = s.iri(crate::vocab::corr::CONFIDENCE)?;
    let mut facts = Vec::new();
    for q in s.quads([Some(n), None, None], GraphSel::Union) {
        let [_, p, o, _] = q?;
        let graphs = s.graphs_of([n, p, o])?.into_iter().map(|g| graph_info(s, g, &mut cache)).collect();
        let triple = oxrdf::Triple::new(
            oxrdf::NamedNode::new_unchecked(&me),
            match s.term(p)? {
                Term::NamedNode(x) => x,
                _ => continue,
            },
            s.term(o)?,
        )
        .to_string();
        let model = match (fact_p, s.id(&oxrdf::Literal::new_simple_literal(&triple).into())?) {
            (Some(fp), Some(v)) => s.subjects(fp, v)?.first().map(|&f| {
                let g = |p: Option<Id>| p.and_then(|p| s.object(f, p).ok().flatten()).and_then(|v| literal(s, v)).unwrap_or_default();
                (g(model_p), g(hash_p), g(conf_p))
            }),
            _ => None,
        };
        facts.push(FactProvenance { predicate: vocab::compact(&literal(s, p).unwrap_or_default()), object: crate::sparql::show(&s.term(o)?), graphs, model });
    }
    let mut quarantined = Vec::new();
    if let (Some(fp), Some(rp)) = (fact_p, s.iri(harv::REASON)?) {
        let short = vocab::compact(&me);
        let ty = s.iri(crate::vocab::RDF_TYPE)?.ok_or_else(|| anyhow::anyhow!("empty store"))?;
        for q in s.quads([None, Some(fp), None], GraphSel::Union) {
            let [sub, _, o, _] = q?;
            let Some(text) = literal(s, o) else { continue };
            if (mentions(&text, &short) || mentions(&text, &me))
                && let Some(r) = s.object(sub, rp)?.and_then(|v| literal(s, v))
                    && s.object(sub, ty)?.is_some_and(|c| literal(s, c).as_deref() == Some(harv::QUARANTINED)) {
                        quarantined.push((text, r));
                    }
        }
    }
    Ok(Provenance { node: vocab::compact(&me), facts, quarantined })
}

/// `text` names `node` as a whole term, so `id:app/cart` is not found in `id:app/cartservice`.
fn mentions(text: &str, node: &str) -> bool {
    let name_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '%');
    text.match_indices(node).any(|(i, _)| {
        !text[..i].ends_with(name_char) && !text[i + node.len()..].starts_with(name_char)
    })
}

#[derive(Serialize)]
pub struct NewsHit {
    pub at: String,
    pub summary: String,
    pub vendor: String,
    pub apps: Vec<String>,
    pub processes: Vec<String>,
    pub related_threads: usize,
}

/// Q4: news items that mention a vendor our applications use, newest first, with the
/// applications and business processes behind each vendor.
pub fn news(s: &Snapshot, since_ms: Option<i64>) -> anyhow::Result<Vec<NewsHit>> {
    let ids = Ids::new(s)?;
    let (Some(item), Some(mentions), Some(uses)) = (s.iri(evt::NEWS_ITEM)?, s.iri(evt::MENTIONS)?, s.iri(ops::USES_VENDOR)?) else { return Ok(vec![]) };
    let summary = s.iri(evt::SUMMARY)?;
    let supports = s.iri(ops::SUPPORTS)?;
    let about = s.iri(disc::ABOUT)?;
    let mut out = Vec::new();
    for n in s.subjects(ids.ty, item)? {
        let at = s.object(n, ids.at)?.and_then(term::millis).unwrap_or(0);
        if since_ms.is_some_and(|x| at < x) {
            continue;
        }
        for v in s.objects(n, mentions)? {
            let apps = s.subjects(uses, v)?;
            if apps.is_empty() {
                continue;
            }
            let mut procs: FxHashSet<String> = FxHashSet::default();
            let mut threads = 0;
            for &a in &apps {
                if let Some(p) = supports {
                    procs.extend(s.objects(a, p)?.into_iter().map(|x| app_name(s, &ids, x)));
                }
                if let Some(p) = about {
                    threads += s.subjects(p, a)?.len();
                }
            }
            let mut processes: Vec<String> = procs.into_iter().collect();
            processes.sort();
            out.push(NewsHit {
                at: term::format_millis(at),
                summary: summary.and_then(|p| s.object(n, p).ok().flatten()).and_then(|x| literal(s, x)).unwrap_or_default(),
                vendor: app_name(s, &ids, v),
                apps: apps.iter().map(|&a| app_name(s, &ids, a)).collect(),
                processes,
                related_threads: threads,
            });
        }
    }
    out.sort_by(|a, b| b.at.cmp(&a.at));
    Ok(out)
}

/// A duration such as `90`, `90s`, `10m`, `2h` or `1d`, in seconds.
pub fn duration_s(s: &str) -> anyhow::Result<i64> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse()?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => anyhow::bail!("unknown unit in {s}"),
    };
    n.checked_mul(scale).ok_or_else(|| anyhow::anyhow!("duration {s} is too long"))
}

#[derive(Serialize)]
pub struct Code {
    pub incident: String,
    pub app: String,
    pub at: String,
    /// Candidate events that carry a line of code, newest first.
    pub locations: Vec<CodeLocation>,
    /// Candidate commits: those that changed a located file first, then newest first.
    pub commits: Vec<CodeCommit>,
    /// The files the incident's description is about, best first.
    pub files: Vec<CodeFile>,
    /// `confident` or `unsure`: whether ogrep's first file stands clear of the rest.
    pub files_confidence: Option<String>,
    /// Past incident write-ups in the code repositories that share words with this one.
    pub writeups: Vec<Writeup>,
    pub degraded: Vec<String>,
}

#[derive(Serialize)]
pub struct CodeLocation {
    pub event: String,
    pub app: String,
    pub signal: String,
    pub at: String,
    pub location: String,
    pub summary: Option<String>,
}

#[derive(Serialize)]
pub struct CodeCommit {
    pub event: String,
    pub app: String,
    pub at: String,
    pub minutes_before: f64,
    pub revision: Option<String>,
    pub summary: Option<String>,
    pub files: Vec<String>,
    /// The locations of this answer in a file the commit changed.
    pub touches: Vec<String>,
}

#[derive(Serialize)]
pub struct CodeFile {
    pub path: String,
    pub score: f64,
    pub line: Option<u64>,
}

#[derive(Serialize)]
pub struct Writeup {
    pub path: String,
    pub score: f64,
    pub terms: Vec<String>,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The graph half of `ograph code`: where an incident's candidate events point in the code and
/// which candidate commits changed those files. The second value describes the incident in
/// words, for [`code_search`]: its title, then the applications and messages along its
/// top-ranked chain.
pub fn code(s: &Snapshot, incident: &str, st: &Settings) -> anyhow::Result<(Code, String)> {
    let ids = Ids::new(s)?;
    let i = lookup(s, incident)?;
    let (app, t, cands) = correlate::candidates(s, &ids, i, st)?;
    let (template, summary, revision, title) = (s.iri(evt::TEMPLATE)?, s.iri(evt::SUMMARY)?, s.iri(evt::REVISION)?, s.iri(inc::TITLE)?);
    let text = |e: Id, p: Option<Id>| p.and_then(|p| s.object(e, p).ok().flatten()).and_then(|v| literal(s, v));
    let describe = |e: Id| text(e, template).or_else(|| text(e, summary));
    let (mut locations, mut commits) = (Vec::new(), Vec::new());
    for c in cands.iter().rev() {
        let at = term::format_millis(c.at_ms);
        let mut found = correlate::texts(s, c.id, ids.code_location)?;
        found.sort();
        locations.extend(found.into_iter().map(|location| CodeLocation { event: vocab::compact(&c.iri), app: c.app_name.clone(), signal: c.kind.clone(), at: at.clone(), location, summary: describe(c.id) }));
        if c.kind == "COMMIT" {
            let mut files = c.changed.clone();
            files.sort();
            commits.push(CodeCommit { event: vocab::compact(&c.iri), app: c.app_name.clone(), at, minutes_before: ((t - c.at_ms) as f64 / 60_000.0 * 10.0).round() / 10.0, revision: text(c.id, revision), summary: describe(c.id), files, touches: vec![] });
        }
    }
    for c in &mut commits {
        for l in &locations {
            if c.files.iter().any(|f| f == correlate::location_file(&l.location)) && !c.touches.contains(&l.location) {
                c.touches.push(l.location.clone());
            }
        }
    }
    commits.sort_by_key(|c| c.touches.is_empty());
    let mut words: Vec<String> = text(i, title).into_iter().collect();
    let analysis = correlate::analyze(s, &ids, i, st)?;
    if let Some(chain) = analysis.apps.first().and_then(|r| r.chain.as_ref()) {
        for link in std::iter::once(&chain.root).chain(&chain.links) {
            words.push(link.app_name.clone());
            words.extend(describe(link.id));
        }
    }
    let words = words.join(" ").replace("<*>", " ").split_whitespace().collect::<Vec<_>>().join(" ");
    let answer = Code { incident: vocab::compact(&literal(s, i).unwrap_or_default()), app: app_name(s, &ids, app), at: term::format_millis(t), locations, commits, files: vec![], files_confidence: None, writeups: vec![], degraded: vec![] };
    Ok((answer, words))
}

/// The ogrep half of `ograph code`: the files and the past write-ups `text` is about, from
/// every code repository, merged by score. A repository ogrep cannot answer for is named in
/// `degraded` and the others still count.
pub fn code_search(answer: &mut Code, text: &str, root: &Path, repos: &[PathBuf], top: usize) {
    if text.is_empty() || top == 0 {
        return;
    }
    // The confidence of the repository whose first file ranks first overall.
    let mut first: Option<(f64, Option<String>)> = None;
    for repo in repos {
        let within = |path: &str| rel(root, &repo.join(path));
        let mut failed = |e: ogrep::Failure| {
            answer.degraded.push(format!("{}: {e}", rel(root, repo)));
            matches!(e, ogrep::Failure::NotRunnable(_))
        };
        match ogrep::about(repo, text, top) {
            Ok(about) => {
                if let Some(f) = about.files.first().filter(|f| first.as_ref().is_none_or(|(score, _)| f.score > *score)) {
                    first = Some((f.score, about.confidence.clone()));
                }
                answer.files.extend(about.files.into_iter().map(|f| CodeFile { path: within(&f.path), score: f.score, line: f.line }));
            }
            Err(e) => {
                if failed(e) {
                    break;
                }
            }
        }
        match ogrep::similar_incidents(repo, text) {
            Ok(found) => answer.writeups.extend(found.into_iter().map(|w| Writeup { path: within(&w.path), score: w.score, terms: w.terms })),
            Err(e) => {
                if failed(e) {
                    break;
                }
            }
        }
    }
    answer.files.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.path.cmp(&b.path)));
    answer.files.truncate(top);
    answer.writeups.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.path.cmp(&b.path)));
    answer.writeups.truncate(top);
    for score in answer.files.iter_mut().map(|f| &mut f.score).chain(answer.writeups.iter_mut().map(|w| &mut w.score)) {
        *score = round2(*score);
    }
    answer.files_confidence = first.and_then(|(_, level)| level);
}

/// The applications `ograph suspects` asks ogrep about, and how far back it looks by default.
pub struct Failing {
    pub apps: Vec<String>,
    pub since: String,
}

/// The graph half of `ograph suspects`. An incident fails its own application and those of the
/// other incidents opened within the correlation window of it, and looks back to the day
/// before it. Anything else is a comma-separated list of application names, looking back 24
/// hours; a name the catalog knows by an alias becomes its catalog name.
pub fn failing(s: &Snapshot, target: &str, st: &Settings) -> anyhow::Result<Failing> {
    let ids = Ids::new(s).ok();
    let is_a = |node: Id, class: Option<Id>| ids.as_ref().zip(class).is_some_and(|(ids, c)| s.contains([node, ids.ty, c]).unwrap_or(false));
    if let (Some(ids), Ok(i)) = (&ids, lookup(s, target))
        && is_a(i, ids.incident)
    {
        let (app, t) = correlate::incident_anchor(s, ids, i)?;
        let mut others = Vec::new();
        for other in ids.incident.map(|c| s.subjects(ids.ty, c)).transpose()?.unwrap_or_default() {
            if let Ok((a, at)) = correlate::incident_anchor(s, ids, other)
                && a != app
                && (at - t).abs() <= st.window_s * 1000
            {
                others.push(app_name(s, ids, a));
            }
        }
        others.sort();
        others.dedup();
        let apps = std::iter::once(app_name(s, ids, app)).chain(others).collect();
        return Ok(Failing { apps, since: term::format_millis(t - 86_400_000).chars().take(10).collect() });
    }
    let application = s.iri(ops::APPLICATION)?;
    let apps: Vec<String> = target
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| match (&ids, lookup(s, n)) {
            (Some(ids), Ok(a)) if is_a(a, application) => app_name(s, ids, a),
            _ => n.to_owned(),
        })
        .collect();
    anyhow::ensure!(!apps.is_empty(), "suspects needs an incident or application names");
    Ok(Failing { apps, since: "24h".into() })
}

#[derive(Serialize, Clone)]
pub struct Suspect {
    pub rank: usize,
    pub app: String,
    pub score: f64,
    /// Dependency hops from the failing applications.
    pub distance: f64,
    /// The failing applications this one could explain.
    pub explains: Vec<String>,
    pub changes: Vec<SuspectChange>,
}

#[derive(Serialize, Clone)]
pub struct SuspectChange {
    pub revision: String,
    pub summary: String,
    pub at: String,
    pub files: Vec<String>,
}

/// The ogrep half of `ograph suspects`: likely causes from the dependency graph and the commits
/// since `since`, from the first of `repos` ogrep answers for. The second value says why the
/// repositories before it gave nothing.
pub fn suspects(root: &Path, repos: &[PathBuf], failing: &[String], since: &str) -> (Vec<Suspect>, Vec<String>) {
    let mut degraded = Vec::new();
    for repo in repos {
        match ogrep::suspects(repo, failing, since) {
            Ok(found) => {
                let change = |c: ogrep::Commit| SuspectChange { revision: c.sha, summary: c.subject, at: term::format_millis(c.time * 1000), files: c.files.iter().map(|f| rel(root, &repo.join(f))).collect() };
                let out = found.into_iter().enumerate().map(|(k, x)| Suspect { rank: k + 1, app: x.service, score: round2(x.score), distance: x.distance, explains: x.explains, changes: x.changes.into_iter().map(change).collect() });
                return (out.collect(), degraded);
            }
            Err(e) => {
                degraded.push(format!("{}: {e}", rel(root, repo)));
                if matches!(e, ogrep::Failure::NotRunnable(_)) {
                    break;
                }
            }
        }
    }
    (vec![], degraded)
}

/// What `ograph chains` answers: the analysis and, when telemetry ranked no application, the
/// suspects the dependency graph and recent commits give.
#[derive(Serialize)]
pub struct Chains {
    #[serde(flatten)]
    pub analysis: correlate::Analysis,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suspects: Vec<Suspect>,
}
