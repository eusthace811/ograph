//! The investigation verbs on native paths: `before`, `blast`, `similar`, `why`, `uses`, `news`, `code`.
//! SPARQL stays for ad hoc questions; these run on the indexes directly.

use crate::correlate::{self, Ids, Settings, app_name, literal};
use crate::enrich::{ogrep, rel};
use crate::store::{GraphSel, Snapshot};
use crate::term::{self, Id};
use crate::vocab::{self, corr, disc, evt, harv, inc, ops, rca};
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<correlate::Withheld>,
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
    let mut scope: Vec<(String, usize)> = correlate::downstream(s, &ids, app, depth, Some(t))?.into_iter().map(|(a, d)| (app_name(s, &ids, a), d)).collect();
    scope.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    Ok(Before {
        incident: vocab::compact(&literal(s, i).unwrap_or_default()),
        app: app_name(s, &ids, app),
        at: term::format_millis(t),
        scope,
        withheld: correlate::withheld(s, &ids, app, depth, t)?,
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

/// The time `when` names: an incident's opening time, or a time as `--known-at` takes it.
pub fn time_of(s: &Snapshot, when: &str) -> anyhow::Result<i64> {
    if let Ok(i) = lookup(s, when)
        && let Some(at) = s.iri(evt::AT)?
        && let Some(t) = s.object(i, at)?.and_then(term::millis)
    {
        return Ok(t);
    }
    crate::workspace::when_ms(when)
}

#[derive(Serialize)]
pub struct Blast {
    pub app: String,
    /// The time whose dependencies were followed, when one was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub dependents: Vec<(String, usize)>,
    pub processes: Vec<(String, Vec<String>)>,
    pub open_incidents: Vec<String>,
}

/// Q2: which services and business processes this failure might affect. With `at` (Unix ms),
/// only through dependencies that held then.
pub fn blast(s: &Snapshot, app: &str, depth: usize, at: Option<i64>) -> anyhow::Result<Blast> {
    let ids = Ids::new(s)?;
    let a = lookup(s, app)?;
    let ups = correlate::upstream(s, &ids, a, depth, at)?;
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
    Ok(Blast { app: app_name(s, &ids, a), at: at.map(term::format_millis), dependents: ups.into_iter().map(|(x, d)| (app_name(s, &ids, x), d)).collect(), processes, open_incidents: open })
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
    /// Roots people rejected on that incident: what it was found not to be.
    pub ruled_out: Vec<RuledOut>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct RuledOut {
    pub app: String,
    pub signal: String,
    pub evidence: Vec<String>,
}

/// An incident's signature: (application, signal) pairs seen before it, summed over their events'
/// strengths. Strength saturates at 1, so a max made every strongly anomalous application look
/// alike; the sum keeps how much of the onset each application carries.
pub fn signature(s: &Snapshot, ids: &Ids, i: Id, st: &Settings) -> anyhow::Result<FxHashMap<(String, String), f64>> {
    let st = Settings { window_s: st.signature_window_s, ..st.clone() };
    let (_, _, cands) = correlate::causes(s, ids, i, &st)?;
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

/// One closed-vocabulary fact an incident states about itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Stated {
    On(Id),
    FaultType(Id),
    FaultClass(Id),
}

/// An incident's stated signature: the application it is on, its stated fault type and the
/// fault class proposed from its title. Every value is an application of the catalog or a
/// closed-vocabulary literal, so equal means the same. Accepted causes are left out: an
/// incident's own verdict would then pick the past incidents that share its answer.
fn statement(s: &Snapshot, ids: &Ids, i: Id) -> anyhow::Result<FxHashSet<Stated>> {
    let mut out = FxHashSet::default();
    if let Some(a) = s.object(i, ids.on)? {
        out.insert(Stated::On(a));
    }
    if let Some(p) = s.iri(inc::FAULT_TYPE)? {
        out.extend(s.objects(i, p)?.into_iter().map(Stated::FaultType));
    }
    if let Some(p) = s.iri(inc::FAULT_CLASS)? {
        let unknown = s.id(&oxrdf::Literal::new_simple_literal("UNKNOWN").into())?;
        out.extend(s.objects(i, p)?.into_iter().filter(|v| Some(*v) != unknown).map(Stated::FaultClass));
    }
    Ok(out)
}

fn overlap(a: &FxHashSet<Stated>, b: &FxHashSet<Stated>) -> f64 {
    if a.is_empty() || b.is_empty() { 0.0 } else { a.intersection(b).count() as f64 / ((a.len() * b.len()) as f64).sqrt() }
}

/// Reciprocal rank fusion (Cormack et al. 2009, k = 60) of two scores per item. Equal scores
/// share a rank, so a ranking that cannot tell items apart leaves the other's order alone.
fn fuse(scores: &[(f64, f64)]) -> Vec<f64> {
    let rank = |pick: fn(&(f64, f64)) -> f64| {
        let mut sorted: Vec<f64> = scores.iter().map(pick).collect();
        sorted.sort_by(|x, y| y.total_cmp(x));
        move |v: f64| 1.0 + sorted.partition_point(|&w| w > v) as f64
    };
    let (ra, rb) = (rank(|x| x.0), rank(|x| x.1));
    scores.iter().map(|&(a, b)| 1.0 / (60.0 + ra(a)) + 1.0 / (60.0 + rb(b))).collect()
}

/// The application of a cause: from its telemetry, or from the verdict that judged it once the
/// telemetry is gone.
fn cause_app(s: &Snapshot, ids: &Ids, e: Id) -> anyhow::Result<Option<Id>> {
    if let Some(a) = s.object(e, ids.on)? {
        return Ok(Some(a));
    }
    let (Some(root), Some(root_on)) = (s.iri(corr::ROOT_CAUSE)?, s.iri(rca::ROOT_ON)?) else { return Ok(None) };
    for c in s.subjects(root, e)? {
        if let Some(&v) = verdicts(s, c)?.last()
            && let Some(a) = s.object(v, root_on)?
        {
            return Ok(Some(a));
        }
    }
    Ok(None)
}

/// What people did in an incident, by time: the actions whose thread names it, and the actions
/// on its application from when it opened until it was resolved, or for the correlation window
/// when it has no resolved time. Each reads `KIND app at TIME: words`.
fn actions_in(s: &Snapshot, ids: &Ids, incident: Id, app: Id, at: i64, st: &Settings) -> anyhow::Result<Vec<String>> {
    let (Some(class), Some(kind_p)) = (s.iri(evt::OPERATOR_ACTION)?, s.iri(evt::ACTION_KIND)?) else { return Ok(vec![]) };
    let until = s.iri(inc::RESOLVED_AT)?.and_then(|p| s.object(incident, p).ok().flatten()).and_then(term::millis).unwrap_or(at + st.window_s * 1000);
    let mut found: Vec<Id> = s.events_on(app, term::time_id(at), term::time_id(until))?.into_iter().map(|(_, e)| e).collect();
    if let Some(p) = s.iri(evt::DURING)? {
        found.extend(s.subjects(p, incident)?);
    }
    let summary = s.iri(evt::SUMMARY)?;
    let mut out = Vec::new();
    for e in found {
        if !s.contains([e, ids.ty, class])? {
            continue;
        }
        let (Some(t), Some(on)) = (s.object(e, ids.at)?.and_then(term::millis), s.object(e, ids.on)?) else { continue };
        let get = |p: Option<Id>| p.and_then(|p| s.object(e, p).ok().flatten()).and_then(|v| literal(s, v)).unwrap_or_default();
        out.push((t, format!("{} {} at {}: {}", get(Some(kind_p)), app_name(s, ids, on), term::format_millis(t), get(summary))));
    }
    out.sort();
    out.dedup();
    Ok(out.into_iter().map(|(_, m)| m).collect())
}

/// A root a verdict judged: its application and signal as the verdict kept them, so they
/// outlive the root's telemetry.
struct JudgedRoot {
    accepted: bool,
    app: Option<Id>,
    signal: String,
    evidence: Vec<String>,
}

/// The current verdict on each chain of `incident`; a verdict a later one superseded is history.
fn judged_roots(s: &Snapshot, incident: Id) -> anyhow::Result<Vec<JudgedRoot>> {
    let (Some(leads), Some(state)) = (s.iri(vocab::corr::LEADS_TO)?, s.iri(vocab::rca::STATE)?) else { return Ok(vec![]) };
    let (root_on, root_signal, root_class, evidence) = (s.iri(vocab::rca::ROOT_ON)?, s.iri(vocab::rca::ROOT_SIGNAL)?, s.iri(vocab::rca::ROOT_CLASS)?, s.iri(vocab::rca::EVIDENCE)?);
    let mut out = Vec::new();
    for chain in s.subjects(leads, incident)? {
        if let Some(&v) = verdicts(s, chain)?.last() {
            let text = |p: Option<Id>| p.and_then(|p| s.object(v, p).ok().flatten()).and_then(|o| literal(s, o));
            let Some(state) = text(Some(state)) else { continue };
            let signal = text(root_signal).or_else(|| text(root_class).map(|c| c.rsplit('#').next().unwrap_or("").to_uppercase())).unwrap_or_default();
            let mut evidence = correlate::texts(s, v, evidence)?;
            evidence.sort();
            out.push(JudgedRoot { accepted: state == "accepted", app: root_on.and_then(|p| s.object(v, p).ok().flatten()), signal, evidence });
        }
    }
    Ok(out)
}

/// Incidents that resemble `i`, best match first: the incident, its similarity, application
/// and time. Past ones only, unless `past_any_time`. An incident with telemetry is compared by
/// its telemetry signature, and the stated signature reorders the matches by rank fusion; one
/// without telemetry is compared by what it states. The similarity is the cosine of the
/// signature that chose the candidates.
fn resembling(s: &Snapshot, ids: &Ids, i: Id, st: &Settings) -> anyhow::Result<Vec<(Id, f64, Id, i64)>> {
    let (_, t) = correlate::incident_anchor(s, ids, i)?;
    let mine = signature(s, ids, i, st)?;
    let stated = statement(s, ids, i)?;
    let Some(c) = ids.incident else { return Ok(vec![]) };
    let mut out = Vec::new();
    let mut scores = Vec::new();
    for other in s.subjects(ids.ty, c)? {
        if other == i {
            continue;
        }
        let Ok((app, at)) = correlate::incident_anchor(s, ids, other) else { continue };
        if at >= t && !st.past_any_time {
            continue;
        }
        let round = |x: f64| (x.max(0.0) * 1000.0).round() / 1000.0;
        let told = round(overlap(&stated, &statement(s, ids, other)?));
        let similarity = if mine.is_empty() { told } else { round(cosine(&mine, &signature(s, ids, other, st)?)) };
        if similarity == 0.0 {
            continue;
        }
        scores.push((similarity, told));
        out.push((other, similarity, app, at));
    }
    let mut ranked: Vec<(f64, (Id, f64, Id, i64))> = fuse(&scores).into_iter().zip(out).collect();
    ranked.sort_by(|(fx, x), (fy, y)| fy.total_cmp(fx).then(y.1.total_cmp(&x.1)).then(y.3.cmp(&x.3)));
    Ok(ranked.into_iter().map(|(_, x)| x).collect())
}

/// Q3: past incidents that resemble this one, with their accepted causes and mitigations, and
/// the roots people rejected on them.
pub fn similar(s: &Snapshot, incident: &str, st: &Settings, limit: usize) -> anyhow::Result<Vec<Similar>> {
    let ids = Ids::new(s)?;
    let i = lookup(s, incident)?;
    let (cause_p, mit_p, title_p) = (s.iri(inc::CAUSE)?, s.iri(inc::MITIGATION)?, s.iri(inc::TITLE)?);
    let mut out = Vec::new();
    for (other, similarity, app, at) in resembling(s, &ids, i, st)?.into_iter().take(limit) {
        let causes: Vec<Id> = cause_p.map(|p| s.objects(other, p)).transpose()?.unwrap_or_default();
        let mut ruled_out: Vec<RuledOut> = judged_roots(s, other)?.into_iter().filter(|v| !v.accepted).map(|v| RuledOut { app: v.app.map(|a| app_name(s, &ids, a)).unwrap_or_default(), signal: v.signal, evidence: v.evidence }).collect();
        ruled_out.sort_by(|a, b| (&a.app, &a.signal).cmp(&(&b.app, &b.signal)));
        out.push(Similar {
            incident: vocab::compact(&literal(s, other).unwrap_or_default()),
            similarity,
            app: app_name(s, &ids, app),
            at: term::format_millis(at),
            cause_apps: causes.iter().map(|&e| cause_app(s, &ids, e)).collect::<anyhow::Result<Vec<_>>>()?.into_iter().flatten().map(|a| app_name(s, &ids, a)).collect(),
            causes: causes.iter().filter_map(|&e| literal(s, e)).map(|x| vocab::compact(&x)).collect(),
            mitigations: mit_p.map(|p| s.objects(other, p)).transpose()?.unwrap_or_default().into_iter().filter_map(|v| literal(s, v)).chain(actions_in(s, &ids, other, app, at, st)?).collect(),
            title: title_p.and_then(|p| s.object(other, p).ok().flatten()).and_then(|v| literal(s, v)),
            ruled_out,
        });
    }
    Ok(out)
}

/// How many similar incidents with an accepted cause `chains` consults.
const PAST_TOP: usize = 3;

/// The accepted causes of the `PAST_TOP` incidents most like `i` that have one, most similar
/// first. Empty, and free, in a store without verdicts.
pub fn past(s: &Snapshot, ids: &Ids, i: Id, st: &Settings) -> anyhow::Result<Vec<correlate::PastRoot>> {
    let mut past = Vec::new();
    if s.iri(vocab::rca::STATE)?.is_none() {
        return Ok(past);
    }
    let mut incidents = 0;
    for (other, similarity, _, _) in resembling(s, ids, i, st)? {
        let incident = vocab::compact(&literal(s, other).unwrap_or_default());
        let mut roots: Vec<correlate::PastRoot> = judged_roots(s, other)?
            .into_iter()
            .filter(|v| v.accepted)
            .map(|v| correlate::PastRoot { incident: incident.clone(), similarity, app: v.app.map(|a| app_name(s, ids, a)).unwrap_or_default(), signal: v.signal, evidence: v.evidence })
            .collect();
        if roots.is_empty() {
            continue;
        }
        roots.sort_by(|a, b| (&a.app, &a.signal).cmp(&(&b.app, &b.signal)));
        past.extend(roots);
        incidents += 1;
        if incidents == PAST_TOP {
            break;
        }
    }
    Ok(past)
}

/// `ograph chains`: the chains of an incident, ranked and marked with the causes people accepted
/// on the incidents most like it.
pub fn chains(s: &Snapshot, ids: &Ids, i: Id, st: &Settings) -> anyhow::Result<correlate::Analysis> {
    correlate::analyze(s, ids, i, st, &past(s, ids, i, st)?)
}

#[derive(Serialize)]
pub struct Provenance {
    pub node: String,
    pub facts: Vec<FactProvenance>,
    pub quarantined: Vec<(String, String)>,
    /// For a chain, every verdict on it, oldest first; the last is the current one.
    pub verdicts: Vec<VerdictEvent>,
}

#[derive(Serialize)]
pub struct VerdictEvent {
    pub verdict: String,
    pub state: String,
    pub by: String,
    pub at: String,
    pub evidence: Vec<String>,
    /// The rank the chain had when it was judged, if correlation ranked it then.
    pub rank: Option<String>,
    pub superseded_by: Option<String>,
}

/// The verdicts on a chain, oldest first: each one but the last is `rca:supersededBy` the next.
pub fn verdicts(s: &Snapshot, chain: Id) -> anyhow::Result<Vec<Id>> {
    let Some(on) = s.iri(rca::ON_CHAIN)? else { return Ok(vec![]) };
    let mut events = s.subjects(on, chain)?;
    events.sort_unstable();
    let next: FxHashMap<Id, Id> = match s.iri(rca::SUPERSEDED_BY)? {
        Some(p) => events.iter().filter_map(|&v| s.object(v, p).ok().flatten().map(|n| (v, n))).collect(),
        None => FxHashMap::default(),
    };
    let later: FxHashSet<Id> = next.values().copied().collect();
    let mut order: Vec<Id> = Vec::with_capacity(events.len());
    let mut cur = events.iter().copied().find(|v| !later.contains(v));
    while let Some(v) = cur.filter(|v| !order.contains(v)) {
        order.push(v);
        cur = next.get(&v).copied();
    }
    // A history the links do not order (an edited store) keeps every verdict, by id.
    order.extend(events.into_iter().filter(|v| !order.contains(v)).collect::<Vec<_>>());
    Ok(order)
}

fn verdict_event(s: &Snapshot, v: Id) -> anyhow::Result<VerdictEvent> {
    let one = |p: &str| -> anyhow::Result<Option<String>> { Ok(s.iri(p)?.and_then(|p| s.object(v, p).ok().flatten()).and_then(|o| literal(s, o))) };
    let mut evidence: Vec<String> = match s.iri(rca::EVIDENCE)? {
        Some(p) => s.objects(v, p)?.into_iter().filter_map(|o| literal(s, o)).collect(),
        None => vec![],
    };
    evidence.sort();
    Ok(VerdictEvent {
        verdict: vocab::compact(&literal(s, v).unwrap_or_default()),
        state: one(rca::STATE)?.unwrap_or_default(),
        by: one(rca::BY)?.unwrap_or_default(),
        at: one(rca::AT)?.unwrap_or_default(),
        evidence,
        rank: one(rca::RANK)?,
        superseded_by: one(rca::SUPERSEDED_BY)?.map(|x| vocab::compact(&x)),
    })
}

#[derive(Serialize)]
pub struct Replay {
    /// The hash of `config.toml`'s settings and of the settings replayed, as chains store them.
    pub config_settings: String,
    pub settings: String,
    /// Stored chains ranked under other settings than `config.toml`'s: a harvest would move them.
    pub stale_chains: usize,
    pub verdicts: Vec<Replayed>,
    /// Verdicts the configured settings agree with and the replayed settings contradict.
    pub flips: usize,
    /// The reverse: verdicts only the replayed settings agree with.
    pub fixed: usize,
    /// Verdicts the replayed settings contradict, flipped or not.
    pub contradicted: usize,
}

#[derive(Serialize)]
pub struct Replayed {
    pub incident: String,
    pub chain: String,
    pub state: String,
    pub app: String,
    /// The chain's rank when it was judged.
    pub judged_rank: Option<String>,
    /// Under the configured settings, then under the replayed ones.
    pub before: Judged,
    pub after: Judged,
    /// "lost rank 1" or "dropped out" for an accepted chain, "rose to rank 1" for a rejected one,
    /// when the configured settings agree with the verdict and the replayed ones do not.
    pub flip: Option<&'static str>,
}

#[derive(Serialize)]
pub struct Judged {
    /// The rank of the judged root's application, and of the judged chain.
    pub app_rank: Option<usize>,
    pub chain_rank: Option<usize>,
    pub contradicts: Option<Contradiction>,
}

/// How a ranking contradicts a verdict.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Contradiction {
    /// An accepted root's application ranks, but not first.
    BelowRank1,
    /// An accepted root's application is not among the `max_chains` a harvest stores.
    DroppedOut,
    /// A rejected chain ranks first.
    AtRank1,
}

impl Contradiction {
    pub fn name(self) -> &'static str {
        match self {
            Self::BelowRank1 => "below rank 1",
            Self::DroppedOut => "dropped out",
            Self::AtRank1 => "at rank 1",
        }
    }

    /// What a change of settings that brings it about does to the verdict.
    pub fn flip(self) -> &'static str {
        match self {
            Self::BelowRank1 => "lost rank 1",
            Self::DroppedOut => "dropped out",
            Self::AtRank1 => "rose to rank 1",
        }
    }
}

/// An accepted verdict holds while its root's application ranks first, as `chains` would print
/// it; it has dropped out when that application is not among the `max_chains` a harvest stores.
/// A rejected verdict condemns its chain, not the application, so only that chain at rank 1
/// contradicts it.
fn judge(state: &str, a: Option<&correlate::Analysis>, app: &str, chain: &str, max_chains: usize) -> Judged {
    let ranked = a.map(|a| a.apps.as_slice()).unwrap_or_default();
    let app_rank = ranked.iter().position(|r| r.app == app).map(|k| k + 1);
    let chain_rank = ranked.iter().position(|r| r.chain.as_ref().is_some_and(|c| c.id == chain)).map(|k| k + 1);
    let contradicts = match (state, app_rank) {
        ("accepted", Some(1)) => None,
        ("accepted", Some(k)) if k <= max_chains => Some(Contradiction::BelowRank1),
        ("accepted", _) => Some(Contradiction::DroppedOut),
        _ if chain_rank == Some(1) => Some(Contradiction::AtRank1),
        _ => None,
    };
    Judged { app_rank, chain_rank, contradicts }
}

/// The ranking a harvest stores: the past is read only when `precedent_weight` lets it move a rank.
fn ranked(s: &Snapshot, ids: &Ids, i: Id, st: &Settings) -> anyhow::Result<correlate::Analysis> {
    let past = if st.precedent_weight > 0.0 { past(s, ids, i, st)? } else { vec![] };
    correlate::analyze(s, ids, i, st, &past)
}

/// `ograph replay`: correlation again for every incident with a current verdict, under the
/// configured settings and under `st`, paired, so a flip is a verdict that `st` alone breaks.
pub fn replay(s: &Snapshot, st: &Settings, config: &Settings) -> anyhow::Result<Replay> {
    let ids = Ids::new(s)?;
    let (config_settings, settings) = (config.hash(), st.hash());
    let paired = config_settings != settings;
    let mut out = Replay { config_settings, settings, stale_chains: 0, verdicts: vec![], flips: 0, fixed: 0, contradicted: 0 };
    if let (Some(rank), Some(sp)) = (s.iri(corr::RANK)?, s.iri(corr::SETTINGS)?) {
        let current = s.id(&oxrdf::Literal::new_simple_literal(&out.config_settings).into())?;
        for q in s.quads([None, Some(rank), None], GraphSel::Union) {
            let c = q?[0];
            out.stale_chains += usize::from(current.is_none() || s.object(c, sp)? != current);
        }
    }
    let (Some(ty), Some(verdict), Some(on_chain)) = (s.iri(vocab::RDF_TYPE)?, s.iri(rca::VERDICT)?, s.iri(rca::ON_CHAIN)?) else { return Ok(out) };
    let sup = s.iri(rca::SUPERSEDED_BY)?;
    let get = |x: Id, p: &str| -> anyhow::Result<Option<Id>> { Ok(match s.iri(p)? { Some(p) => s.object(x, p)?, None => None }) };
    let mut analyses: FxHashMap<Id, [Option<correlate::Analysis>; 2]> = FxHashMap::default();
    for v in s.subjects(ty, verdict)? {
        if let Some(p) = sup
            && s.object(v, p)?.is_some()
        {
            continue;
        }
        let Some(chain) = s.object(v, on_chain)? else { continue };
        let Some(incident) = get(chain, corr::LEADS_TO)? else { continue };
        let [parent, candidate] = analyses.entry(incident).or_insert_with(|| {
            let parent = ranked(s, &ids, incident, config).ok();
            let candidate = if paired { ranked(s, &ids, incident, st).ok() } else { parent.clone() };
            [parent, candidate]
        });
        let chain_iri = literal(s, chain).unwrap_or_default();
        let app = get(v, rca::ROOT_ON)?.map(|a| app_name(s, &ids, a)).unwrap_or_default();
        let state = get(v, rca::STATE)?.and_then(|x| literal(s, x)).unwrap_or_default();
        let before = judge(&state, parent.as_ref(), &app, &chain_iri, config.max_chains);
        let after = judge(&state, candidate.as_ref(), &app, &chain_iri, st.max_chains);
        let flip = if before.contradicts.is_none() { after.contradicts.map(Contradiction::flip) } else { None };
        out.flips += usize::from(flip.is_some());
        out.fixed += usize::from(before.contradicts.is_some() && after.contradicts.is_none());
        out.contradicted += usize::from(after.contradicts.is_some());
        out.verdicts.push(Replayed {
            incident: get(incident, inc::ID)?.and_then(|x| literal(s, x)).unwrap_or_else(|| vocab::compact(&literal(s, incident).unwrap_or_default())),
            chain: vocab::compact(&chain_iri),
            state,
            app,
            judged_rank: get(v, rca::RANK)?.and_then(|x| literal(s, x)),
            before,
            after,
            flip,
        });
    }
    out.verdicts.sort_by(|a, b| (&a.incident, &a.chain).cmp(&(&b.incident, &b.chain)));
    Ok(out)
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

#[derive(Serialize, Debug, PartialEq)]
pub struct FactChange {
    /// `added`, `removed`, or `changed` when one value of a subject's property replaced another.
    pub change: &'static str,
    pub at: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
    pub graph: String,
    pub source: Option<String>,
    pub extractor: Option<String>,
}

/// What the sources stated at the time of `then` (a [`Snapshot::known_at`] view) and state
/// differently in `now`, by subject. Harvest bookkeeping, quarantined proposals and chains are
/// left out: chains are recomputed from the facts listed here.
pub fn changes(then: &Snapshot, now: &Snapshot) -> anyhow::Result<Vec<FactChange>> {
    let derived = [vocab::QUARANTINE.to_owned(), format!("{}harvest/correlation/", vocab::GRAPH)];
    let records = [format!("{}fact/", vocab::DATA), format!("{}quarantine/", vocab::DATA)];
    let (mut cache_then, mut cache_now) = (FxHashMap::default(), FxHashMap::default());
    let mut by_property: std::collections::BTreeMap<(String, String), [Vec<FactChange>; 2]> = Default::default();
    for c in then.changes()? {
        let [s, p, o, g] = c.quad;
        let (Term::NamedNode(subject), Term::NamedNode(predicate), Term::NamedNode(graph)) = (now.term(s)?, now.term(p)?, now.term(g)?) else { continue };
        if s == g
            || predicate.as_str().starts_with(harv::NS)
            || records.iter().any(|r| subject.as_str().starts_with(r.as_str()))
            || derived.iter().any(|d| graph.as_str().starts_with(d.as_str()))
        {
            continue;
        }
        let info = if c.added { graph_info(now, g, &mut cache_now) } else { graph_info(then, g, &mut cache_then) };
        let (subject, predicate) = (vocab::compact(subject.as_str()), vocab::compact(predicate.as_str()));
        by_property.entry((subject.clone(), predicate.clone())).or_default()[usize::from(c.added)].push(FactChange {
            change: if c.added { "added" } else { "removed" },
            at: term::format_millis(c.at_ms),
            subject,
            predicate,
            object: crate::sparql::show(&now.term(o)?),
            was: None,
            graph: info.graph,
            source: info.source,
            extractor: info.extractor,
        });
    }
    let mut out = Vec::new();
    for [mut removed, mut added] in by_property.into_values() {
        if let ([old], [new]) = (removed.as_slice(), added.as_mut_slice()) {
            new.change = "changed";
            new.was = Some(old.object.clone());
            removed.clear();
        }
        for side in [&mut removed, &mut added] {
            side.sort_by(|a, b| a.object.cmp(&b.object));
            out.append(side);
        }
    }
    Ok(out)
}

/// How an incident's ranked root causes moved between a time and now, and which source files
/// moved them.
#[derive(Serialize, Debug, PartialEq)]
pub struct RankShift {
    pub since: String,
    pub then: Vec<RankedApp>,
    pub now: Vec<RankedApp>,
    /// The graphs that recorded or removed a fact since, chains, quarantine and harvest
    /// bookkeeping aside.
    pub changed_graphs: usize,
    /// The changed graphs that move the ranking when tested one at a time; empty when the
    /// ranking did not move.
    pub causes: Vec<RankCause>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct RankedApp {
    pub app: String,
    pub score: f64,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct RankCause {
    pub graph: String,
    pub source: Option<String>,
    /// Taking this graph alone back to the time changes the ranking now.
    pub needed: bool,
    /// This graph's change alone, on the store as it was at the time, changes the ranking then.
    pub enough: bool,
}

/// Ranks `incident` at `t_ms` (a time the history covers) and now, and when the ranked
/// applications differ, tests each changed source graph twice: now with only that graph taken
/// back, and then with only that graph brought forward. A graph is a cause when either test
/// changes the order of the top `top` applications. Every view reads the one state of the store
/// `s` holds.
pub fn rank_shift(s: &Snapshot, t_ms: i64, incident: &str, st: &Settings, top: usize) -> anyhow::Result<RankShift> {
    let ranking = |v: &Snapshot| -> anyhow::Result<Vec<RankedApp>> {
        let (Ok(i), Ok(ids)) = (lookup(v, incident), Ids::new(v)) else { return Ok(vec![]) };
        let a = chains(v, &ids, i, st)?;
        Ok(a.apps.into_iter().take(top).map(|r| RankedApp { app: r.app, score: r.score }).collect())
    };
    let order = |r: &[RankedApp]| r.iter().map(|a| a.app.clone()).collect::<Vec<_>>();
    let d = s.delta(t_ms)?;
    let derived = [vocab::QUARANTINE.to_owned(), format!("{}harvest/correlation/", vocab::GRAPH)];
    let mut bookkeeping: FxHashMap<Id, bool> = FxHashMap::default();
    let mut changed = std::collections::BTreeSet::new();
    for &[subject, p, _, g] in d.quads() {
        if subject != g && !*bookkeeping.entry(p).or_insert_with(|| literal(s, p).is_some_and(|p| p.starts_with(harv::NS))) {
            changed.insert(g);
        }
    }
    let graphs: Vec<(Id, String)> = changed
        .into_iter()
        .map(|g| (g, literal(s, g).unwrap_or_default()))
        .filter(|(_, iri)| !derived.iter().any(|x| iri.starts_with(x.as_str())))
        .collect();
    let then_view = s.now()?.revert(&d, |_| true)?;
    let (then, current) = (ranking(&then_view)?, ranking(s)?);
    let mut causes = Vec::new();
    if order(&then) != order(&current) {
        let (mut cache_now, mut cache_then) = (FxHashMap::default(), FxHashMap::default());
        for &(g, ref iri) in &graphs {
            let needed = order(&ranking(&s.now()?.revert(&d, |x| x == g)?)?) != order(&current);
            let enough = order(&ranking(&s.now()?.revert(&d, |x| x != g)?)?) != order(&then);
            if needed || enough {
                let source = graph_info(s, g, &mut cache_now).source.or_else(|| graph_info(&then_view, g, &mut cache_then).source);
                causes.push(RankCause { graph: vocab::compact(iri), source, needed, enough });
            }
        }
    }
    Ok(RankShift { since: term::format_millis(t_ms), then, now: current, changed_graphs: graphs.len(), causes })
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
    let verdicts = verdicts(s, n)?.into_iter().map(|v| verdict_event(s, v)).collect::<anyhow::Result<_>>()?;
    Ok(Provenance { node: vocab::compact(&me), facts, quarantined, verdicts })
}

/// `text` names `node` as a whole term, so `id:app/cart` is not found in `id:app/cartservice`.
fn mentions(text: &str, node: &str) -> bool {
    let name_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '%');
    text.match_indices(node).any(|(i, _)| {
        !text[..i].ends_with(name_char) && !text[i + node.len()..].starts_with(name_char)
    })
}

#[derive(Serialize)]
pub struct Uses {
    /// The file, directory or record, as harvests name it.
    pub source: String,
    /// The harvest graphs that hold its facts.
    pub graphs: Vec<String>,
    /// The naming source (`catalog`, `ownership` or `docs`) it feeds. Every other file's
    /// application names are checked against it, so without it facts elsewhere can change too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub naming: Option<String>,
    pub facts: Vec<UsedFact>,
    /// The events and incidents that no other source states.
    pub events: Vec<UsedEvent>,
    /// Nodes it states together with records of other files. They stay without it, and a
    /// field only it set may take another record's value.
    pub shared: Vec<String>,
    /// The stored chains that would change without it, and those that would enter.
    pub chains: Vec<ChainChange>,
    /// The current verdict on each chain that would go or be relinked.
    pub verdicts: Vec<UsedVerdict>,
    pub quarantined: Vec<UsedQuarantine>,
}

#[derive(Serialize)]
pub struct UsedFact {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub graph: String,
    /// Other sources that state the same fact; when there are none it goes with this source.
    pub also_in: Vec<String>,
    /// For a fact of another source: why its next harvest would quarantine it without this
    /// one, such as an action whose incident goes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub because: Option<String>,
}

#[derive(Serialize)]
pub struct UsedEvent {
    pub node: String,
    pub signal: String,
    pub app: Option<String>,
    pub at: Option<String>,
    pub record: Option<String>,
}

#[derive(Serialize)]
pub struct ChainChange {
    pub chain: String,
    pub incident: String,
    /// `gone`, `relinked` (another root or set of links), `moved` (another rank) or `new`.
    pub change: &'static str,
    pub rank: Option<usize>,
    pub rank_after: Option<usize>,
    /// Its root, links and incident that go with the source. Empty when the chain changes
    /// through something else the source states, such as a dependency its links cross.
    pub uses: Vec<String>,
}

#[derive(Serialize)]
pub struct UsedVerdict {
    pub chain: String,
    pub incident: String,
    pub state: String,
    pub by: String,
    pub at: String,
}

#[derive(Serialize)]
pub struct UsedQuarantine {
    pub node: String,
    pub fact: String,
    pub reason: String,
}

/// A record id's file and its suffix: `path#n` (a JSON record), `path:n` (a line) or `path@n`
/// (a time in milliseconds).
pub fn split_record(t: &str) -> (&str, Option<&str>) {
    match t.rfind(['#', ':', '@']) {
        Some(i) if i > 0 && i + 1 < t.len() && t[i + 1..].bytes().all(|b| b.is_ascii_digit()) => (&t[..i], Some(&t[i..])),
        _ => (t, None),
    }
}

/// A path given relative to `cwd`, with an optional record suffix, as harvests key its file:
/// relative to the workspace root when it is below it, absolute otherwise.
pub fn source_key(root: &Path, cwd: &Path, arg: &str) -> String {
    let (path, record) = split_record(arg);
    let key = rel(&cwd.join(root), &cwd.join(path));
    format!("{}{}", key.trim_end_matches('/'), record.unwrap_or_default())
}

const NAMING: [&str; 3] = ["catalog", "ownership", "docs"];

/// `ograph uses`: what depends on a source file, a directory of them or one record. Its facts
/// are those of the graphs keyed by the file and, in a graph that spans several files (the
/// incidents), those of the nodes whose `evt:sourceRecord` names it. Chains are ranked again
/// for every incident over a view of the store without those facts, so a chain that changes
/// through a dependency the file states is found as well as one whose events go.
pub fn uses(store: &crate::store::Store, ont: &crate::ontology::Ontology, st: &Settings, target: &str) -> anyhow::Result<Uses> {
    let s = &store.snapshot()?;
    let (key, record) = split_record(target);
    let enc = crate::model::path_iri(key);
    let below = |path: &str, top: &str| path == top || path.strip_prefix(top).is_some_and(|r| r.starts_with('/'));
    let names = |r: &str| match record {
        Some(suffix) => r.strip_prefix(key) == Some(suffix),
        None => below(split_record(r).0, key),
    };
    let (harvest, quarantine) = (format!("{}harvest/", vocab::GRAPH), format!("{}/", vocab::QUARANTINE));
    let (file_p, rec_p) = (s.iri(harv::FILE)?, s.iri(evt::SOURCE_RECORD)?);
    let mut out = Uses { source: target.to_owned(), graphs: vec![], naming: None, facts: vec![], events: vec![], shared: vec![], chains: vec![], verdicts: vec![], quarantined: vec![] };
    // Graphs keyed by the file, graphs of several files that read it, and every input graph.
    let (mut own, mut spanning, mut input) = (Vec::new(), Vec::new(), FxHashSet::default());
    let mut quarantines: Vec<(Id, bool)> = Vec::new();
    let mut spanning_keys = Vec::new();
    let graphs = s.graphs()?;
    for &(g, _) in &graphs {
        let Some(iri) = literal(s, g) else { continue };
        let Some((source, path)) = iri.strip_prefix(&harvest).and_then(|r| r.split_once('/')) else { continue };
        if source == "correlation" || source == "verdicts" {
            continue;
        }
        input.insert(g);
        let read = |p: Id| -> anyhow::Result<bool> {
            for q in s.quads([Some(g), Some(p), None], GraphSel::Named(g)) {
                if literal(s, q?[2]).is_some_and(|f| below(&f, key)) {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        if below(path, &enc) {
            own.push(g);
        } else if file_p.map(read).transpose()?.unwrap_or(false) {
            spanning.push(g);
            spanning_keys.push(format!("{source}/{path}"));
        } else if source == "catalog" && below(&enc, path) {
            out.naming = Some(source.to_owned());
            out.graphs.push(vocab::compact(&iri));
            continue;
        } else {
            continue;
        }
        if NAMING.contains(&source) {
            out.naming = Some(source.to_owned());
        }
        out.graphs.push(vocab::compact(&iri));
    }
    for &(g, _) in &graphs {
        let Some(rest) = literal(s, g).and_then(|iri| iri.strip_prefix(&quarantine).map(str::to_owned)) else { continue };
        let own = rest.split_once('/').is_some_and(|(_, p)| below(p, &enc));
        if own || spanning_keys.contains(&rest) {
            // A record's entries, and a shared graph's, are those whose reason names it first.
            quarantines.push((g, own && record.is_none()));
        }
    }

    let mut derived: FxHashSet<[Id; 4]> = FxHashSet::default();
    if record.is_none() {
        for &g in &own {
            for q in s.quads([None; 3], GraphSel::Named(g)) {
                let q = q?;
                if q[0] != g {
                    derived.insert(q);
                }
            }
        }
    }
    let mut shared = FxHashSet::default();
    let by_record: Vec<Id> = if record.is_some() { own.iter().chain(&spanning).copied().collect() } else { spanning.clone() };
    if let Some(rp) = rec_p {
        for &g in &by_record {
            let mut subjects = FxHashSet::default();
            for q in s.quads([None, Some(rp), None], GraphSel::Named(g)) {
                let q = q?;
                if literal(s, q[2]).is_some_and(|r| names(&r)) {
                    subjects.insert(q[0]);
                }
            }
            for x in subjects {
                let quads: Vec<[Id; 4]> = s.quads([Some(x), None, None], GraphSel::Named(g)).collect::<Result<_, _>>()?;
                let only_here = quads.iter().filter(|q| q[1] == rp).all(|q| literal(s, q[2]).is_some_and(|r| names(&r)));
                if only_here {
                    derived.extend(quads);
                } else {
                    shared.insert(x);
                    derived.extend(quads.into_iter().filter(|q| q[1] == rp && literal(s, q[2]).is_some_and(|r| names(&r))));
                }
            }
        }
    }

    let gone_of = |derived: &FxHashSet<[Id; 4]>| -> anyhow::Result<(Vec<Id>, FxHashSet<Id>)> {
        let mut subjects: Vec<Id> = derived.iter().map(|q| q[0]).collect::<FxHashSet<_>>().into_iter().collect();
        subjects.sort_unstable();
        let mut gone = FxHashSet::default();
        'node: for &x in &subjects {
            if shared.contains(&x) {
                continue;
            }
            for q in s.quads([Some(x), None, None], GraphSel::AnyNamed) {
                let q = q?;
                if input.contains(&q[3]) && !derived.contains(&q) {
                    continue 'node;
                }
            }
            gone.insert(x);
        }
        Ok((subjects, gone))
    };
    let (subjects, gone) = gone_of(&derived)?;
    // Other sources' facts that name what goes, or describe a node that loses a fact, are
    // checked again by their next harvest, and SHACL can quarantine them.
    let mut again = FxHashSet::default();
    for &x in &subjects {
        let side = if gone.contains(&x) { [None, None, Some(x)] } else { [Some(x), None, None] };
        for q in s.quads(side, GraphSel::AnyNamed) {
            let q = q?;
            if input.contains(&q[3]) && !derived.contains(&q) {
                again.insert(q[3]);
            }
        }
    }
    let mut because: FxHashMap<[Id; 4], String> = FxHashMap::default();
    if !again.is_empty() {
        let view = store.snapshot()?.without(derived.iter().copied())?;
        let mut again: Vec<Id> = again.into_iter().collect();
        again.sort_unstable();
        for g in again {
            let now: FxHashSet<oxrdf::Triple> = crate::gateway::rejected_again(s, ont, g)?.into_iter().map(|(t, _)| t).collect();
            for (t, why) in crate::gateway::rejected_again(&view, ont, g)?.into_iter().filter(|(t, _)| !now.contains(t)) {
                let ids = [s.id(&t.subject.clone().into())?, s.id(&t.predicate.clone().into())?, s.id(&t.object)?];
                let [Some(x), Some(p), Some(o)] = ids else { continue };
                because.insert([x, p, o, g], why);
                if let Some(f) = s.iri(crate::gateway::fact_node(&t).as_str())? {
                    for q in s.quads([Some(f), None, None], GraphSel::Named(g)) {
                        derived.insert(q?);
                    }
                }
            }
        }
        derived.extend(because.keys().copied());
    }
    let (subjects, gone) = if because.is_empty() { (subjects, gone) } else { gone_of(&derived)? };

    let mut cache = FxHashMap::default();
    let mut facts: Vec<UsedFact> = Vec::with_capacity(derived.len());
    for q in &derived {
        let [x, p, o, g] = *q;
        let mut also_in: Vec<String> = s.graphs_of([x, p, o])?.into_iter().filter(|h| *h != g && input.contains(h) && !derived.contains(&[x, p, o, *h])).map(|h| graph_info(s, h, &mut cache).graph).collect();
        also_in.sort();
        facts.push(UsedFact { subject: vocab::compact(&literal(s, x).unwrap_or_default()), predicate: vocab::compact(&literal(s, p).unwrap_or_default()), object: crate::sparql::show(&s.term(o)?), graph: graph_info(s, g, &mut cache).graph, also_in, because: because.get(q).cloned() });
    }
    facts.sort_by(|a, b| (&a.subject, &a.predicate, &a.object, &a.graph).cmp(&(&b.subject, &b.predicate, &b.object, &b.graph)));
    out.facts = facts;

    let ids = Ids::new(s)?;
    let one = |x: Id, p: &str| -> anyhow::Result<Option<Id>> { Ok(match s.iri(p)? { Some(p) => s.object(x, p)?, None => None }) };
    for &x in &subjects {
        if !gone.contains(&x) {
            continue;
        }
        let Some(signal) = one(x, evt::SIGNAL)?.and_then(|v| literal(s, v)) else { continue };
        out.events.push(UsedEvent {
            node: vocab::compact(&literal(s, x).unwrap_or_default()),
            signal,
            app: one(x, evt::ON)?.map(|a| app_name(s, &ids, a)),
            at: one(x, evt::AT)?.and_then(|v| literal(s, v)),
            record: one(x, evt::SOURCE_RECORD)?.and_then(|v| literal(s, v)),
        });
    }
    out.events.sort_by(|a, b| (&a.at, &a.node).cmp(&(&b.at, &b.node)));
    out.shared = shared.iter().filter_map(|&x| literal(s, x)).map(|x| vocab::compact(&x)).collect();
    out.shared.sort();

    let gone_iris: FxHashSet<String> = gone.iter().filter_map(|&x| literal(s, x)).collect();
    if !derived.is_empty() {
        out.chains = chain_changes(s, store.snapshot()?.without(derived.iter().copied())?, st, &gone_iris)?;
    }
    for c in out.chains.iter().filter(|c| matches!(c.change, "gone" | "relinked")) {
        let Some(chain) = s.iri(&crate::workspace::resolve_iri(&c.chain))? else { continue };
        let Some(&v) = verdicts(s, chain)?.last() else { continue };
        let e = verdict_event(s, v)?;
        out.verdicts.push(UsedVerdict { chain: c.chain.clone(), incident: c.incident.clone(), state: e.state, by: e.by, at: e.at });
    }

    if let (Some(fp), Some(rp)) = (s.iri(harv::FACT)?, s.iri(harv::REASON)?) {
        for (g, whole) in quarantines {
            for q in s.quads([None, Some(fp), None], GraphSel::Named(g)) {
                let q = q?;
                let reason = s.object(q[0], rp)?.and_then(|r| literal(s, r)).unwrap_or_default();
                if whole || reason.split_whitespace().next().is_some_and(names) {
                    out.quarantined.push(UsedQuarantine { node: vocab::compact(&literal(s, q[0]).unwrap_or_default()), fact: literal(s, q[2]).unwrap_or_default(), reason });
                }
            }
        }
        out.quarantined.sort_by(|a, b| (&a.fact, &a.reason).cmp(&(&b.fact, &b.reason)));
    }
    Ok(out)
}

/// Every incident ranked as a harvest stores it, over the store and over `view`, the store
/// without a source's facts, and the chains that differ.
fn chain_changes(s: &Snapshot, view: Snapshot, st: &Settings, gone: &FxHashSet<String>) -> anyhow::Result<Vec<ChainChange>> {
    type Stored = std::collections::BTreeMap<String, (usize, String, std::collections::BTreeSet<String>)>;
    let stored = |a: &correlate::Analysis| -> Stored {
        a.apps.iter().take(st.max_chains).enumerate().filter_map(|(k, r)| r.chain.as_ref().map(|c| (c.id.clone(), (k + 1, c.root.iri.clone(), c.links.iter().map(|l| l.iri.clone()).collect())))).collect()
    };
    let (ids, view_ids) = (Ids::new(s)?, Ids::new(&view)?);
    let (Some(ty), Some(class)) = (s.iri(vocab::RDF_TYPE)?, s.iri(inc::INCIDENT)?) else { return Ok(vec![]) };
    let mut out = Vec::new();
    for i in s.subjects(ty, class)? {
        let Ok(now) = ranked(s, &ids, i, st) else { continue };
        let (before, after) = (stored(&now), ranked(&view, &view_ids, i, st).map(|a| stored(&a)).unwrap_or_default());
        let incident = match s.iri(inc::ID)? {
            Some(p) => s.object(i, p)?.and_then(|x| literal(s, x)),
            None => None,
        }
        .unwrap_or_else(|| vocab::compact(&now.incident));
        for (id, (rank, root, links)) in &before {
            let change = match after.get(id) {
                None => "gone",
                Some((_, r, l)) if r != root || l != links => "relinked",
                Some((k, ..)) if k != rank => "moved",
                _ => continue,
            };
            let uses = std::iter::once(root).chain(links).chain(std::iter::once(&now.incident)).filter(|x| gone.contains(*x)).map(|x| vocab::compact(x)).collect();
            out.push(ChainChange { chain: vocab::compact(id), incident: incident.clone(), change, rank: Some(*rank), rank_after: after.get(id).map(|a| a.0), uses });
        }
        for (id, (rank, ..)) in after.iter().filter(|(id, _)| !before.contains_key(*id)) {
            out.push(ChainChange { chain: vocab::compact(id), incident: incident.clone(), change: "new", rank: None, rank_after: Some(*rank), uses: vec![] });
        }
    }
    out.sort_by(|a, b| (&a.incident, a.rank.or(a.rank_after), &a.chain).cmp(&(&b.incident, b.rank.or(b.rank_after), &b.chain)));
    Ok(out)
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
    /// Deployments before the incident whose changed files are known, those that changed a
    /// located file first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<CodeDeployment>,
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
    /// For a log message: the commit that first added it to the code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub introduced: Option<Introduced>,
}

#[derive(Serialize, Clone)]
pub struct Introduced {
    pub revision: String,
    pub at: String,
    pub days_before: f64,
    pub summary: String,
    /// Where the commit added it, which is another path if the file has moved since.
    pub path: String,
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
pub struct CodeDeployment {
    pub event: String,
    pub app: String,
    pub at: String,
    pub minutes_before: f64,
    pub version: Option<String>,
    /// The files changed since the version the application ran before.
    pub files: Vec<String>,
    /// The locations of this answer in a file the deployment changed.
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
    let version = s.iri(evt::VERSION)?;
    let minutes_before = |at_ms: i64| ((t - at_ms) as f64 / 60_000.0 * 10.0).round() / 10.0;
    let (mut locations, mut commits, mut deployments) = (Vec::new(), Vec::new(), Vec::new());
    for c in cands.iter().rev() {
        let at = term::format_millis(c.at_ms);
        let mut found = correlate::texts(s, c.id, ids.code_location)?;
        found.sort();
        locations.extend(found.into_iter().map(|location| CodeLocation { event: vocab::compact(&c.iri), app: c.app_name.clone(), signal: c.kind.clone(), at: at.clone(), location, summary: describe(c.id), introduced: None }));
        if c.kind == "COMMIT" {
            let mut files = correlate::texts(s, c.id, ids.changed_file)?;
            files.sort();
            commits.push(CodeCommit { event: vocab::compact(&c.iri), app: c.app_name.clone(), at, minutes_before: minutes_before(c.at_ms), revision: text(c.id, revision), summary: describe(c.id), files, touches: vec![] });
        } else if c.kind == "DEPLOYMENT" {
            let mut files = correlate::texts(s, c.id, ids.changed_file)?;
            files.sort();
            if !files.is_empty() {
                deployments.push(CodeDeployment { event: vocab::compact(&c.iri), app: c.app_name.clone(), at, minutes_before: minutes_before(c.at_ms), version: text(c.id, version), files, touches: vec![] });
            }
        }
    }
    let touched = |files: &[String]| {
        let mut touches: Vec<String> = Vec::new();
        for l in &locations {
            if files.iter().any(|f| f == correlate::location_file(&l.location)) && !touches.contains(&l.location) {
                touches.push(l.location.clone());
            }
        }
        touches
    };
    for c in &mut commits {
        c.touches = touched(&c.files);
    }
    for d in &mut deployments {
        d.touches = touched(&d.files);
    }
    commits.sort_by_key(|c| c.touches.is_empty());
    deployments.sort_by_key(|d| d.touches.is_empty());
    let mut words: Vec<String> = text(i, title).into_iter().collect();
    let analysis = chains(s, &ids, i, st)?;
    if let Some(chain) = analysis.apps.first().and_then(|r| r.chain.as_ref()) {
        for link in std::iter::once(&chain.root).chain(&chain.links) {
            words.push(link.app_name.clone());
            words.extend(describe(link.id));
        }
    }
    let words = words.join(" ").replace("<*>", " ").split_whitespace().collect::<Vec<_>>().join(" ");
    let answer = Code { incident: vocab::compact(&literal(s, i).unwrap_or_default()), app: app_name(s, &ids, app), at: term::format_millis(t), locations, commits, deployments, files: vec![], files_confidence: None, writeups: vec![], degraded: vec![] };
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

/// A message shorter than this is in too many lines of a repository to date.
const MIN_MESSAGE: usize = 12;

/// The longest part of a log template between its variable parts: what the code most likely
/// holds as written.
fn constant_part(template: &str) -> Option<&str> {
    template.split("<*>").map(str::trim).filter(|part| part.chars().count() >= MIN_MESSAGE).max_by_key(|part| part.len())
}

/// The history half of `ograph code`: for each located log message, the commit that first
/// added it, among the commits made until the incident. A message can enter the code days
/// before it is first written, which the commits of the correlation window do not show. One
/// ogrep call per distinct message, side by side.
pub fn code_history(answer: &mut Code, root: &Path, repos: &[PathBuf]) {
    let Some(t) = crate::enrich::parse_iso(&answer.at) else { return };
    let mut asks: Vec<(&PathBuf, &str)> = Vec::new();
    let mut ask_of = Vec::new();
    for l in &answer.locations {
        let file = correlate::location_file(&l.location);
        let ask = l.summary.as_deref().filter(|_| l.signal.starts_with("LOG_")).and_then(constant_part).zip(repos.iter().find(|r| Path::new(file).starts_with(rel(root, r))));
        ask_of.push(ask.map(|(part, repo)| {
            asks.iter().position(|a| *a == (repo, part)).unwrap_or_else(|| {
                asks.push((repo, part));
                asks.len() - 1
            })
        }));
    }
    let found = ogrep::each(&asks, |(repo, part)| ogrep::introduced(repo, part, &answer.at));
    let mut notes = Vec::new();
    let found: Vec<Option<Introduced>> = found
        .into_iter()
        .zip(&asks)
        .map(|(r, (repo, _))| match r {
            Ok(c) => c.map(|c| Introduced { revision: c.sha, at: term::format_millis(c.time * 1000), days_before: ((t - c.time * 1000) as f64 / 86_400_000.0 * 10.0).round() / 10.0, summary: c.subject, path: rel(root, &repo.join(c.path)) }),
            Err(e) => {
                let note = format!("{}: {e}", rel(root, repo));
                if !notes.contains(&note) {
                    notes.push(note);
                }
                None
            }
        })
        .collect();
    drop(asks);
    for (l, ask) in answer.locations.iter_mut().zip(ask_of) {
        l.introduced = ask.and_then(|n| found[n].clone());
    }
    answer.degraded.extend(notes);
}

/// The applications `ograph suspects` asks ogrep about, and how far back it looks by default.
pub struct Failing {
    pub apps: Vec<String>,
    pub since: String,
    /// For an incident, the time it opened: commits made after it cannot have caused it.
    pub until: Option<String>,
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
        return Ok(Failing { apps, since: term::format_millis(t - 86_400_000).chars().take(10).collect(), until: Some(term::format_millis(t)) });
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
    Ok(Failing { apps, since: "24h".into(), until: None })
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
/// from `since` to `until`, from the first of `repos` ogrep answers for. The second value says
/// why the repositories before it gave nothing. An ogrep too old to take `until` is asked
/// again without it, and the answer then also counts later commits, which a note says.
pub fn suspects(root: &Path, repos: &[PathBuf], failing: &[String], since: &str, until: Option<&str>) -> (Vec<Suspect>, Vec<String>) {
    let mut degraded = Vec::new();
    for repo in repos {
        let mut answer = ogrep::suspects(repo, failing, since, until);
        if let (Some(until), Err(ogrep::Failure::Failed { stderr, .. })) = (until, &answer)
            && stderr.contains("--until")
        {
            degraded.push(format!("{}: this ogrep cannot stop at {until}, so commits made after it count too", rel(root, repo)));
            answer = ogrep::suspects(repo, failing, since, None);
        }
        match answer {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shift: Option<RankShift>,
}

/// At most this many mentions per `resolve` call, so an agent names what it needs, not a list.
pub const MAX_MENTIONS: usize = 5;

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MentionStatus {
    Resolved,
    Ambiguous,
    Unresolved,
}

#[derive(Serialize)]
pub struct Mention {
    pub mention: String,
    pub status: MentionStatus,
    /// The application, only when `status` is resolved.
    pub app: Option<String>,
    pub name: Option<String>,
    pub rationale: String,
    pub candidates: Vec<Candidate>,
}

#[derive(Serialize)]
pub struct Candidate {
    pub app: String,
    pub name: String,
    pub score: f64,
}

#[derive(Serialize)]
pub struct Resolved {
    pub results: Vec<Mention>,
    /// Mentions past the first [`MAX_MENTIONS`], not looked at.
    pub skipped: Vec<String>,
}

/// Application names, aliases or IRIs to catalog applications, by the gateway's own rules: a
/// name the gateway would accept resolves, one it would quarantine as ambiguous or missing does
/// not, so an answer never names an application a harvest would refuse.
pub fn resolve(s: &Snapshot, mentions: &[String]) -> anyhow::Result<Resolved> {
    let catalog = crate::gateway::Catalog::from_snapshot(s, None)?;
    let ids = Ids::new(s)?;
    let named = |n: &oxrdf::NamedNode| -> anyhow::Result<(String, String)> {
        let name = s.iri(n.as_str())?.map(|i| app_name(s, &ids, i)).unwrap_or_default();
        Ok((vocab::compact(n.as_str()), name))
    };
    let mentions: Vec<&String> = mentions.iter().filter(|m| !m.trim().is_empty()).collect();
    let mut results = Vec::new();
    for m in mentions.iter().take(MAX_MENTIONS) {
        let m = m.trim();
        let iri = crate::workspace::resolve_iri(m);
        let r = if iri.starts_with(vocab::DATA) {
            let n = oxrdf::NamedNode::new_unchecked(&iri);
            let found = match (s.iri(&iri)?, s.iri(ops::APPLICATION)?) {
                (Some(i), Some(c)) => s.object(i, ids.ty)? == Some(c),
                _ => false,
            };
            crate::gateway::Match {
                resolution: if found { crate::gateway::Resolution::Exact(n.clone()) } else { crate::gateway::Resolution::Missing },
                why: if found { "an application IRI in the catalog".into() } else { "an IRI that is not a catalog application".into() },
                candidates: if found { vec![(n, 1.0)] } else { vec![] },
            }
        } else {
            catalog.explain(ops::APPLICATION, m)
        };
        let (status, app) = match &r.resolution {
            crate::gateway::Resolution::Exact(n) | crate::gateway::Resolution::Fuzzy(n, _) => (MentionStatus::Resolved, Some(n)),
            crate::gateway::Resolution::Ambiguous(..) => (MentionStatus::Ambiguous, None),
            crate::gateway::Resolution::Missing => (MentionStatus::Unresolved, None),
        };
        let app = app.map(named).transpose()?;
        let candidates = r.candidates.iter().map(|(n, score)| named(n).map(|(app, name)| Candidate { app, name, score: (score * 1000.0).round() / 1000.0 })).collect::<anyhow::Result<_>>()?;
        results.push(Mention { mention: m.to_owned(), status, name: app.as_ref().map(|a| a.1.clone()), app: app.map(|a| a.0), rationale: r.why, candidates });
    }
    Ok(Resolved { results, skipped: mentions.iter().skip(MAX_MENTIONS).map(|m| m.trim().to_owned()).collect() })
}

#[cfg(all(test, unix))]
mod tests {
    use crate::correlate::{PastRoot, Settings};
    use crate::workspace::Workspace;
    use crate::workspace::tests::{T, alert, fixture, harvest};
    use std::path::{Path, PathBuf};

    const DAY: i64 = 86_400;

    /// INC-1 and, a day later, INC-2, each after the same two alerts, so they are fully similar
    /// and both rank ledger-db above payments-api at 0.9.
    fn twice(name: &str) -> PathBuf {
        let alerts = [alert("ledger-db", "PoolSaturated", T - 600), alert("payments-api", "ErrorRate", T - 300), alert("ledger-db", "PoolSaturated", T + DAY - 600), alert("payments-api", "ErrorRate", T + DAY - 300)];
        let root = fixture(name, &[("alerts", "alerts.json", &format!("[{}]", alerts.join(",")))]);
        std::fs::write(root.join("incidents.json"), format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T}}},{{"id":"INC-2","service":"checkout","opened":{}}}]"#, T + DAY)).unwrap();
        harvest(&root, None);
        root
    }

    /// Application, score to three decimals, and the accepted cause it matches, per ranked application.
    fn ranked(root: &Path, incident: &str, st: impl FnOnce(&mut Settings)) -> Vec<(String, String, Option<PastRoot>)> {
        let ws = Workspace::open(root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let ids = crate::correlate::Ids::new(&snap).unwrap();
        let mut settings = ws.config.correlation.clone();
        st(&mut settings);
        let a = super::chains(&snap, &ids, super::lookup(&snap, incident).unwrap(), &settings).unwrap();
        a.apps.into_iter().map(|r| (r.app, format!("{:.3}", r.score), r.precedent)).collect()
    }

    #[test]
    fn similar_lists_the_roots_rejected_on_a_past_incident_and_chains_ranks_as_before() {
        let root = twice("ruled-out");
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-1", false, "oncall", &["pool was at 40%".into()]).unwrap();
        let ws = Workspace::open(&root).unwrap();
        let similar = super::similar(&ws.store.snapshot().unwrap(), "INC-2", &ws.config.correlation, 5).unwrap();
        assert_eq!(similar.len(), 1);
        assert_eq!(similar[0].ruled_out, [super::RuledOut { app: "ledger-db".into(), signal: "ALERT".into(), evidence: vec!["pool was at 40%".into()] }]);
        assert!(similar[0].cause_apps.is_empty());
        assert_eq!(ranked(&root, "INC-2", |st| st.precedent_weight = 0.1), [("ledger-db".to_owned(), "0.900".to_owned(), None), ("payments-api".to_owned(), "0.900".to_owned(), None)]);
    }

    #[test]
    fn a_superseded_verdict_is_neither_a_precedent_nor_ruled_out() {
        let root = twice("superseded");
        let ws = Workspace::open(&root).unwrap();
        ws.verdict("id:chain/inc-1-1", false, "oncall", &["pool was at 40%".into()]).unwrap();
        ws.verdict("id:chain/inc-1-1", true, "oncall", &["PM-9".into()]).unwrap();
        let similar = super::similar(&ws.store.snapshot().unwrap(), "INC-2", &ws.config.correlation, 5).unwrap();
        assert_eq!(similar[0].ruled_out, []);
        assert_eq!(similar[0].cause_apps, ["ledger-db"]);
        let mark = Some(PastRoot { incident: "id:inc/inc-1".into(), similarity: 1.0, app: "ledger-db".into(), signal: "ALERT".into(), evidence: vec!["PM-9".into()] });
        assert_eq!(ranked(&root, "INC-2", |_| {}), [("ledger-db".to_owned(), "0.900".to_owned(), mark), ("payments-api".to_owned(), "0.900".to_owned(), None)]);
        ws.verdict("id:chain/inc-1-1", false, "oncall", &["PM-9 was wrong".into()]).unwrap();
        let similar = super::similar(&ws.store.snapshot().unwrap(), "INC-2", &ws.config.correlation, 5).unwrap();
        assert_eq!(similar[0].ruled_out, [super::RuledOut { app: "ledger-db".into(), signal: "ALERT".into(), evidence: vec!["PM-9 was wrong".into()] }]);
        assert_eq!(ranked(&root, "INC-2", |_| {}), [("ledger-db".to_owned(), "0.900".to_owned(), None), ("payments-api".to_owned(), "0.900".to_owned(), None)]);
    }

    #[test]
    fn a_cause_accepted_on_a_similar_incident_is_marked_and_can_be_ranked_higher() {
        let root = twice("precedent");
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-2", true, "oncall", &["PM-7".into()]).unwrap();
        let mark = Some(PastRoot { incident: "id:inc/inc-1".into(), similarity: 1.0, app: "payments-api".into(), signal: "ALERT".into(), evidence: vec!["PM-7".into()] });
        assert_eq!(ranked(&root, "INC-2", |_| {}), [("ledger-db".to_owned(), "0.900".to_owned(), None), ("payments-api".to_owned(), "0.900".to_owned(), mark.clone())]);
        assert_eq!(ranked(&root, "INC-2", |st| st.precedent_weight = 0.1), [("payments-api".to_owned(), "1.000".to_owned(), mark.clone()), ("ledger-db".to_owned(), "0.900".to_owned(), None)]);
        // INC-2 opened later, so it is no precedent for INC-1, unless an evaluation says so.
        assert!(ranked(&root, "INC-1", |st| st.precedent_weight = 0.1).iter().all(|r| r.2.is_none()));
        Workspace::open(&root).unwrap().verdict("id:chain/inc-2-2", true, "oncall", &["PM-9".into()]).unwrap();
        assert_eq!(ranked(&root, "INC-1", |st| st.past_any_time = true)[1].2.as_ref().map(|p| p.incident.as_str()), Some("id:inc/inc-2"));
    }

    #[test]
    fn a_harvest_stores_the_ranks_the_precedent_weight_gives() {
        let root = twice("precedent-stored");
        Workspace::open(&root).unwrap().verdict("id:chain/inc-1-2", true, "oncall", &[]).unwrap();
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), config + "[correlation]\nprecedent_weight = 0.1\n").unwrap();
        harvest(&root, None);
        let ws = Workspace::open(&root).unwrap();
        let rows = crate::sparql::run(&ws.store.snapshot().unwrap(), &crate::sparql::parse("SELECT ?app WHERE { ?c corr:leadsTo ?i ; corr:rank 1 ; corr:candidateApp ?app . ?i inc:id \"INC-2\" }").unwrap(), None).unwrap();
        assert_eq!(serde_json::to_value(&rows).unwrap()["rows"], serde_json::json!([["id:app/payments-api"]]));
    }
}
