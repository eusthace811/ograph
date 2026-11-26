//! Correlation: candidate causal chains for an incident, walked depth-first over the native
//! per-application time index. A chain is a hypothesis with its evidence, never a conclusion.
//!
//! Scoring is Netflix's published rule (post, page 9): a known causal pair carries a 0.9 prior,
//! any other pair scores `time_proximity * 0.6 + severity * 0.4`, and links under 0.6 are
//! discarded. The window defaults to 30 minutes, not Netflix's 10: on RCAEval a 10-minute window
//! drops to 0.11 top-1 once the incident opens 10 minutes after the fault, while 30 minutes
//! keeps 0.79 (docs/evidence/rcaeval-lag.txt). `rank = "propagation"` is the alternative for a
//! team that must keep a short window.

use crate::store::Snapshot;
use crate::term::{self, Id};
use crate::vocab::{RDF_TYPE, evt, inc, ops};
use oxrdf::Term;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub window_s: i64,
    /// How long after the incident opened events still count (detectors lag, symptoms spread).
    pub after_s: i64,
    pub min_confidence: f64,
    pub w_time: f64,
    pub w_severity: f64,
    pub known_pair_prior: f64,
    pub depth: usize,
    pub max_chains: usize,
    pub rank: RankMode,
    pub known_pairs: Vec<(String, String)>,
    /// Propagation-mode weights: anomaly strength, best chain, spread to callers, onset.
    pub prop_weights: [f64; 4],
    /// Ablation: ignore dependency edges (every application is in scope, no caller links).
    pub ignore_dependencies: bool,
    /// Ablation: keep only these signal kinds (empty keeps all).
    pub only_signals: Vec<String>,
    /// Window for an incident's signature in `similar`: the onset pattern, not the whole lead-up.
    pub signature_window_s: i64,
    /// How much an accepted cause of a similar past incident lifts a root with the same
    /// application and signal: the score gains this times the similarity. 0 ranks on telemetry
    /// alone.
    pub precedent_weight: f64,
    /// Evaluation: incidents that opened later count as past ones too, so a labeled set can be
    /// scored leave-one-out.
    pub past_any_time: bool,
    /// A root is credited only with what its own application shows. A known pair holds within
    /// one application (and for pairs that close at the incident): a deployment on a callee
    /// links to its caller's errors on time and severity, not on the pair's prior. And in chain
    /// ranking an application ranks below another whose chain [explains](Ranked::explains) it.
    /// false ranks as before (docs/evidence/deploy-ranking.txt).
    pub own_degradation: bool,
}

impl Settings {
    /// Scores are probabilities and windows are spans, so out-of-range settings are refused
    /// rather than written as confidences the shapes then quarantine.
    pub fn check(&self) -> anyhow::Result<()> {
        for (name, v) in [("min_confidence", self.min_confidence), ("w_time", self.w_time), ("w_severity", self.w_severity), ("known_pair_prior", self.known_pair_prior), ("precedent_weight", self.precedent_weight)] {
            anyhow::ensure!((0.0..=1.0).contains(&v), "correlation setting {name} = {v} must be between 0 and 1");
        }
        anyhow::ensure!(self.w_time + self.w_severity <= 1.0, "correlation settings w_time + w_severity = {} must not exceed 1", self.w_time + self.w_severity);
        anyhow::ensure!(self.prop_weights.iter().all(|w| w.is_finite() && *w >= 0.0), "correlation setting prop_weights must be finite and not negative");
        for (name, v) in [("window_s", self.window_s), ("signature_window_s", self.signature_window_s)] {
            anyhow::ensure!(v > 0, "correlation setting {name} = {v} must be positive");
        }
        anyhow::ensure!(self.after_s >= 0, "correlation setting after_s = {} must not be negative", self.after_s);
        Ok(())
    }

    /// 64 bits of the digest of every setting, stored on chains as `corr:settings`, so a chain
    /// says which settings ranked it.
    pub fn hash(&self) -> String {
        let json = serde_json::to_string(self).expect("settings serialize");
        hex::encode(crate::gateway::sha2_hash(&json))[..16].to_owned()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RankMode {
    /// Netflix's chain score as published: the best chain rooted at an application wins.
    Chain,
    /// Also rewards applications whose anomaly comes first and spreads to their callers.
    Propagation,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window_s: 1800,
            after_s: 0,
            min_confidence: 0.6,
            w_time: 0.6,
            w_severity: 0.4,
            known_pair_prior: 0.9,
            depth: 4,
            max_chains: 10,
            rank: RankMode::Chain,
            known_pairs: [("DEPLOYMENT", "LOG_ERROR"), ("CONFIG_CHANGE", "METRIC_ANOMALY"), ("ALERT", "INCIDENT"), ("TRACE_FAILED", "INCIDENT")]
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            prop_weights: [0.4, 0.5, 0.1, 0.0],
            ignore_dependencies: false,
            only_signals: vec![],
            signature_window_s: 900,
            precedent_weight: 0.0,
            past_any_time: false,
            own_degradation: true,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Signal {
    pub id: Id,
    pub iri: String,
    pub app: Id,
    pub app_iri: String,
    pub app_name: String,
    pub at_ms: i64,
    /// End of the episode (`evt:until`); `at_ms` for a point event.
    pub until_ms: i64,
    pub kind: String,
    pub severity: f64,
    pub distance: usize,
    /// Set when the event's series fired again and again before the incident (see
    /// [`mark_chronic`]): background the incident did not start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chronic: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Chain {
    /// Derived from the incident and the root event, so a verdict stays on the root it judged
    /// whatever rank the chain has after the next correlation.
    pub id: String,
    pub root: Signal,
    pub links: Vec<Signal>,
    pub confidence: f64,
    /// One per event of the chain, root first: the link that leaves it. The last one closes
    /// the chain at the incident. `confidence` is their geometric mean.
    pub scores: Vec<LinkScore>,
    /// The first link with the lowest score: where the hypothesis is thinnest.
    pub weakest: Weakest,
}

/// How one link scored. `score` is `known_pair_prior` for a known pair and
/// `w_time * time + w_severity * severity` otherwise.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct LinkScore {
    /// `application:SIGNAL` of the event the link leaves.
    pub from: String,
    /// `application:SIGNAL` of the event it reaches, or `incident`.
    pub to: String,
    /// Time proximity: 1 when the next event starts before this one ends, 0 a window later.
    pub time: f64,
    pub severity: f64,
    pub known_pair: bool,
    pub edge: Edge,
    pub score: f64,
}

/// Where the dependency a link crosses comes from.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    /// Both ends are on one application, so the link crosses no dependency.
    SameApp,
    /// The catalog states it: declared in a file, or inferred by ogrep from configuration.
    Catalog,
    /// Only parent spans state it.
    Trace,
    /// Only some other source states it.
    Other,
    /// No single dependency: the link closes at the incident from more than one hop away, or
    /// dependencies are ignored.
    Indirect,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Weakest {
    pub from: String,
    pub to: String,
    pub score: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Ranked {
    pub app: String,
    pub score: f64,
    pub first_ms: i64,
    pub chain: Option<Chain>,
    /// The most similar past incident whose accepted cause has this root's application and signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precedent: Option<PastRoot>,
    /// Applications this one's chain explains, so they rank below it whatever their score: the
    /// chain reaches an event of theirs no later than the root of their own best chain, through
    /// links that each leave an anomaly still under way, from a root of at least
    /// [`EXPLAINING_SEVERITY`]. Their own chain is a later part of the same cascade, and scores
    /// higher only because the geometric mean drops this root's link.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub explains: Vec<String>,
}

/// A root people accepted as the cause of a past incident that resembles the one under analysis.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PastRoot {
    pub incident: String,
    pub similarity: f64,
    pub app: String,
    pub signal: String,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Analysis {
    pub incident: String,
    pub app: String,
    pub at_ms: i64,
    pub candidates: usize,
    /// Ranked root-cause applications, each with its best chain. A chain's `corr:rank` is its
    /// application's position here, so the number `ograph chains` prints is the one a verdict
    /// resolves.
    pub apps: Vec<Ranked>,
    /// Dependencies of the scope that the repository added after the incident opened, so
    /// correlation did not follow them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<Withheld>,
}

/// A dated dependency left out of an incident's scope because it began after the incident opened.
#[derive(Clone, Debug, Serialize)]
pub struct Withheld {
    pub caller: String,
    pub callee: String,
    pub valid_from: String,
}

/// The dated dependencies that began after `t` (Unix ms) of applications in `app`'s scope at `t`
/// near enough that `depth` would follow them.
pub fn withheld(s: &Snapshot, ids: &Ids, app: Id, depth: usize, t: i64) -> anyhow::Result<Vec<Withheld>> {
    if ids.valid_from.is_empty() {
        return Ok(vec![]);
    }
    let scope = downstream(s, ids, app, depth, Some(t))?;
    let mut out: Vec<Withheld> = ids
        .valid_from
        .iter()
        .filter(|&(&(x, _), &from)| from > t && scope.get(&x).is_some_and(|&d| d < depth))
        .map(|(&(x, y), &from)| Withheld { caller: app_name(s, ids, x), callee: app_name(s, ids, y), valid_from: term::format_millis(from) })
        .collect();
    out.sort_by(|a, b| (&a.caller, &a.callee).cmp(&(&b.caller, &b.callee)));
    Ok(out)
}

pub struct Ids {
    pub ty: Id,
    pub on: Id,
    pub at: Id,
    pub until: Option<Id>,
    pub depends: Id,
    pub value: Option<Id>,
    pub severity: Option<Id>,
    pub signal: Option<Id>,
    pub incident: Option<Id>,
    pub name: Option<Id>,
    pub changed_file: Option<Id>,
    pub code_location: Option<Id>,
    pub metric: Option<Id>,
    pub template: Option<Id>,
    /// Dependencies a repository's history dates, as (caller, callee) -> the first time, in Unix
    /// ms, that a graph stating the dependency gives it. A dependency missing here holds at every
    /// time: some graph states it without a date.
    pub valid_from: FxHashMap<(Id, Id), i64>,
}

impl Ids {
    pub fn new(s: &Snapshot) -> anyhow::Result<Self> {
        let need = |iri: &str| -> anyhow::Result<Id> { s.iri(iri)?.ok_or_else(|| anyhow::anyhow!("{} not in the store; harvest first", crate::vocab::compact(iri))) };
        Ok(Self {
            ty: need(RDF_TYPE)?,
            on: need(evt::ON)?,
            at: need(evt::AT)?,
            until: s.iri(evt::UNTIL)?,
            depends: s.iri(ops::DEPENDS_ON)?.unwrap_or(u64::MAX),
            value: s.iri(evt::VALUE)?,
            severity: s.iri(evt::SEVERITY)?,
            signal: s.iri(evt::SIGNAL)?,
            incident: s.iri(inc::INCIDENT)?,
            name: s.iri(ops::NAME)?,
            changed_file: s.iri(evt::CHANGED_FILE)?,
            code_location: s.iri(evt::CODE_LOCATION)?,
            metric: s.iri(evt::METRIC)?,
            template: s.iri(evt::TEMPLATE)?,
            valid_from: valid_from(s)?,
        })
    }

    /// The applications `app` depends on, with dependencies that began after `at` (Unix ms) left
    /// out; all of them without a time.
    pub fn callees(&self, s: &Snapshot, app: Id, at: Option<i64>) -> anyhow::Result<Vec<Id>> {
        Ok(s.objects(app, self.depends)?.into_iter().filter(|&y| self.holds(app, y, at)).collect())
    }

    /// The applications that depend on `app`, as [`Ids::callees`] filters them.
    pub fn callers(&self, s: &Snapshot, app: Id, at: Option<i64>) -> anyhow::Result<Vec<Id>> {
        Ok(s.subjects(self.depends, app)?.into_iter().filter(|&x| self.holds(x, app, at)).collect())
    }

    fn holds(&self, caller: Id, callee: Id, at: Option<i64>) -> bool {
        match (at, self.valid_from.get(&(caller, callee))) {
            (Some(at), Some(&from)) => from <= at,
            _ => true,
        }
    }
}

fn valid_from(s: &Snapshot) -> anyhow::Result<FxHashMap<(Id, Id), i64>> {
    let mut out = FxHashMap::default();
    let (Some(vf), Some(caller), Some(callee), Some(depends)) = (s.iri(ops::VALID_FROM)?, s.iri(ops::CALLER)?, s.iri(ops::CALLEE)?, s.iri(ops::DEPENDS_ON)?) else { return Ok(out) };
    let mut dated: FxHashMap<Id, FxHashMap<Id, i64>> = FxHashMap::default();
    for q in s.quads([None, Some(vf), None], crate::store::GraphSel::AnyNamed) {
        let [n, _, t, g] = q?;
        if let Some(ms) = term::millis(t) {
            let e = dated.entry(n).or_default().entry(g).or_insert(ms);
            *e = (*e).min(ms);
        }
    }
    for (n, by_graph) in dated {
        let (Some(x), Some(y)) = (s.object(n, caller)?, s.object(n, callee)?) else { continue };
        let graphs = s.graphs_of([x, depends, y])?;
        if !graphs.is_empty() && graphs.iter().all(|g| by_graph.contains_key(g)) {
            out.insert((x, y), graphs.iter().map(|g| by_graph[g]).min().unwrap_or(i64::MIN));
        }
    }
    Ok(out)
}

pub fn literal(s: &Snapshot, id: Id) -> Option<String> {
    match s.term(id).ok()? {
        Term::Literal(l) => Some(l.value().to_owned()),
        Term::NamedNode(n) => Some(n.as_str().to_owned()),
        _ => None,
    }
}

pub fn app_name(s: &Snapshot, ids: &Ids, app: Id) -> String {
    ids.name.and_then(|n| s.object(app, n).ok().flatten()).and_then(|v| literal(s, v)).unwrap_or_else(|| {
        literal(s, app).map(|i| i.rsplit('/').next().unwrap_or(&i).to_owned()).unwrap_or_default()
    })
}

/// Applications `app` depends on, transitively, with their distance (0 for `app`), through
/// dependencies that held at `at` (Unix ms).
pub fn downstream(s: &Snapshot, ids: &Ids, app: Id, depth: usize, at: Option<i64>) -> anyhow::Result<FxHashMap<Id, usize>> {
    let mut dist = FxHashMap::default();
    dist.insert(app, 0);
    let mut frontier = vec![app];
    for d in 1..=depth {
        let mut next = Vec::new();
        for x in frontier {
            for y in ids.callees(s, x, at)? {
                if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(y) {
                    e.insert(d);
                    next.push(y);
                }
            }
        }
        frontier = next;
    }
    Ok(dist)
}

/// Applications that depend on `app`, transitively, by distance: the blast radius, through
/// dependencies that held at `at` (Unix ms).
pub fn upstream(s: &Snapshot, ids: &Ids, app: Id, depth: usize, at: Option<i64>) -> anyhow::Result<Vec<(Id, usize)>> {
    let mut seen = FxHashSet::default();
    seen.insert(app);
    let mut out = Vec::new();
    let mut frontier = vec![app];
    for d in 1..=depth {
        let mut next = Vec::new();
        for x in frontier {
            for y in ids.callers(s, x, at)? {
                if seen.insert(y) {
                    out.push((y, d));
                    next.push(y);
                }
            }
        }
        frontier = next;
    }
    Ok(out)
}

fn severity_of(s: &Snapshot, ids: &Ids, e: Id) -> f64 {
    if let Some(v) = ids.value.and_then(|p| s.object(e, p).ok().flatten()).and_then(|v| literal(s, v)).and_then(|v| v.parse::<f64>().ok()) {
        return crate::enrich::signals::strength(v);
    }
    match ids.severity.and_then(|p| s.object(e, p).ok().flatten()).and_then(|v| literal(s, v)).as_deref() {
        Some("SEV1") => 1.0,
        Some("SEV2") => 0.75,
        Some("SEV3") => 0.5,
        _ => 0.25,
    }
}

fn kind_of(s: &Snapshot, ids: &Ids, e: Id) -> String {
    if let Some(v) = ids.signal.and_then(|p| s.object(e, p).ok().flatten()).and_then(|v| literal(s, v)) {
        return v;
    }
    s.object(e, ids.ty).ok().flatten().and_then(|c| literal(s, c)).map(|c| c.rsplit('#').next().unwrap_or("").to_uppercase()).unwrap_or_default()
}

/// The values of `p` on `e` as text; nothing when the store has never held `p`.
pub fn texts(s: &Snapshot, e: Id, p: Option<Id>) -> anyhow::Result<Vec<String>> {
    let Some(p) = p else { return Ok(vec![]) };
    Ok(s.objects(e, p)?.into_iter().filter_map(|v| literal(s, v)).collect())
}

/// The file of a `path:line` code location.
pub fn location_file(location: &str) -> &str {
    location.rsplit_once(':').map_or(location, |(file, _)| file)
}

/// The incident's application and time.
pub fn incident_anchor(s: &Snapshot, ids: &Ids, incident: Id) -> anyhow::Result<(Id, i64)> {
    let app = s.object(incident, ids.on)?.ok_or_else(|| anyhow::anyhow!("incident has no evt:on"))?;
    let at = s.object(incident, ids.at)?.and_then(term::millis).ok_or_else(|| anyhow::anyhow!("incident has no evt:at"))?;
    Ok((app, at))
}

/// How far before the window an episode may have started and still count, if it is still under
/// way when the window opens.
const EPISODE_LOOKBACK_MS: i64 = 24 * 3600 * 1000;

/// The IRI of the chain from `root` to `incident`: 64 bits of their digest, so two roots of
/// one incident do not share an id in practice, and `shapes.ttl` rejects a chain with two roots
/// if they ever do.
pub fn chain_iri(incident: &str, root: &str) -> String {
    let local = incident.rsplit('/').next().unwrap_or("inc");
    let h = hex::encode(crate::gateway::sha2_hash(&format!("{incident} {root}")));
    format!("{}chain/{}-{}", crate::vocab::DATA, crate::model::slug(local), &h[..16])
}

/// Events on the incident's application and everything it depends on, in the window, and
/// episodes that started before it but were still under way when it opened.
pub fn candidates(s: &Snapshot, ids: &Ids, incident: Id, st: &Settings) -> anyhow::Result<(Id, i64, Vec<Signal>)> {
    let (app, t) = incident_anchor(s, ids, incident)?;
    let mut scope = downstream(s, ids, app, st.depth, Some(t))?;
    if st.ignore_dependencies
        && let Some(c) = s.iri(ops::APPLICATION)? {
            for a in s.subjects(ids.ty, c)? {
                scope.entry(a).or_insert(1);
            }
        }
    let opens = t - st.window_s * 1000;
    let (from, to) = (term::time_id(opens - EPISODE_LOOKBACK_MS), term::time_id(t + st.after_s * 1000));
    let mut out = Vec::new();
    let mut names = FxHashMap::default();
    for (&x, &d) in &scope {
        for (tid, e) in s.events_on(x, from, to)? {
            let at_ms = term::millis(tid).unwrap_or(0);
            let until_ms = ids.until.and_then(|p| s.object(e, p).ok().flatten()).and_then(term::millis).unwrap_or(at_ms).max(at_ms);
            if until_ms < opens || e == incident || ids.incident.is_some_and(|c| s.contains([e, ids.ty, c]).unwrap_or(false)) {
                continue;
            }
            let kind = kind_of(s, ids, e);
            if !st.only_signals.is_empty() && !st.only_signals.contains(&kind) {
                continue;
            }
            let name = names.entry(x).or_insert_with(|| app_name(s, ids, x)).clone();
            out.push(Signal {
                id: e,
                iri: literal(s, e).unwrap_or_default(),
                app: x,
                app_iri: literal(s, x).unwrap_or_default(),
                app_name: name,
                at_ms,
                until_ms,
                kind,
                severity: severity_of(s, ids, e),
                distance: d,
                chronic: None,
            });
        }
    }
    out.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then(a.iri.cmp(&b.iri)));
    Ok((app, t, out))
}

/// An event as a signal, for marks that need no incident: its application, time and kind.
pub fn event_signal(s: &Snapshot, ids: &Ids, e: Id) -> anyhow::Result<Option<Signal>> {
    let (Some(app), Some(at_ms)) = (s.object(e, ids.on)?, s.object(e, ids.at)?.and_then(term::millis)) else { return Ok(None) };
    let kind = kind_of(s, ids, e);
    Ok(Some(Signal {
        id: e,
        iri: literal(s, e).unwrap_or_default(),
        app,
        app_iri: literal(s, app).unwrap_or_default(),
        app_name: app_name(s, ids, app),
        at_ms,
        until_ms: at_ms,
        kind,
        severity: severity_of(s, ids, e),
        distance: 0,
        chronic: None,
    }))
}

/// Episodes of one series in the lookback that make each of them chronic.
pub const CHRONIC_EPISODES: usize = 2;

/// The series a detected event belongs to: its signal kind and its metric or log template.
/// Deployments, changes, alerts and other one-off records have none.
fn series_of(s: &Snapshot, ids: &Ids, e: Id, kind: &str) -> Option<(String, String)> {
    let detail = |p: Option<Id>| p.and_then(|p| s.object(e, p).ok().flatten()).and_then(|v| literal(s, v));
    match detail(ids.metric).or_else(|| detail(ids.template)) {
        Some(d) => Some((kind.to_owned(), d)),
        None if kind.starts_with("TRACE_") => Some((kind.to_owned(), String::new())),
        None => None,
    }
}

/// Marks each signal whose series (same application, signal kind and metric or log template)
/// started at least [`CHRONIC_EPISODES`] episodes in `[from, to)`, Unix ms. A series that keeps
/// firing is background: sparse series reach full strength on any change, and noise fires
/// whether or not anything is wrong. Only events the view holds count, so `--known-at` applies.
pub fn mark_chronic<'a>(s: &Snapshot, ids: &Ids, from: i64, to: i64, what: &str, sigs: impl IntoIterator<Item = &'a mut Signal>) -> anyhow::Result<()> {
    let mut counts: FxHashMap<Id, FxHashMap<(String, String), usize>> = FxHashMap::default();
    for sig in sigs {
        let Some(key) = series_of(s, ids, sig.id, &sig.kind) else { continue };
        if let std::collections::hash_map::Entry::Vacant(slot) = counts.entry(sig.app) {
            let c = slot.insert(FxHashMap::default());
            for (tid, e) in s.events_on(sig.app, term::time_id(from), term::time_id(to))? {
                if term::millis(tid).is_none_or(|at| at >= to) {
                    continue;
                }
                if let Some(k) = series_of(s, ids, e, &kind_of(s, ids, e)) {
                    *c.entry(k).or_default() += 1;
                }
            }
        }
        let n = counts[&sig.app].get(&key).copied().unwrap_or(0);
        if n >= CHRONIC_EPISODES {
            let name = if key.1.is_empty() || sig.kind.starts_with("LOG") { format!("{} {}", sig.app_name, sig.kind) } else { format!("{} {}", sig.app_name, key.1) };
            sig.chronic = Some(format!("{name} fired {n} times {what}: likely background, not a new fault"));
        }
    }
    Ok(())
}

impl Settings {
    fn known(&self, from: &str, to: &str) -> bool {
        self.known_pairs.iter().any(|(a, b)| a == from && b == to)
    }

    /// Netflix's rule: a known causal pair carries its prior weight; any other pair is scored
    /// on temporal proximity and severity. Severity is in [0, 1], so a link is too.
    ///
    /// Proximity runs from the end of `a`'s episode, not its start: an anomaly still under way
    /// when the next event (or the incident) arrives is adjacent to it. Measuring from the start
    /// made a cause that began five minutes before the incident and never stopped score below a
    /// one-point blip seconds before it.
    fn link(&self, a: &Signal, to_kind: &str, to_app: Option<Id>, to_ms: i64) -> Parts {
        let w = (self.window_s * 1000) as f64;
        let gap = (to_ms - a.until_ms.min(to_ms)) as f64;
        let severity = a.severity.min(1.0);
        if ((to_ms - a.at_ms) as f64) < -1000.0 || gap > w {
            return Parts { time: 0.0, severity, known_pair: false, score: 0.0 };
        }
        let time = (1.0 - gap.max(0.0) / w).clamp(0.0, 1.0);
        let known_pair = self.known(&a.kind, to_kind) && !(self.own_degradation && to_app.is_some_and(|b| b != a.app));
        let score = if known_pair { self.known_pair_prior } else { self.w_time * time + self.w_severity * severity };
        Parts { time, severity, known_pair, score }
    }
}

struct Parts {
    time: f64,
    severity: f64,
    known_pair: bool,
    score: f64,
}

/// Where the dependency of `caller` on `callee` comes from: the harvest sources whose graphs
/// state it, the catalog first.
fn edge(s: &Snapshot, ids: &Ids, callee: Id, caller: Id) -> anyhow::Result<Edge> {
    if callee == caller {
        return Ok(Edge::SameApp);
    }
    let harvest = format!("{}harvest/", crate::vocab::GRAPH);
    let mut found = Edge::Indirect;
    for g in s.graphs_of([caller, ids.depends, callee])? {
        let graph = literal(s, g).unwrap_or_default();
        match graph.strip_prefix(&harvest).and_then(|rest| rest.split('/').next()) {
            Some("catalog") => return Ok(Edge::Catalog),
            Some("traces") => found = Edge::Trace,
            _ if found == Edge::Indirect => found = Edge::Other,
            _ => {}
        }
    }
    Ok(found)
}

/// The signal of what a person did (`evt:OperatorAction`). `before` lists these events, but
/// they are no root-cause candidates: an action in the lead-up to an incident is mostly a
/// response to symptoms that came before it opened, and a rollback or a deploy that changed the
/// code already has its own record from the deployments source.
pub const ACTION_SIGNAL: &str = "OPERATOR_ACTION";

/// [`candidates`] without operator actions: what correlation and signatures compare.
pub fn causes(s: &Snapshot, ids: &Ids, incident: Id, st: &Settings) -> anyhow::Result<(Id, i64, Vec<Signal>)> {
    let (app, t, mut cands) = candidates(s, ids, incident, st)?;
    cands.retain(|c| c.kind != ACTION_SIGNAL);
    Ok((app, t, cands))
}

/// Builds chains and ranks root-cause applications. `past` holds the accepted causes of similar
/// incidents, most similar first: a root with the application and signal of one is marked with
/// it, and gains `precedent_weight` times its similarity.
pub fn analyze(s: &Snapshot, ids: &Ids, incident: Id, st: &Settings, past: &[PastRoot]) -> anyhow::Result<Analysis> {
    let (app, t, cands) = causes(s, ids, incident, st)?;
    let incident_iri = literal(s, incident).unwrap_or_default();
    // Which application calls which, inside the scope: a link may go from an event on X to an
    // event on X or on a caller of X.
    let scope: FxHashSet<Id> = cands.iter().map(|c| c.app).chain([app]).collect();
    let mut callers: FxHashMap<Id, FxHashSet<Id>> = FxHashMap::default();
    for &x in &scope {
        if st.ignore_dependencies {
            break;
        }
        for y in ids.callers(s, x, Some(t))? {
            if scope.contains(&y) {
                callers.entry(x).or_default().insert(y);
            }
        }
    }
    let can_follow = |a: &Signal, b: &Signal| b.at_ms >= a.at_ms && b.at_ms - a.until_ms.min(b.at_ms) <= st.window_s * 1000 && (b.app == a.app || callers.get(&a.app).is_some_and(|c| c.contains(&b.app)));
    // Depth-first from each candidate root: extend through later events on callers, close each
    // path at the incident. Confidence is the geometric mean of the links. Intermediate links
    // must clear `min_confidence`; a chain is kept as a hypothesis only if its closing link and
    // its confidence clear it too, while ranking sees every root's best path.
    let mut best_any: Vec<Option<Chain>> = Vec::with_capacity(cands.len());
    let mut chains = Vec::new();
    for (ri, _) in cands.iter().enumerate() {
        let mut stack: Vec<(Vec<usize>, f64)> = vec![(vec![ri], 0.0)];
        let mut best: Option<(Vec<usize>, f64, bool)> = None;
        let mut expanded = 0;
        while let Some((path, logsum)) = stack.pop() {
            expanded += 1;
            if expanded > 2000 {
                break;
            }
            let last = &cands[*path.last().unwrap()];
            // An event inside `after_s` closes as if it happened at the incident.
            let close = st.link(last, "INCIDENT", None, t.max(last.at_ms)).score.max(1e-9);
            let conf = ((logsum + close.ln()) / path.len() as f64).exp();
            let gated = close >= st.min_confidence && conf >= st.min_confidence;
            // Equal confidence prefers the longer path: it explains more of the cascade.
            if best.as_ref().is_none_or(|b| (gated, quantize(conf), path.len()) > (b.2, quantize(b.1), b.0.len())) {
                best = Some((path.clone(), conf, gated));
            }
            if path.len() > st.depth {
                continue;
            }
            for (ni, next) in cands.iter().enumerate().skip(*path.last().unwrap() + 1) {
                if next.app == last.app && next.kind == last.kind {
                    continue;
                }
                if !can_follow(last, next) {
                    continue;
                }
                let l = st.link(last, &next.kind, Some(next.app), next.at_ms).score;
                if l >= st.min_confidence {
                    let mut p = path.clone();
                    p.push(ni);
                    stack.push((p, logsum + l.ln()));
                }
            }
        }
        let chain = match best {
            Some((path, conf, gated)) => {
                let scores = link_scores(s, ids, st, &cands, &path, app, t)?;
                let low = scores.iter().fold(&scores[0], |low, l| if l.score < low.score { l } else { low });
                let weakest = Weakest { from: low.from.clone(), to: low.to.clone(), score: low.score };
                let c = Chain { id: chain_iri(&incident_iri, &cands[path[0]].iri), root: cands[path[0]].clone(), links: path[1..].iter().map(|&i| cands[i].clone()).collect(), confidence: conf, scores, weakest };
                if gated {
                    chains.push(c.clone());
                }
                Some(c)
            }
            None => None,
        };
        best_any.push(chain);
    }
    let w_ms = (st.window_s * 1000) as f64;
    let mut per_app: FxHashMap<Id, Ranked> = FxHashMap::default();
    match st.rank {
        RankMode::Chain => {
            for c in &chains {
                let first = cands.iter().filter(|x| x.app == c.root.app).map(|x| x.at_ms).min().unwrap_or(c.root.at_ms);
                let e = per_app.entry(c.root.app).or_insert_with(|| Ranked { app: c.root.app_name.clone(), score: f64::MIN, first_ms: first, chain: None, precedent: None, explains: vec![] });
                if c.confidence > e.score {
                    e.score = c.confidence;
                    e.chain = Some(c.clone());
                }
            }
        }
        RankMode::Propagation => {
            let [ws, wc, wp, we] = st.prop_weights;
            let apps: FxHashSet<Id> = cands.iter().map(|c| c.app).collect();
            for x in apps {
                let mine: Vec<usize> = (0..cands.len()).filter(|&i| cands[i].app == x).collect();
                let strongest = *mine.iter().max_by(|&&a, &&b| cands[a].severity.total_cmp(&cands[b].severity)).unwrap();
                let strength = cands[strongest].severity.min(1.0);
                let peak_at = cands[strongest].at_ms;
                let chain = mine.iter().filter_map(|&i| best_any[i].clone()).max_by(|a, b| a.confidence.total_cmp(&b.confidence));
                let spread = callers.get(&x).map_or(0.0, |cs| {
                    let hit = cs.iter().filter(|y| cands.iter().any(|c| c.app == **y && c.at_ms >= peak_at)).count();
                    hit as f64 / cs.len().max(1) as f64
                });
                let onset = ((t - peak_at) as f64 / w_ms).clamp(0.0, 1.0);
                let score = ws * strength + wc * chain.as_ref().map_or(0.0, |c| c.confidence) + wp * spread + we * onset;
                per_app.insert(x, Ranked { app: cands[strongest].app_name.clone(), score, first_ms: cands[mine[0]].at_ms, chain, precedent: None, explains: vec![] });
            }
        }
    }
    let mut apps: Vec<Ranked> = per_app.into_values().collect();
    for r in &mut apps {
        let Some(root) = r.chain.as_ref().map(|c| &c.root) else { continue };
        r.precedent = past.iter().find(|p| p.app == root.app_name && p.signal == root.kind).cloned();
        r.score += st.precedent_weight * r.precedent.as_ref().map_or(0.0, |p| p.similarity);
    }
    match st.rank {
        // Sustained, saturated anomalies tie on confidence. Among ties the root whose chain
        // reaches more applications explains more of the cascade, then the earlier onset wins.
        RankMode::Chain => apps.sort_by_key(|r| {
            let c = r.chain.as_ref();
            (std::cmp::Reverse(quantize(r.score)), std::cmp::Reverse(c.map_or(0, breadth)), c.map_or(i64::MAX, |c| c.root.at_ms), r.app.clone())
        }),
        RankMode::Propagation => apps.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.first_ms.cmp(&b.first_ms)).then(a.app.cmp(&b.app))),
    }
    if st.own_degradation && st.rank == RankMode::Chain {
        explain(&mut apps, st.precedent_weight);
    }
    let withheld = if st.ignore_dependencies { vec![] } else { withheld(s, ids, app, st.depth, t)? };
    Ok(Analysis { incident: incident_iri, app: app_name(s, ids, app), at_ms: t, candidates: cands.len(), apps, withheld })
}

/// The least strength of a root that explains its callers: SEV2, the class the detectors give an
/// anomaly from 0.5 (crate::enrich::signals). A weaker anomaly still under way on a callee is as
/// likely background as cause, and on RCAEval such anomalies would take rank 1 from the root.
pub const EXPLAINING_SEVERITY: f64 = 0.5;

/// Fills [`Ranked::explains`] and moves each application above every one it explains, keeping
/// the order otherwise. A change explains nothing: it is a point in time, never under way. An
/// accepted cause outweighs the order of anomalies: an application whose precedent gains more
/// (`precedent_weight` above 0) is not moved below one that explains it.
fn explain(apps: &mut Vec<Ranked>, precedent_weight: f64) {
    let gain = |r: &Ranked| precedent_weight * r.precedent.as_ref().map_or(0.0, |p| p.similarity);
    let first: FxHashMap<Id, i64> = apps.iter().filter_map(|r| r.chain.as_ref()).map(|c| (c.root.app, c.root.at_ms)).collect();
    for r in apps.iter_mut() {
        let Some(c) = &r.chain else { continue };
        if c.root.severity < EXPLAINING_SEVERITY {
            continue;
        }
        let path: Vec<&Signal> = std::iter::once(&c.root).chain(&c.links).collect();
        for k in 1..path.len() {
            let (from, to) = (path[k - 1], path[k]);
            if to.app == from.app {
                continue;
            }
            if c.scores[k - 1].time < 1.0 {
                break;
            }
            if to.app != c.root.app && first.get(&to.app).is_some_and(|&at| to.at_ms <= at) && !r.explains.contains(&to.app_name) {
                r.explains.push(to.app_name.clone());
            }
        }
    }
    // Each move puts an explaining application right above one it explains; a cycle of
    // explanations stops after a bounded number of passes.
    for _ in 0..apps.len() * 2 {
        let mut moved = false;
        for x in 0..apps.len() {
            let Some(y) = (0..x).find(|&y| apps[x].explains.contains(&apps[y].app) && gain(&apps[y]) <= gain(&apps[x])) else { continue };
            let r = apps.remove(x);
            apps.insert(y, r);
            moved = true;
            break;
        }
        if !moved {
            break;
        }
    }
}

/// The score of every link along `path`, then of the link that closes it at the incident, with
/// the same calls the search scored them by. None of them is outside the window, where a link
/// scores 0 whatever its parts: the search follows only links inside it, and a candidate still
/// under way when the window opens closes inside it.
fn link_scores(s: &Snapshot, ids: &Ids, st: &Settings, cands: &[Signal], path: &[usize], incident_app: Id, t: i64) -> anyhow::Result<Vec<LinkScore>> {
    let label = |x: &Signal| format!("{}:{}", x.app_name, x.kind);
    let mut out = Vec::with_capacity(path.len());
    for (k, &i) in path.iter().enumerate() {
        let a = &cands[i];
        let (to, parts, edge) = match path.get(k + 1).map(|&n| &cands[n]) {
            Some(next) => (label(next), st.link(a, &next.kind, Some(next.app), next.at_ms), edge(s, ids, a.app, next.app)?),
            None => ("incident".to_owned(), st.link(a, "INCIDENT", None, t.max(a.at_ms)), edge(s, ids, a.app, incident_app)?),
        };
        out.push(LinkScore { from: label(a), to, time: parts.time, severity: parts.severity, known_pair: parts.known_pair, edge, score: parts.score });
    }
    Ok(out)
}

/// Confidence at the resolution ranking cares about, so float noise cannot break a tie.
fn quantize(x: f64) -> i64 {
    (x * 1e9).round() as i64
}

/// Distinct applications a chain passes through.
fn breadth(c: &Chain) -> usize {
    std::iter::once(&c.root).chain(&c.links).map(|s| s.app).collect::<FxHashSet<_>>().len()
}

#[cfg(all(test, unix))]
mod tests {
    use crate::workspace::Workspace;
    use crate::workspace::tests::{T, alert, fixture, harvest};
    use std::path::Path;

    /// The best chain of each ranked application, one line per link.
    fn links(root: &Path) -> Vec<(String, Vec<String>, String)> {
        let ws = Workspace::open(root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let ids = super::Ids::new(&snap).unwrap();
        let a = super::analyze(&snap, &ids, crate::query::lookup(&snap, "INC-1").unwrap(), &ws.config.correlation, &[]).unwrap();
        a.apps
            .iter()
            .map(|r| {
                let c = r.chain.as_ref().unwrap();
                let scores = c.scores.iter().map(|l| format!("{} -> {} {:.4} time {:.4} severity {:.2} known {} {:?}", l.from, l.to, l.score, l.time, l.severity, l.known_pair, l.edge)).collect();
                (format!("{} {:.4}", r.app, c.confidence), scores, format!("{} -> {} {:.4}", c.weakest.from, c.weakest.to, c.weakest.score))
            })
            .collect()
    }

    #[test]
    fn a_chain_says_how_each_link_scored_and_which_is_weakest() {
        let deployment = format!(r#"{{"service":"payments-api","version":"2","at":{}}}"#, T - 180);
        let root = fixture("link-scores", &[("deployments", "deployments.jsonl", &deployment), ("alerts", "alerts.json", &format!("[{}]", alert("checkout", "ErrorRate", T - 60)))]);
        harvest(&root, None);
        // 0.6 * (1 - 120 s / 1800 s) + 0.4 * 0.25 = 0.66, and sqrt(0.66 * 0.9) = 0.7707.
        let deployed = (
            "payments-api 0.7707".to_owned(),
            vec![
                "payments-api:DEPLOYMENT -> checkout:ALERT 0.6600 time 0.9333 severity 0.25 known false Catalog".to_owned(),
                "checkout:ALERT -> incident 0.9000 time 0.9667 severity 1.00 known true SameApp".to_owned(),
            ],
            "payments-api:DEPLOYMENT -> checkout:ALERT 0.6600".to_owned(),
        );
        let alerted = ("checkout 0.9000".to_owned(), vec!["checkout:ALERT -> incident 0.9000 time 0.9667 severity 1.00 known true SameApp".to_owned()], "checkout:ALERT -> incident 0.9000".to_owned());
        assert_eq!(links(&root), [alerted, deployed]);
    }

    fn sig(app: u64, name: &str, kind: &str, at_s: i64, until_s: i64, severity: f64) -> super::Signal {
        super::Signal { id: app * 100 + at_s as u64, iri: format!("ev/{name}-{at_s}"), app, app_iri: String::new(), app_name: name.into(), at_ms: at_s * 1000, until_ms: until_s * 1000, kind: kind.into(), severity, distance: 0, chronic: None }
    }

    /// A ranked application whose best chain is `path`, closing at the incident at 1000 s.
    fn ranked(path: Vec<super::Signal>) -> super::Ranked {
        let st = super::Settings::default();
        let mut scores = Vec::new();
        for (k, a) in path.iter().enumerate() {
            let (to, to_app, to_ms) = path.get(k + 1).map_or(("INCIDENT", None, 1_000_000), |b| (b.kind.as_str(), Some(b.app), b.at_ms));
            let p = st.link(a, to, to_app, to_ms);
            scores.push(super::LinkScore { from: a.app_name.clone(), to: to.into(), time: p.time, severity: p.severity, known_pair: p.known_pair, edge: super::Edge::Catalog, score: p.score });
        }
        let confidence = (scores.iter().map(|l| l.score.ln()).sum::<f64>() / scores.len() as f64).exp();
        let weakest = super::Weakest { from: String::new(), to: String::new(), score: 0.0 };
        let chain = super::Chain { id: path[0].iri.clone(), root: path[0].clone(), links: path[1..].to_vec(), confidence, scores, weakest };
        super::Ranked { app: path[0].app_name.clone(), score: confidence, first_ms: path[0].at_ms, chain: Some(chain), precedent: None, explains: vec![] }
    }

    #[test]
    fn a_known_pair_holds_within_one_application() {
        let st = super::Settings::default();
        let deploy = sig(1, "payments-api", "DEPLOYMENT", 700, 700, 0.25);
        assert_eq!(st.link(&deploy, "LOG_ERROR", Some(1), 760_000).score, 0.9);
        // On a caller the pair's prior does not hold: 0.6 * (1 - 60 s / 1800 s) + 0.4 * 0.25.
        let across = st.link(&deploy, "LOG_ERROR", Some(2), 760_000);
        assert!(!across.known_pair);
        assert!((across.score - 0.68).abs() < 1e-9);
        let old = super::Settings { own_degradation: false, ..st };
        assert_eq!(old.link(&deploy, "LOG_ERROR", Some(2), 760_000).score, 0.9);
    }

    #[test]
    fn an_application_ranks_below_the_one_whose_chain_explains_it() {
        // ledger-db degrades first and is still degraded when payments-api, its caller, does;
        // payments-api's own chain scores higher only because it leaves out ledger-db's link.
        let ledger = sig(3, "ledger-db", "METRIC_ANOMALY", 600, 1000, 0.8);
        let payments = sig(2, "payments-api", "METRIC_ANOMALY", 650, 1000, 1.0);
        let deploy = sig(4, "fx-api", "DEPLOYMENT", 500, 500, 0.25);
        let mut apps = vec![ranked(vec![payments.clone()]), ranked(vec![ledger.clone(), payments.clone()]), ranked(vec![deploy, payments.clone()])];
        assert!(apps[0].score > apps[1].score);
        super::explain(&mut apps, 0.0);
        let order: Vec<(&str, Vec<String>)> = apps.iter().map(|r| (r.app.as_str(), r.explains.clone())).collect();
        // The deployment is a point in time, never under way when payments-api degrades.
        assert_eq!(order, [("ledger-db", vec!["payments-api".to_owned()]), ("payments-api", vec![]), ("fx-api", vec![])]);
        // An anomaly weaker than SEV2 explains nothing, and neither does one that ended first.
        for weak in [sig(3, "ledger-db", "METRIC_ANOMALY", 600, 1000, 0.3), sig(3, "ledger-db", "METRIC_ANOMALY", 600, 610, 0.8)] {
            let mut apps = vec![ranked(vec![payments.clone()]), ranked(vec![weak, payments.clone()])];
            super::explain(&mut apps, 0.0);
            assert_eq!((apps[0].app.as_str(), apps[1].explains.len()), ("payments-api", 0));
        }
    }

    #[test]
    fn a_link_names_where_its_dependency_comes_from() {
        let root = fixture("link-edges", &[("alerts", "alerts.json", &format!("[{}]", alert("ledger-db", "PoolSaturated", T - 600)))]);
        harvest(&root, None);
        // checkout reaches ledger-db only through payments-api, so no one dependency closes the chain.
        assert_eq!(links(&root)[0].1, ["ledger-db:ALERT -> incident 0.9000 time 0.6667 severity 1.00 known true Indirect"]);
        let span = |id: &str, parent: &str, service: &str| format!(r#"{{"trace_id":"t1","span_id":"{id}","parent_span_id":"{parent}","service":"{service}","start_ms":{}000,"duration_us":1000,"status":"ok"}}"#, T - 900);
        std::fs::write(root.join("traces.jsonl"), format!("{}\n{}\n", span("a", "", "checkout"), span("b", "a", "ledger-db"))).unwrap();
        let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
        std::fs::write(root.join(".ograph/config.toml"), config + "[[source]]\nkind = \"traces\"\npath = \"traces.jsonl\"\n").unwrap();
        harvest(&root, None);
        assert_eq!(links(&root)[0].1, ["ledger-db:ALERT -> incident 0.9000 time 0.6667 severity 1.00 known true Trace"]);
    }
}
