//! The validation gateway. A batch of [`ProposedFact`]s becomes one named graph, and every
//! rejected fact lands in the batch's quarantine graph with its reason. Nothing else writes.
//!
//! Checks, in order: IRI scheme, catalog resolution, ontology membership and catalog-only
//! classes, closed vocabularies and LLM confidence, evidence, then incremental SHACL on the
//! nodes the batch touches (domain, range, cardinality), seen against the store as it will be.

use crate::model::{Batch, EntityRef, Extraction, Object, ProposedFact, entity, slug};
use crate::ontology::Ontology;
use crate::shacl::{self, Graph};
use crate::store::{GraphSel, Snapshot, Store};
use crate::term::{self, Id};
use crate::vocab::{self, RDF_TYPE, RDFS_LABEL, corr, harv, ops};
use oxrdf::vocab::xsd;
use oxrdf::{Literal, NamedNode, Term, Triple};
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::collections::HashMap;

pub const MIN_LLM_CONFIDENCE: f64 = 0.5;
pub const FUZZY_ACCEPT: f64 = 0.93;
pub const FUZZY_QUARANTINE: f64 = 0.80;
/// A fuzzy match must beat the best match on any other entity by this much.
pub const FUZZY_MARGIN: f64 = 0.03;
/// Two words this unlike are different words, not a typo of one another.
const SIBLING_WORD: f64 = 0.8;

/// Structural facts a model may propose only when the catalog or telemetry already shows them.
/// Co-occurring events do not count: on RCAEval they confirm invented edges almost as often as
/// real ones (docs/evidence/confirm.txt).
pub const NEEDS_SUPPORT: [&str; 1] = [ops::DEPENDS_ON];
pub const SUPPORTING_SOURCES: [&str; 2] = ["catalog", "traces"];

/// The supporting source that holds `s p o` outside the graph being replaced, if any.
pub fn supported_by(snap: &Snapshot, s: &NamedNode, p: &NamedNode, o: &NamedNode, replaced: Option<Id>) -> Option<&'static str> {
    let id = |t: Term| snap.id(&t).ok().flatten();
    let spo = [id(s.clone().into())?, id(p.clone().into())?, id(o.clone().into())?];
    snap.graphs_of(spo).unwrap_or_default().into_iter().filter(|g| Some(*g) != replaced).find_map(|g| {
        let Ok(Term::NamedNode(name)) = snap.term(g) else { return None };
        SUPPORTING_SOURCES.into_iter().find(|x| name.as_str().starts_with(&format!("{}harvest/{x}/", vocab::GRAPH)))
    })
}

fn iri_scheme() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^https://ograph\.dev/id/[a-z][a-z0-9-]*/[a-z0-9][a-z0-9._-]*(/[a-z0-9._-]+)*$").unwrap())
}

/// Names and aliases of catalog entities, normalized, per class.
#[derive(Default, Clone)]
pub struct Catalog {
    names: HashMap<(&'static str, String), (NamedNode, Vec<String>)>,
    known: FxHashSet<String>,
    /// Names that missed an exact match, so a name repeated across a harvest is scored once.
    misses: RefCell<HashMap<(&'static str, String), Resolution>>,
}

#[derive(Clone)]
pub enum Resolution {
    Exact(NamedNode),
    Fuzzy(NamedNode, f64),
    Ambiguous(NamedNode, f64),
    Missing,
}

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// `query` names a sibling of the catalog name `known` rather than misspelling it: the same
/// words but one, and that one unlike its counterpart (`inventory-service-uk` against
/// `inventory-service-eu`, `v1` against `v2`, but not `svc` against `service`), or the same words plus some that are not filler
/// (`inventory-service-eu-canary` against `inventory-service-eu`, but not `checkout-svc`
/// against `checkout`).
fn sibling(query: &[String], known: &[String]) -> bool {
    if query.len() != known.len() {
        let mut rest = query.iter();
        let contains = known.iter().all(|w| rest.any(|x| x == w));
        return contains && query.iter().any(|w| !known.contains(w) && !FILLER.contains(&w.as_str()));
    }
    let mut diff = query.iter().zip(known).filter(|(x, y)| x != y);
    let filler = |w: &String| FILLER.contains(&w.as_str());
    matches!((diff.next(), diff.next()), (Some((x, y)), None) if strsim::jaro_winkler(x, y) < SIBLING_WORD && !(filler(x) && filler(y)))
}

/// Words that name the kind of thing rather than which one.
pub(crate) const FILLER: [&str; 6] = ["svc", "service", "srv", "server", "app", "api"];

const CATALOG_CLASSES: [&str; 2] = [ops::APPLICATION, ops::TEAM];

/// The sources whose batches write catalog entities, names or aliases. Every other batch of a
/// harvest checks against one catalog built once.
const NAMING_SOURCES: [&str; 3] = ["catalog", "ownership", "docs"];

impl Catalog {
    /// The catalog in `snap`, without what only the `replaced` graph says.
    pub fn from_snapshot(snap: &Snapshot, replaced: Option<Id>) -> crate::store::Result<Self> {
        let mut c = Catalog::default();
        let (Some(ty), name, alias) = (snap.iri(RDF_TYPE)?, snap.iri(ops::NAME)?, snap.iri(ops::ALIAS)?) else { return Ok(c) };
        let outside = |spo: [Id; 3]| replaced.is_none_or(|g| snap.graphs_of(spo).is_ok_and(|gs| gs.iter().any(|x| *x != g)));
        for class in CATALOG_CLASSES {
            let Some(cid) = snap.iri(class)? else { continue };
            for e in snap.subjects(ty, cid)? {
                let Term::NamedNode(n) = snap.term(e)? else { continue };
                if !outside([e, ty, cid]) {
                    continue;
                }
                c.known.insert(n.as_str().to_owned());
                for p in [name, alias].into_iter().flatten() {
                    for v in snap.objects(e, p)? {
                        if !outside([e, p, v]) {
                            continue;
                        }
                        if let Term::Literal(l) = snap.term(v)? {
                            c.names.insert((class, norm(l.value())), (n.clone(), words(l.value())));
                        }
                    }
                }
                let local = n.as_str().rsplit('/').next().unwrap_or_default().to_owned();
                c.names.insert((class, norm(&local)), (n.clone(), words(&local)));
            }
        }
        Ok(c)
    }

    pub fn add(&mut self, class: &'static str, iri: &NamedNode, names: &[&str]) {
        self.misses.get_mut().clear();
        self.known.insert(iri.as_str().to_owned());
        for n in names {
            self.names.insert((class, norm(n)), (iri.clone(), words(n)));
        }
    }

    pub fn contains(&self, iri: &str) -> bool {
        self.known.contains(iri)
    }

    pub fn resolve(&self, class: &'static str, name: &str) -> Resolution {
        let key = norm(name);
        if key.is_empty() {
            return Resolution::Missing;
        }
        if let Some((n, _)) = self.names.get(&(class, key.clone())) {
            return Resolution::Exact(n.clone());
        }
        if let Some(r) = self.misses.borrow().get(&(class, name.to_owned())) {
            return r.clone();
        }
        let r = self.score(class, name, &key);
        self.misses.borrow_mut().insert((class, name.to_owned()), r.clone());
        r
    }

    fn score(&self, class: &'static str, name: &str, key: &str) -> Resolution {
        let mut scored: Vec<(f64, &NamedNode, &[String])> =
            self.names.iter().filter(|((c, _), _)| *c == class).map(|((_, k), (n, w))| (strsim::jaro_winkler(key, k), n, w.as_slice())).collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
        let Some(&(s, n, w)) = scored.first() else { return Resolution::Missing };
        let runner_up = scored.iter().find(|x| x.1 != n).map_or(0.0, |x| x.0);
        match s {
            _ if s < FUZZY_QUARANTINE => Resolution::Missing,
            _ if s >= FUZZY_ACCEPT && s - runner_up >= FUZZY_MARGIN && !sibling(&words(name), w) => Resolution::Fuzzy(n.clone(), s),
            _ => Resolution::Ambiguous(n.clone(), s),
        }
    }
}

/// The store as it will look after the batch commits: union minus what only the replaced graph
/// holds, plus the accepted facts, plus the ontology for class reasoning.
struct Overlay<'a> {
    snap: &'a Snapshot,
    ont: &'a Ontology,
    add: oxrdf::Graph,
    replaced: Option<Id>,
    ids: RefCell<FxHashMap<Term, Option<Id>>>,
}

impl Overlay<'_> {
    fn id(&self, t: &Term) -> Option<Id> {
        if let Some(v) = self.ids.borrow().get(t) {
            return *v;
        }
        let v = self.snap.id(t).ok().flatten();
        self.ids.borrow_mut().insert(t.clone(), v);
        v
    }
    fn visible(&self, spo: [Id; 3]) -> bool {
        match self.replaced {
            None => true,
            Some(g) => self.snap.graphs_of(spo).map(|gs| gs.iter().any(|x| *x != g)).unwrap_or(true),
        }
    }
    fn store_quads(&self, spo: [Option<Id>; 3]) -> Vec<[Id; 3]> {
        self.snap
            .quads(spo, GraphSel::Union)
            .filter_map(Result::ok)
            .map(|q| [q[0], q[1], q[2]])
            .filter(|t| self.visible(*t))
            .collect()
    }
    fn term(&self, id: Id) -> Option<Term> {
        self.snap.term(id).ok()
    }
}

impl Graph for Overlay<'_> {
    fn objects(&self, s: &Term, p: &NamedNode) -> Vec<Term> {
        let mut out = self.add.objects(s, p);
        out.extend(self.ont.graph.objects(s, p));
        if let (Some(si), Some(pi)) = (self.id(s), self.id(&p.clone().into())) {
            out.extend(self.store_quads([Some(si), Some(pi), None]).into_iter().filter_map(|t| self.term(t[2])));
        }
        out.sort_by_cached_key(Term::to_string);
        out.dedup();
        out
    }
    fn subjects(&self, p: &NamedNode, o: &Term) -> Vec<Term> {
        let mut out = self.add.subjects(p, o);
        if let (Some(pi), Some(oi)) = (self.id(&p.clone().into()), self.id(o)) {
            out.extend(self.store_quads([None, Some(pi), Some(oi)]).into_iter().filter_map(|t| self.term(t[0])));
        }
        out.sort_by_cached_key(Term::to_string);
        out.dedup();
        out
    }
    fn outgoing(&self, s: &Term) -> Vec<(NamedNode, Term)> {
        let mut out = self.add.outgoing(s);
        if let Some(si) = self.id(s) {
            for t in self.store_quads([Some(si), None, None]) {
                if let (Some(Term::NamedNode(p)), Some(o)) = (self.term(t[1]), self.term(t[2])) {
                    out.push((p, o));
                }
            }
        }
        out.sort_by_cached_key(|(p, o)| format!("{p} {o}"));
        out.dedup();
        out
    }
    fn with_predicate(&self, p: &NamedNode) -> Vec<(Term, Term)> {
        let mut out = self.add.with_predicate(p);
        if let Some(pi) = self.id(&p.clone().into()) {
            for t in self.store_quads([None, Some(pi), None]) {
                if let (Some(s), Some(o)) = (self.term(t[0]), self.term(t[2])) {
                    out.push((s, o));
                }
            }
        }
        out
    }
    fn superclasses(&self, class: &Term) -> Vec<Term> {
        match class {
            Term::NamedNode(n) => match self.ont.classes.get(n.as_str()) {
                Some(sups) => sups.iter().map(|c| NamedNode::new_unchecked(c).into()).collect(),
                None => vec![class.clone()],
            },
            _ => vec![class.clone()],
        }
    }
    fn as_dyn(&self) -> &dyn Graph {
        self
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Outcome {
    pub graph: String,
    pub proposed: usize,
    pub accepted: usize,
    pub quarantined: Vec<Rejection>,
    pub removed: usize,
    pub added: usize,
    pub fuzzy_matches: usize,
    pub degraded: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Rejection {
    pub fact: String,
    pub reason: String,
}

struct Resolved {
    triple: Triple,
    fact: usize,
}

fn kind_of(class: &str) -> String {
    slug(class.rsplit(['#', '/']).next().unwrap_or("thing"))
}

/// Runs the checks against `catalog` without writing. Returns the accepted triples and the
/// rejections.
pub fn check(snap: &Snapshot, ont: &Ontology, batch: &Batch, replaced: Option<Id>, catalog: &Catalog) -> (Vec<(Triple, usize)>, Vec<Rejection>, usize) {
    let mut catalog = std::borrow::Cow::Borrowed(catalog);
    let ty = NamedNode::new_unchecked(RDF_TYPE);
    let name_p = NamedNode::new_unchecked(ops::NAME);
    let alias_p = NamedNode::new_unchecked(ops::ALIAS);
    // Catalog facts in this batch define catalog entities before anything else resolves.
    for f in &batch.facts {
        if !matches!(f.method, Extraction::Catalog { .. }) || f.predicate != ty {
            continue;
        }
        if let (EntityRef::Known(s), Object::Entity(EntityRef::Known(c))) = (&f.subject, &f.object)
            && let Some(class) = CATALOG_CLASSES.iter().find(|k| **k == c.as_str()) {
                let names: Vec<String> = batch
                    .facts
                    .iter()
                    .filter(|g| g.subject == f.subject && (g.predicate == name_p || g.predicate == alias_p))
                    .filter_map(|g| match &g.object {
                        Object::Literal(l) => Some(l.value().to_owned()),
                        _ => None,
                    })
                    .collect();
                catalog.to_mut().add(class, s, &names.iter().map(String::as_str).collect::<Vec<_>>());
            }
    }
    let mut rejected: Vec<Rejection> = batch.withheld.iter().map(|(f, reason)| Rejection { fact: f.describe(), reason: reason.clone() }).collect();
    let mut fuzzy = 0;
    let mut accepted: Vec<Resolved> = Vec::new();
    let mut minted: FxHashSet<(NamedNode, &'static str)> = FxHashSet::default();
    let resolve = |r: &EntityRef, catalog_method: bool, fuzzy: &mut usize, minted: &mut FxHashSet<(NamedNode, &'static str)>| -> Result<NamedNode, String> {
        match r {
            EntityRef::Known(n) => {
                if !n.as_str().starts_with(vocab::DATA) && n.as_str().starts_with("https://ograph.dev/") && !ont.is_class(n.as_str()) {
                    return Err(format!("{} is not an ontology class", vocab::compact(n.as_str())));
                }
                if !iri_scheme().is_match(n.as_str()) && !ont.is_class(n.as_str()) {
                    return Err(format!("IRI {n} does not follow the id scheme"));
                }
                let is_catalog_kind = ["app/", "team/"].iter().any(|k| n.as_str()[vocab::DATA.len().min(n.as_str().len())..].starts_with(k));
                if is_catalog_kind && !catalog_method && !catalog.contains(n.as_str()) {
                    return Err(format!("{} is not in the catalog", vocab::compact(n.as_str())));
                }
                Ok(n.clone())
            }
            EntityRef::Provisional { class, name } => {
                if CATALOG_CLASSES.contains(class) {
                    match catalog.resolve(class, name) {
                        Resolution::Exact(n) => Ok(n),
                        Resolution::Fuzzy(n, _) => {
                            *fuzzy += 1;
                            Ok(n)
                        }
                        Resolution::Ambiguous(n, s) if s >= FUZZY_ACCEPT => Err(format!("\"{name}\" is close to {} ({s:.2}) but names a sibling or ties another entity; not created", vocab::compact(n.as_str()))),
                        Resolution::Ambiguous(n, s) => Err(format!("\"{name}\" only fuzzily matches {} ({s:.2}); not created", vocab::compact(n.as_str()))),
                        Resolution::Missing => Err(format!("\"{name}\" is not in the {} catalog; not created", vocab::compact(class))),
                    }
                } else if !ont.is_class(class) {
                    Err(format!("{class} is not an ontology class"))
                } else {
                    let n = entity(&kind_of(class), name);
                    minted.insert((n.clone(), class));
                    Ok(n)
                }
            }
        }
    };
    for (i, f) in batch.facts.iter().enumerate() {
        let reject = |reason: String| Rejection { fact: f.describe(), reason };
        let catalog_method = matches!(f.method, Extraction::Catalog { .. });
        let p = f.predicate.as_str();
        if p != RDF_TYPE && p != RDFS_LABEL && ont.property(p).is_none() {
            rejected.push(reject(format!("{} is not an ontology property", vocab::compact(p))));
            continue;
        }
        if f.evidence.is_empty() {
            rejected.push(reject("no evidence".into()));
            continue;
        }
        if !matches!(f.method, Extraction::Human { .. })
            && let Some(e) = f.evidence.iter().find(|e| !batch.records.contains(&e.0)) {
                rejected.push(reject(format!("evidence {} is not a record this harvest read", e.0)));
                continue;
            }
        if let Extraction::Llm { confidence, .. } = &f.method
            && confidence.get() < MIN_LLM_CONFIDENCE {
                rejected.push(reject(format!("model confidence {:.2} is below {MIN_LLM_CONFIDENCE}", confidence.get())));
                continue;
            }
        let s = match resolve(&f.subject, catalog_method, &mut fuzzy, &mut minted) {
            Ok(s) => s,
            Err(e) => {
                rejected.push(reject(e));
                continue;
            }
        };
        let o: Term = match &f.object {
            Object::Literal(l) => l.clone().into(),
            Object::Entity(r) => match resolve(r, catalog_method, &mut fuzzy, &mut minted) {
                Ok(n) => n.into(),
                Err(e) => {
                    rejected.push(reject(e));
                    continue;
                }
            },
        };
        if p == RDF_TYPE {
            let Term::NamedNode(c) = &o else { unreachable!("types are entities") };
            if !ont.is_class(c.as_str()) {
                rejected.push(reject(format!("{} is not an ontology class", vocab::compact(c.as_str()))));
                continue;
            }
            if CATALOG_CLASSES.contains(&c.as_str()) && !catalog_method {
                rejected.push(reject(format!("only the catalog may create a {}", vocab::compact(c.as_str()))));
                continue;
            }
        }
        if let (Some(v), Term::Literal(l)) = (ont.vocabulary(p), &o)
            && !v.iter().any(|t| t.value == l.value()) {
                rejected.push(reject(format!("\"{}\" is not in the closed vocabulary of {}", l.value(), vocab::compact(p))));
                continue;
            }
        if let Term::NamedNode(on) = &o
            && matches!(f.method, Extraction::Llm { .. })
            && NEEDS_SUPPORT.contains(&p)
            && supported_by(snap, &s, &f.predicate, on, replaced).is_none()
        {
            rejected.push(reject(format!("unconfirmed: no {} edge backs this model-proposed {}", SUPPORTING_SOURCES.join(" or "), vocab::compact(p))));
            continue;
        }
        if let Term::Literal(l) = &o
            && p == ops::ALIAS
            && let Resolution::Exact(other) = catalog.resolve(ops::APPLICATION, l.value())
            && other != s
        {
            rejected.push(reject(format!("alias \"{}\" already names {}", l.value(), vocab::compact(other.as_str()))));
            continue;
        }
        accepted.push(Resolved { triple: Triple::new(s, f.predicate.clone(), o), fact: i });
    }
    // Two applications claiming one alias in the same batch: neither gets it, whatever the order.
    let alias_of = |a: &Resolved| match &a.triple.object {
        Term::Literal(l) if a.triple.predicate.as_str() == ops::ALIAS => Some(norm(l.value())),
        _ => None,
    };
    let mut claims: FxHashMap<String, std::collections::BTreeSet<String>> = FxHashMap::default();
    for a in &accepted {
        if let Some(k) = alias_of(a) {
            claims.entry(k).or_default().insert(vocab::compact(a.triple.subject.to_string().trim_matches(['<', '>'])));
        }
    }
    accepted.retain(|a| match alias_of(a).and_then(|k| claims.get(&k)).filter(|c| c.len() > 1) {
        Some(c) => {
            let names: Vec<&str> = c.iter().map(String::as_str).collect();
            rejected.push(Rejection { fact: batch.facts[a.fact].describe(), reason: format!("alias is claimed by {} in one batch", names.join(" and ")) });
            false
        }
        None => true,
    });
    for (n, class) in minted {
        accepted.push(Resolved { triple: Triple::new(n, ty.clone(), NamedNode::new_unchecked(class)), fact: usize::MAX });
    }
    // Incremental SHACL on the touched nodes, repeated until no violation remains, because
    // removing a fact can break another shape. Each pass drops a fact or stops.
    let mut overlay = Overlay { snap, ont, add: oxrdf::Graph::new(), replaced, ids: RefCell::default() };
    loop {
        overlay.add = accepted.iter().map(|r| r.triple.clone()).collect();
        let focus: Vec<Term> = {
            let mut f: Vec<Term> = accepted.iter().map(|r| Term::from(r.triple.subject.clone())).collect();
            f.sort_by_cached_key(Term::to_string);
            f.dedup();
            f
        };
        let results = shacl::Validator::new(&ont.shapes, &overlay).validate_focus(&focus);
        if results.is_empty() {
            break;
        }
        let mut drop: FxHashSet<usize> = FxHashSet::default();
        let mut why: FxHashMap<usize, String> = FxHashMap::default();
        for r in &results {
            let prop = r.path.as_deref().map(|p| p.trim_matches(['<', '>']).to_owned()).or_else(|| {
                let local = r.shape.trim_matches(['<', '>']).strip_prefix("https://ograph.dev/shape/")?.strip_suffix("/domain")?.to_owned();
                let (pfx, name) = local.split_once('-')?;
                vocab::PREFIXES.iter().find(|(p, _)| *p == pfx).map(|(_, ns)| format!("{ns}{name}"))
            });
            let on_focus = |a: &Resolved| a.triple.subject.to_string() == r.focus;
            let mut hits: Vec<usize> = accepted
                .iter()
                .enumerate()
                .filter(|(_, a)| on_focus(a) && prop.as_ref().is_none_or(|p| a.triple.predicate.as_str() == p))
                .map(|(k, _)| k)
                .collect();
            // A required value missing rejects the node. A bad value the batch did not propose
            // belongs to another graph, and this batch is not rejected for it.
            if hits.is_empty() && r.value.is_none() {
                hits = accepted.iter().enumerate().filter(|(_, a)| on_focus(a)).map(|(k, _)| k).collect();
            }
            for k in hits {
                drop.insert(k);
                why.entry(k).or_insert_with(|| format!("SHACL {}: {}", r.component, r.message));
            }
        }
        if drop.is_empty() {
            break;
        }
        let mut keep = Vec::with_capacity(accepted.len());
        for (k, a) in accepted.into_iter().enumerate() {
            if drop.contains(&k) {
                let fact = batch.facts.get(a.fact).map_or_else(|| format!("{} (minted type)", a.triple), ProposedFact::describe);
                rejected.push(Rejection { fact, reason: why.remove(&k).unwrap_or_default() });
            } else {
                keep.push(a);
            }
        }
        accepted = keep;
    }
    (accepted.into_iter().map(|r| (r.triple, r.fact)).collect(), rejected, fuzzy)
}

fn s(v: &str) -> Term {
    Literal::new_simple_literal(v).into()
}

/// Checks a batch and commits it: accepted facts replace the batch's graph, rejections replace
/// its quarantine graph. Re-submitting the same batch writes nothing new.
pub fn submit(store: &Store, ont: &Ontology, batch: &Batch, now_ms: i64) -> anyhow::Result<Outcome> {
    submit_with(store, ont, batch, now_ms, true, &mut None)
}

/// [`submit`] with a choice of durability and a catalog shared across batches. A harvest
/// submits non-durably and flushes once, and builds the catalog again only after a batch that
/// can change it.
pub fn submit_with(store: &Store, ont: &Ontology, batch: &Batch, now_ms: i64, _durable: bool, catalog: &mut Option<Catalog>) -> anyhow::Result<Outcome> {
    let graph = batch.graph();
    let qgraph = batch.quarantine_graph();
    let snap = store.snapshot()?;
    let replaced = snap.iri(graph.as_str())?;
    let (accepted, rejected, fuzzy) = if NAMING_SOURCES.contains(&batch.source.as_str()) {
        *catalog = None;
        check(&snap, ont, batch, replaced, &Catalog::from_snapshot(&snap, replaced)?)
    } else {
        let shared = match catalog {
            Some(c) => c,
            None => catalog.insert(Catalog::from_snapshot(&snap, None)?),
        };
        check(&snap, ont, batch, replaced, shared)
    };
    drop(snap);
    let mut triples: Vec<Triple> = accepted.iter().map(|(t, _)| t.clone()).collect();
    let hnode = graph.clone();
    let h = |p: &str, o: Term| Triple::new(hnode.clone(), NamedNode::new_unchecked(p), o);
    triples.push(h(RDF_TYPE, NamedNode::new_unchecked(harv::HARVEST).into()));
    triples.push(h(harv::SOURCE, s(&batch.source)));
    triples.push(h(harv::EXTRACTOR, s(&batch.extractor)));
    triples.push(h(harv::STATUS, s(if batch.degraded.is_empty() { "complete" } else { "degraded" })));
    triples.push(h(harv::AT, Literal::new_typed_literal(term::format_millis(now_ms), xsd::DATE_TIME).into()));
    for d in &batch.degraded {
        triples.push(h(harv::REASON, s(d)));
    }
    for f in &batch.files {
        triples.push(h(harv::FILE, s(f)));
    }
    // Per-fact provenance for model-proposed facts: model, prompt hash and confidence.
    for (t, i) in &accepted {
        let Some(ProposedFact { method: Extraction::Llm { model, prompt_hash, confidence }, evidence, .. }) = batch.facts.get(*i) else { continue };
        let node = NamedNode::new_unchecked(format!("{}fact/{}", vocab::DATA, &hex::encode(sha2_hash(&t.to_string()))[..20]));
        let f = |p: &str, o: Term| Triple::new(node.clone(), NamedNode::new_unchecked(p), o);
        triples.push(f(RDF_TYPE, NamedNode::new_unchecked(harv::LLM_FACT).into()));
        triples.push(f(harv::FACT, s(&t.to_string())));
        triples.push(f(harv::MODEL, s(model)));
        triples.push(f(harv::PROMPT_HASH, s(prompt_hash)));
        triples.push(f(corr::CONFIDENCE, term::decimal(confidence.get()).into()));
        triples.push(f(harv::METHOD, s("llm")));
        for e in evidence {
            triples.push(f(vocab::evt::SOURCE_RECORD, s(&e.0)));
        }
    }
    let mut quarantine = Vec::new();
    for r in &rejected {
        let node = NamedNode::new_unchecked(format!("{}quarantine/{}", vocab::DATA, &hex::encode(sha2_hash(&format!("{}|{}", r.fact, r.reason)))[..20]));
        let f = |p: &str, o: Term| Triple::new(node.clone(), NamedNode::new_unchecked(p), o);
        quarantine.push(f(RDF_TYPE, NamedNode::new_unchecked(harv::QUARANTINED).into()));
        quarantine.push(f(harv::FACT, s(&r.fact)));
        quarantine.push(f(harv::REASON, s(&r.reason)));
        quarantine.push(f(harv::SOURCE, s(&batch.source)));
    }
    let outcome = |removed, added| Outcome {
        graph: graph.as_str().to_owned(),
        proposed: batch.facts.len(),
        accepted: accepted.len(),
        quarantined: rejected.clone(),
        removed,
        added,
        fuzzy_matches: fuzzy,
        degraded: batch.degraded.clone(),
    };
    // Every commit syncs the disk, so a batch that changes nothing but its time commits nothing,
    // and `harv:at` stays the time the graph last changed.
    if !batch.failed() && unchanged(store, &graph, &triples, &qgraph, &quarantine)? {
        return Ok(outcome(0, 0));
    }
    let (removed, added) = store.transact_at(now_ms, |tx| {
        let gid = tx.intern(&graph.clone().into())?;
        let qid = tx.intern(&qgraph.clone().into())?;
        let mut ids = Vec::with_capacity(triples.len());
        for t in &triples {
            ids.push([tx.intern(&t.subject.clone().into())?, tx.intern(&t.predicate.clone().into())?, tx.intern(&t.object)?]);
        }
        if batch.failed() {
            // Keep the last good facts of an unreadable file, such as one caught mid-write.
            let file = tx.intern(&NamedNode::new_unchecked(harv::FILE).into())?;
            ids.extend(tx.graph_quads(gid)?.into_iter().filter(|q| q[0] != gid || q[1] == file).map(|[s, p, o, _]| [s, p, o]));
        }
        let (r1, a1) = tx.replace_graph(gid, ids)?;
        let mut qids = Vec::with_capacity(quarantine.len());
        if batch.failed() {
            qids.extend(tx.graph_quads(qid)?.into_iter().map(|[s, p, o, _]| [s, p, o]));
        }
        for t in &quarantine {
            qids.push([tx.intern(&t.subject.clone().into())?, tx.intern(&t.predicate.clone().into())?, tx.intern(&t.object)?]);
        }
        let (r2, a2) = tx.replace_graph(qid, qids)?;
        Ok((r1 + r2, a1 + a2))
    })?;
    Ok(outcome(removed, added))
}

/// Whether the store already holds exactly these triples in `graph` and `qgraph`, apart from the
/// graph's `harv:at`.
fn unchanged(store: &Store, graph: &NamedNode, triples: &[Triple], qgraph: &NamedNode, quarantine: &[Triple]) -> anyhow::Result<bool> {
    let snap = store.snapshot()?;
    let at = snap.iri(harv::AT)?;
    for (g, ts) in [(graph, triples), (qgraph, quarantine)] {
        let Some(gid) = snap.iri(g.as_str())? else { return Ok(ts.is_empty()) };
        let mut want = FxHashSet::default();
        for t in ts {
            let (Some(s), Some(p), Some(o)) = (snap.id(&t.subject.clone().into())?, snap.id(&t.predicate.clone().into())?, snap.id(&t.object)?) else { return Ok(false) };
            if Some(p) != at {
                want.insert([s, p, o]);
            }
        }
        let mut have = FxHashSet::default();
        for q in snap.quads([None; 3], GraphSel::Named(gid)) {
            let q = q?;
            if Some(q[1]) != at {
                have.insert([q[0], q[1], q[2]]);
            }
        }
        if want != have {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn sha2_hash(s: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes()).into()
}

/// Full SHACL audit of the store, the `ograph validate` command.
pub fn audit(snap: &Snapshot, ont: &Ontology) -> Vec<shacl::ValidationResult> {
    let view = Overlay { snap, ont, add: oxrdf::Graph::new(), replaced: None, ids: RefCell::default() };
    shacl::Validator::new(&ont.shapes, &view).validate_all()
}

#[cfg(test)]
mod tests {
    use super::{sibling, words};

    #[test]
    fn siblings_differ_in_what_they_name() {
        let is = |q: &str, k: &str| sibling(&words(q), &words(k));
        assert!(is("inventory-service-uk", "inventory-service-eu"));
        assert!(is("inventory-service-eu-canary", "inventory-service-eu"));
        assert!(!is("inventory-svc-eu", "inventory-service-eu"));
        assert!(!is("checkout-svc", "checkout-service"));
        assert!(!is("checkout-svc", "checkout"));
    }
}
