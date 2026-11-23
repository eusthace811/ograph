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
#[serde(default)]
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
}

#[derive(Clone, Debug, Serialize)]
pub struct Chain {
    pub root: Signal,
    pub links: Vec<Signal>,
    pub confidence: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Ranked {
    pub app: String,
    pub score: f64,
    pub first_ms: i64,
    pub chain: Option<Chain>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Analysis {
    pub incident: String,
    pub app: String,
    pub at_ms: i64,
    pub candidates: usize,
    pub chains: Vec<Chain>,
    pub apps: Vec<Ranked>,
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
        })
    }
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

/// Applications `app` depends on, transitively, with their distance (0 for `app`).
pub fn downstream(s: &Snapshot, ids: &Ids, app: Id, depth: usize) -> anyhow::Result<FxHashMap<Id, usize>> {
    let mut dist = FxHashMap::default();
    dist.insert(app, 0);
    let mut frontier = vec![app];
    for d in 1..=depth {
        let mut next = Vec::new();
        for x in frontier {
            for y in s.objects(x, ids.depends)? {
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

/// Applications that depend on `app`, transitively, by distance: the blast radius.
pub fn upstream(s: &Snapshot, ids: &Ids, app: Id, depth: usize) -> anyhow::Result<Vec<(Id, usize)>> {
    let mut seen = FxHashSet::default();
    seen.insert(app);
    let mut out = Vec::new();
    let mut frontier = vec![app];
    for d in 1..=depth {
        let mut next = Vec::new();
        for x in frontier {
            for y in s.subjects(ids.depends, x)? {
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

/// The incident's application and time.
pub fn incident_anchor(s: &Snapshot, ids: &Ids, incident: Id) -> anyhow::Result<(Id, i64)> {
    let app = s.object(incident, ids.on)?.ok_or_else(|| anyhow::anyhow!("incident has no evt:on"))?;
    let at = s.object(incident, ids.at)?.and_then(term::millis).ok_or_else(|| anyhow::anyhow!("incident has no evt:at"))?;
    Ok((app, at))
}

/// Events on the incident's application and everything it depends on, in the window.
pub fn candidates(s: &Snapshot, ids: &Ids, incident: Id, st: &Settings) -> anyhow::Result<(Id, i64, Vec<Signal>)> {
    let (app, t) = incident_anchor(s, ids, incident)?;
    let mut scope = downstream(s, ids, app, st.depth)?;
    if st.ignore_dependencies
        && let Some(c) = s.iri(ops::APPLICATION)? {
            for a in s.subjects(ids.ty, c)? {
                scope.entry(a).or_insert(1);
            }
        }
    let (from, to) = (term::time_id(t - st.window_s * 1000), term::time_id(t + st.after_s * 1000));
    let mut out = Vec::new();
    let mut names = FxHashMap::default();
    for (&x, &d) in &scope {
        for (tid, e) in s.events_on(x, from, to)? {
            if e == incident || ids.incident.is_some_and(|c| s.contains([e, ids.ty, c]).unwrap_or(false)) {
                continue;
            }
            let kind = kind_of(s, ids, e);
            if !st.only_signals.is_empty() && !st.only_signals.contains(&kind) {
                continue;
            }
            let name = names.entry(x).or_insert_with(|| app_name(s, ids, x)).clone();
            let at_ms = term::millis(tid).unwrap_or(0);
            let until_ms = ids.until.and_then(|p| s.object(e, p).ok().flatten()).and_then(term::millis).unwrap_or(at_ms).max(at_ms);
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
            });
        }
    }
    out.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then(a.iri.cmp(&b.iri)));
    Ok((app, t, out))
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
    fn link(&self, a: &Signal, to_kind: &str, to_ms: i64) -> f64 {
        let w = (self.window_s * 1000) as f64;
        let gap = (to_ms - a.at_ms) as f64;
        if gap < -1000.0 || gap > w {
            return 0.0;
        }
        if self.known(&a.kind, to_kind) {
            return self.known_pair_prior;
        }
        let gap = (to_ms - a.until_ms.min(to_ms)) as f64;
        let prox = (1.0 - gap.max(0.0) / w).clamp(0.0, 1.0);
        self.w_time * prox + self.w_severity * a.severity.min(1.0)
    }
}

/// Builds chains and ranks root-cause applications.
pub fn analyze(s: &Snapshot, ids: &Ids, incident: Id, st: &Settings) -> anyhow::Result<Analysis> {
    let (app, t, cands) = candidates(s, ids, incident, st)?;
    // Which application calls which, inside the scope: a link may go from an event on X to an
    // event on X or on a caller of X.
    let scope: FxHashSet<Id> = cands.iter().map(|c| c.app).chain([app]).collect();
    let mut callers: FxHashMap<Id, FxHashSet<Id>> = FxHashMap::default();
    for &x in &scope {
        if st.ignore_dependencies {
            break;
        }
        for y in s.subjects(ids.depends, x)? {
            if scope.contains(&y) {
                callers.entry(x).or_default().insert(y);
            }
        }
    }
    let can_follow = |a: &Signal, b: &Signal| b.at_ms >= a.at_ms && b.at_ms - a.at_ms <= st.window_s * 1000 && (b.app == a.app || callers.get(&a.app).is_some_and(|c| c.contains(&b.app)));
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
            let close = st.link(last, "INCIDENT", t.max(last.at_ms)).max(1e-9);
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
                let l = st.link(last, &next.kind, next.at_ms);
                if l >= st.min_confidence {
                    let mut p = path.clone();
                    p.push(ni);
                    stack.push((p, logsum + l.ln()));
                }
            }
        }
        let chain = best.map(|(path, conf, gated)| {
            let c = Chain { root: cands[path[0]].clone(), links: path[1..].iter().map(|&i| cands[i].clone()).collect(), confidence: conf };
            if gated {
                chains.push(c.clone());
            }
            c
        });
        best_any.push(chain);
    }
    let w_ms = (st.window_s * 1000) as f64;
    let mut per_app: FxHashMap<Id, Ranked> = FxHashMap::default();
    match st.rank {
        RankMode::Chain => {
            for c in &chains {
                let first = cands.iter().filter(|x| x.app == c.root.app).map(|x| x.at_ms).min().unwrap_or(c.root.at_ms);
                let e = per_app.entry(c.root.app).or_insert_with(|| Ranked { app: c.root.app_name.clone(), score: f64::MIN, first_ms: first, chain: None });
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
                per_app.insert(x, Ranked { app: cands[strongest].app_name.clone(), score, first_ms: cands[mine[0]].at_ms, chain });
            }
        }
    }
    let mut apps: Vec<Ranked> = per_app.into_values().collect();
    match st.rank {
        // Sustained, saturated anomalies tie on confidence. Among ties the root whose chain
        // reaches more applications explains more of the cascade, then the earlier onset wins.
        RankMode::Chain => apps.sort_by_key(|r| {
            let c = r.chain.as_ref();
            (std::cmp::Reverse(quantize(r.score)), std::cmp::Reverse(c.map_or(0, breadth)), c.map_or(i64::MAX, |c| c.root.at_ms), r.app.clone())
        }),
        RankMode::Propagation => apps.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.first_ms.cmp(&b.first_ms)).then(a.app.cmp(&b.app))),
    }
    chains.sort_by(|a, b| b.confidence.total_cmp(&a.confidence).then(a.root.at_ms.cmp(&b.root.at_ms)));
    chains.truncate(st.max_chains);
    Ok(Analysis { incident: literal(s, incident).unwrap_or_default(), app: app_name(s, ids, app), at_ms: t, candidates: cands.len(), chains, apps })
}

/// Confidence at the resolution ranking cares about, so float noise cannot break a tie.
fn quantize(x: f64) -> i64 {
    (x * 1e9).round() as i64
}

/// Distinct applications a chain passes through.
fn breadth(c: &Chain) -> usize {
    std::iter::once(&c.root).chain(&c.links).map(|s| s.app).collect::<FxHashSet<_>>().len()
}
