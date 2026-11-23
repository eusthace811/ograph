//! The investigation verbs on native paths: `before`, `blast`, `similar`, `why`, `news`.
//! SPARQL stays for ad hoc questions; these run on the indexes directly.

use crate::correlate::{self, Ids, Settings, app_name, literal};
use crate::store::{GraphSel, Snapshot};
use crate::term::{self, Id};
use crate::vocab::{self, disc, evt, harv, inc, ops};
use oxrdf::Term;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;

/// Finds a node from an IRI, a prefixed name, an incident id (`INC-5377`) or an app name.
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
    for p in [inc::ID, ops::NAME] {
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
    for &(x, _) in ups.iter().chain([(a, 0)].iter()) {
        if let Some(p) = supports {
            for proc_ in s.objects(x, p)? {
                processes.entry(app_name(s, &ids, proc_)).or_default().push(app_name(s, &ids, x));
            }
        }
        if let (Some(c), Some(st)) = (ids.incident, status) {
            for i in s.subjects(ids.on, x)? {
                if s.contains([i, ids.ty, c])? {
                    let state = s.object(i, st)?.and_then(|v| literal(s, v));
                    if state.as_deref() != Some("resolved") {
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
        let sim = cosine(&mine, &signature(s, &ids, other, st)?);
        let causes: Vec<Id> = cause_p.map(|p| s.objects(other, p)).transpose()?.unwrap_or_default();
        out.push(Similar {
            incident: vocab::compact(&literal(s, other).unwrap_or_default()),
            similarity: (sim.max(0.0) * 1000.0).round() / 1000.0,
            app: app_name(s, &ids, app),
            at: term::format_millis(at),
            cause_apps: causes.iter().filter_map(|&e| s.object(e, ids.on).ok().flatten()).map(|a| app_name(s, &ids, a)).collect(),
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
