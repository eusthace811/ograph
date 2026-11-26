//! The validation gateway. A batch of [`ProposedFact`]s becomes one named graph, and every
//! rejected fact lands in the batch's quarantine graph with its reason. Nothing else writes.
//!
//! Checks, in order: IRI scheme, catalog resolution, ontology membership and catalog-only
//! classes, closed vocabularies and LLM confidence, evidence, then incremental SHACL on the
//! nodes the batch touches (domain, range, cardinality), seen against the store as it will be.

use crate::model::{Batch, EntityRef, Extraction, Object, ProposedFact};
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

pub(crate) fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

pub(crate) fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// `query` names a sibling of the catalog name `known` rather than misspelling it: the same
/// words but one, and that one unlike its counterpart (`inventory-service-uk` against
/// `inventory-service-eu`, `v1` against `v2`, `db1` against `db2`, but not `svc` against `service`), or the same words plus some that are not filler
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
    // Words either of which holds a number differ by it however alike they look (`db1`, `db2`,
    // `db`), and otherwise by their letters alone (`db1` and `db01`, `1` and `01`, are one).
    let digits = |w: &str| {
        let all: String = w.chars().filter(char::is_ascii_digit).collect();
        let n = all.trim_start_matches('0');
        if n.is_empty() && !all.is_empty() { "0".to_owned() } else { n.to_owned() }
    };
    let letters = |w: &str| w.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
    let has_digit = |w: &str| w.chars().any(|c| c.is_ascii_digit());
    let unlike = |x: &str, y: &str| match has_digit(x) || has_digit(y) {
        true => digits(x) != digits(y) || (letters(x) != letters(y) && strsim::jaro_winkler(&letters(x), &letters(y)) < SIBLING_WORD),
        false => strsim::jaro_winkler(x, y) < SIBLING_WORD,
    };
    matches!((diff.next(), diff.next()), (Some((x, y)), None) if unlike(x, y) && !(filler(x) && filler(y)))
}

/// Words that name the kind of thing rather than which one.
pub(crate) const FILLER: [&str; 6] = ["svc", "service", "srv", "server", "app", "api"];

const CATALOG_CLASSES: [&str; 2] = [ops::APPLICATION, ops::TEAM];

/// The sources whose batches write catalog entities, names or aliases. Every other batch of a
/// harvest checks against one catalog built once.
pub const NAMING_SOURCES: [&str; 3] = ["catalog", "ownership", "docs"];

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
                            c.insert_name(class, &n, l.value());
                        }
                    }
                }
                let local = n.as_str().rsplit('/').next().unwrap_or_default().to_owned();
                c.insert_name(class, &n, &local);
            }
        }
        Ok(c)
    }

    pub fn add(&mut self, class: &'static str, iri: &NamedNode, names: &[&str]) {
        self.misses.get_mut().clear();
        self.known.insert(iri.as_str().to_owned());
        for n in names {
            self.insert_name(class, iri, n);
        }
    }

    /// A name of no words, such as `--`, names nothing: under the empty key it would be a
    /// prefix of every name.
    fn insert_name(&mut self, class: &'static str, iri: &NamedNode, name: &str) {
        let key = norm(name);
        if !key.is_empty() {
            self.names.insert((class, key), (iri.clone(), words(name)));
        }
    }

    pub fn contains(&self, iri: &str) -> bool {
        self.known.contains(iri)
    }

    /// The entity whose name `name` extends with words that are not filler, such as
    /// `legacy-checkout` for `checkout`: a sibling however far its score falls.
    pub fn extended(&self, class: &'static str, name: &str) -> Option<&NamedNode> {
        let w = words(name);
        let mut hits: Vec<&NamedNode> = self.names.iter().filter(|((c, _), (_, k))| *c == class && k.len() < w.len() && sibling(&w, k)).map(|(_, (n, _))| n).collect();
        hits.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        hits.first().copied()
    }

    /// The entity `name` is a name or alias of, exactly, with no fuzzy matching.
    pub(crate) fn exact(&self, class: &'static str, name: &str) -> Option<&NamedNode> {
        self.names.get(&(class, norm(name))).map(|(n, _)| n)
    }

    pub fn resolve(&self, class: &'static str, name: &str) -> Resolution {
        let key = norm(name);
        if key.is_empty() {
            return Resolution::Missing;
        }
        if let Some(n) = self.exact(class, name) {
            return Resolution::Exact(n.clone());
        }
        if let Some(r) = self.misses.borrow().get(&(class, name.to_owned())) {
            return r.clone();
        }
        let r = self.explain_fuzzy(class, name, &key).resolution;
        self.misses.borrow_mut().insert((class, name.to_owned()), r.clone());
        r
    }

    /// [`Catalog::resolve`] with its reason and the best-scoring entities, for `ograph resolve`.
    pub fn explain(&self, class: &'static str, name: &str) -> Match {
        let key = norm(name);
        if key.is_empty() {
            return Match { resolution: Resolution::Missing, why: "an empty name".into(), candidates: vec![] };
        }
        if let Some(n) = self.exact(class, name) {
            return Match { resolution: Resolution::Exact(n.clone()), why: "exact name or alias".into(), candidates: vec![(n.clone(), 1.0)] };
        }
        self.explain_fuzzy(class, name, &key)
    }

    fn explain_fuzzy(&self, class: &'static str, name: &str, key: &str) -> Match {
        let mut scored: Vec<(f64, &NamedNode, &[String])> =
            self.names.iter().filter(|((c, _), _)| *c == class).map(|((_, k), (n, w))| (strsim::jaro_winkler(key, k), n, w.as_slice())).collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.as_str().cmp(b.1.as_str())).then_with(|| a.2.cmp(b.2)));
        let mut candidates: Vec<(NamedNode, f64)> = Vec::new();
        for (s, n, _) in &scored {
            if candidates.len() == 3 {
                break;
            }
            if !candidates.iter().any(|(c, _)| c == *n) {
                candidates.push(((*n).clone(), *s));
            }
        }
        let Some(&(s, n, w)) = scored.first() else { return Match { resolution: Resolution::Missing, why: "the catalog is empty".into(), candidates } };
        let runner = scored.iter().find(|x| x.1 != n);
        let runner_up = runner.map_or(0.0, |x| x.0);
        let best = vocab::compact(n.as_str());
        let (resolution, why) = if s < FUZZY_QUARANTINE {
            (Resolution::Missing, format!("nothing close: the best, {best}, scores {s:.3}, under {FUZZY_QUARANTINE}"))
        } else if s < FUZZY_ACCEPT {
            (Resolution::Ambiguous(n.clone(), s), format!("only fuzzily matches {best} ({s:.3}, under {FUZZY_ACCEPT})"))
        } else if s - runner_up < FUZZY_MARGIN {
            let other = runner.map(|x| vocab::compact(x.1.as_str())).unwrap_or_default();
            (Resolution::Ambiguous(n.clone(), s), format!("{best} ({s:.3}) and {other} ({runner_up:.3}) are within {FUZZY_MARGIN}"))
        } else if sibling(&words(name), w) {
            (Resolution::Ambiguous(n.clone(), s), format!("close to {best} ({s:.3}) but names a sibling of it"))
        } else {
            (Resolution::Fuzzy(n.clone(), s), format!("fuzzy match on {best} ({s:.3}), {:.3} ahead of any other", s - runner_up))
        };
        Match { resolution, why, candidates }
    }
}

/// A catalog application as a prompt names it: its first name, then its other names and aliases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownApp {
    pub name: String,
    pub aliases: Vec<String>,
}

/// Every application in `snap`, by name.
pub fn applications(snap: &Snapshot) -> crate::store::Result<Vec<KnownApp>> {
    let (Some(ty), Some(class)) = (snap.iri(RDF_TYPE)?, snap.iri(ops::APPLICATION)?) else { return Ok(vec![]) };
    let literals = |e: Id, p: Option<Id>| -> crate::store::Result<Vec<String>> {
        let mut out = Vec::new();
        for v in p.map(|p| snap.objects(e, p)).transpose()?.unwrap_or_default() {
            if let Term::Literal(l) = snap.term(v)? {
                out.push(l.value().to_owned());
            }
        }
        out.sort();
        Ok(out)
    };
    let (name, alias) = (snap.iri(ops::NAME)?, snap.iri(ops::ALIAS)?);
    let mut apps = Vec::new();
    for e in snap.subjects(ty, class)? {
        let Term::NamedNode(n) = snap.term(e)? else { continue };
        let mut names = literals(e, name)?;
        if names.is_empty() {
            names.push(n.as_str().rsplit('/').next().unwrap_or_default().to_owned());
        }
        let first = names.remove(0);
        names.extend(literals(e, alias)?);
        names.retain(|x| norm(x) != norm(&first));
        names.sort();
        names.dedup();
        apps.push(KnownApp { name: first, aliases: names });
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(apps)
}

/// How a name resolved against the catalog, why, and the best-scoring entities (at most three).
pub struct Match {
    pub resolution: Resolution,
    pub why: String,
    pub candidates: Vec<(NamedNode, f64)>,
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
    /// The ontology holds the class hierarchy; the store holds none of it. A superclass the
    /// ontology names without declaring still has the subclasses that name it.
    fn subclasses(&self, class: &Term) -> Vec<Term> {
        let Term::NamedNode(n) = class else { return vec![class.clone()] };
        let mut out: Vec<Term> = self.ont.classes.iter().filter(|(c, sups)| *c != n.as_str() && sups.contains(n.as_str())).map(|(c, _)| NamedNode::new_unchecked(c).into()).collect();
        out.insert(0, class.clone());
        out
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
    /// The records that proposed it, kept on its quarantine entry as `evt:sourceRecord`.
    #[serde(skip)]
    pub records: Vec<String>,
}

impl Rejection {
    fn of(f: &ProposedFact, reason: String) -> Self {
        Self { fact: f.describe(), reason, records: f.evidence.iter().map(|e| e.0.clone()).collect() }
    }
}

struct Resolved {
    triple: Triple,
    fact: usize,
}

/// Runs the checks against `catalog` without writing. Returns the accepted triples and the
/// rejections.
pub fn check(snap: &Snapshot, ont: &Ontology, batch: &Batch, replaced: Option<Id>, catalog: &Catalog) -> (Vec<(Triple, usize)>, Vec<Rejection>, usize) {
    // Who holds each name: the catalog before this batch, and the batch's own entities by their
    // names (not their aliases, which the claims pass below settles).
    let mut holders = std::borrow::Cow::Borrowed(catalog);
    let mut catalog = std::borrow::Cow::Borrowed(catalog);
    let ty = NamedNode::new_unchecked(RDF_TYPE);
    let name_p = NamedNode::new_unchecked(ops::NAME);
    let alias_p = NamedNode::new_unchecked(ops::ALIAS);
    // Catalog facts in this batch define catalog entities before anything else resolves.
    let mut names_of: FxHashMap<&EntityRef, Vec<&str>> = FxHashMap::default();
    let mut own_names: FxHashMap<&EntityRef, Vec<&str>> = FxHashMap::default();
    for g in batch.facts.iter().filter(|g| g.predicate == name_p || g.predicate == alias_p) {
        if let Object::Literal(l) = &g.object {
            names_of.entry(&g.subject).or_default().push(l.value());
            if g.predicate == name_p {
                own_names.entry(&g.subject).or_default().push(l.value());
            }
        }
    }
    for f in &batch.facts {
        if !matches!(f.method, Extraction::Catalog { .. }) || f.predicate != ty {
            continue;
        }
        if let (EntityRef::Known(s), Object::Entity(EntityRef::Known(c))) = (&f.subject, &f.object)
            && let Some(class) = CATALOG_CLASSES.iter().find(|k| **k == c.as_str()) {
                catalog.to_mut().add(class, s, names_of.get(&f.subject).map_or(&[][..], Vec::as_slice));
                let mut names = own_names.get(&f.subject).cloned().unwrap_or_default();
                names.push(s.as_str().rsplit('/').next().unwrap_or_default());
                holders.to_mut().add(class, s, &names);
            }
    }
    let mut rejected: Vec<Rejection> = batch.withheld.iter().map(|(f, reason)| Rejection::of(f, reason.clone())).collect();
    let mut fuzzy = 0;
    let mut accepted: Vec<Resolved> = Vec::new();
    let resolve = |r: &EntityRef, catalog_method: bool, fuzzy: &mut usize| -> Result<NamedNode, String> {
        match r {
            EntityRef::Known(n) => {
                if !n.as_str().starts_with(vocab::DATA) && n.as_str().starts_with("https://ograph.dev/") && !ont.is_class(n.as_str()) {
                    return Err(format!("{} is not an ontology class", vocab::compact(n.as_str())));
                }
                if !iri_scheme().is_match(n.as_str()) && !ont.is_class(n.as_str()) {
                    return Err(format!("IRI {n} does not follow the id scheme"));
                }
                let is_catalog_kind = n.as_str().strip_prefix(vocab::DATA).is_some_and(|r| r.starts_with("app/") || r.starts_with("team/"));
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
                } else {
                    Err(format!("\"{name}\" names a {}, which only the catalog resolves by name", vocab::compact(class)))
                }
            }
        }
    };
    for (i, f) in batch.facts.iter().enumerate() {
        let reject = |reason: String| Rejection::of(f, reason);
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
        let s = match resolve(&f.subject, catalog_method, &mut fuzzy) {
            Ok(s) => s,
            Err(e) => {
                rejected.push(reject(e));
                continue;
            }
        };
        let o: Term = match &f.object {
            Object::Literal(l) => l.clone().into(),
            Object::Entity(r) => match resolve(r, catalog_method, &mut fuzzy) {
                Ok(n) => n.into(),
                Err(e) => {
                    rejected.push(reject(e));
                    continue;
                }
            },
        };
        if let Some(r) = ontology_problem(ont, &Triple::new(s.clone(), f.predicate.clone(), o.clone())) {
            rejected.push(reject(r));
            continue;
        }
        if p == RDF_TYPE
            && let Term::NamedNode(c) = &o
            && CATALOG_CLASSES.contains(&c.as_str())
            && !catalog_method
        {
            rejected.push(reject(format!("only the catalog may create a {}", vocab::compact(c.as_str()))));
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
        accepted.push(Resolved { triple: Triple::new(s, f.predicate.clone(), o), fact: i });
    }
    // Two applications claiming one alias in the same batch: neither gets it, whatever the order.
    // One claiming an alias another application held before the batch does not get it either,
    // unless the holder claims it again here (then both claim it).
    // Names are per class: an application and a team may share one.
    let class_of = |a: &Resolved| {
        let local = a.triple.subject.to_string();
        let local = local.trim_matches(['<', '>']).strip_prefix(vocab::DATA).unwrap_or_default();
        CATALOG_CLASSES.into_iter().zip(["app/", "team/"]).find(|(_, k)| local.starts_with(k)).map(|(c, _)| c)
    };
    let alias_of = |a: &Resolved| match &a.triple.object {
        Term::Literal(l) if a.triple.predicate.as_str() == ops::ALIAS => Some((class_of(a), norm(l.value()))),
        _ => None,
    };
    let mut claims: FxHashMap<(Option<&str>, String), std::collections::BTreeSet<String>> = FxHashMap::default();
    for a in &accepted {
        if let Some(k) = alias_of(a) {
            claims.entry(k).or_default().insert(vocab::compact(a.triple.subject.to_string().trim_matches(['<', '>'])));
        }
    }
    accepted.retain(|a| {
        let (Some(c), Term::Literal(l)) = (alias_of(a).and_then(|k| claims.get(&k)), &a.triple.object) else { return true };
        let why = if c.len() > 1 {
            format!("alias is claimed by {} in one batch", c.iter().map(String::as_str).collect::<Vec<_>>().join(" and "))
        } else if let Some(class) = class_of(a)
            && let Some(other) = holders.exact(class, l.value())
            && !c.contains(&vocab::compact(other.as_str()))
        {
            format!("alias \"{}\" already names {}", l.value(), vocab::compact(other.as_str()))
        } else {
            return true;
        };
        rejected.push(Rejection::of(&batch.facts[a.fact], why));
        false
    });
    let (kept, dropped) = shacl_pass(snap, ont, replaced, accepted, |r| &r.triple);
    for (a, reason) in dropped {
        rejected.push(Rejection::of(&batch.facts[a.fact], reason));
    }
    let accepted = kept;
    (accepted.into_iter().map(|r| (r.triple, r.fact)).collect(), rejected, fuzzy)
}

/// Incremental SHACL on the nodes `accepted` touches, seen as the store will be with them in
/// place of graph `replaced`, repeated until no violation remains, because removing a fact can
/// break another shape. Each pass drops a fact or stops. Returns what is kept and what is
/// dropped, with its reason.
fn shacl_pass<T>(snap: &Snapshot, ont: &Ontology, replaced: Option<Id>, mut accepted: Vec<T>, triple: impl Fn(&T) -> &Triple) -> (Vec<T>, Vec<(T, String)>) {
    let mut dropped = Vec::new();
    let mut overlay = Overlay { snap, ont, add: oxrdf::Graph::new(), replaced, ids: RefCell::default() };
    loop {
        overlay.add = accepted.iter().map(|r| triple(r).clone()).collect();
        let focus: Vec<Term> = {
            let mut f: Vec<Term> = accepted.iter().map(|r| Term::from(triple(r).subject.clone())).collect();
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
        let subjects: Vec<String> = accepted.iter().map(|a| triple(a).subject.to_string()).collect();
        for r in &results {
            let prop = order_end(ont, r).or_else(|| r.path.as_deref().map(|p| p.trim_matches(['<', '>']).to_owned())).or_else(|| domain_of(ont, r));
            let on_focus = |k: usize| subjects[k] == r.focus;
            let mut hits: Vec<usize> = accepted
                .iter()
                .enumerate()
                .filter(|&(k, a)| on_focus(k) && prop.as_ref().is_none_or(|p| triple(a).predicate.as_str() == p))
                .map(|(k, _)| k)
                .collect();
            // A required value missing rejects the node. A bad value the batch did not propose
            // belongs to another graph, and this batch is not rejected for it.
            if hits.is_empty() && r.value.is_none() {
                hits = (0..accepted.len()).filter(|&k| on_focus(k)).collect();
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
                dropped.push((a, why.remove(&k).unwrap_or_default()));
            } else {
                keep.push(a);
            }
        }
        accepted = keep;
    }
    (accepted, dropped)
}

/// For a time-order violation, the property that must come later. An end time before its start
/// is quarantined alone: the start places the node on the timeline and is required anyway.
fn order_end(ont: &Ontology, r: &shacl::ValidationResult) -> Option<String> {
    if !r.component.starts_with("LessThan") {
        return None;
    }
    let shape = ont.shapes.shapes.iter().find(|s| s.id.to_string() == r.shape)?;
    shape.constraints.iter().find_map(|c| match c {
        shacl::Constraint::LessThan(p) | shacl::Constraint::LessThanOrEquals(p) => Some(p.as_str().to_owned()),
        _ => None,
    })
}

/// The property of a derived domain shape: its subjects are the focus, so only that property's
/// facts on it are wrong.
fn domain_of(ont: &Ontology, r: &shacl::ValidationResult) -> Option<String> {
    if !r.shape.starts_with("<https://ograph.dev/shape/") || !r.shape.ends_with("/domain>") {
        return None;
    }
    let shape = ont.shapes.shapes.iter().find(|s| s.id.to_string() == r.shape)?;
    shape.targets.iter().find_map(|t| match t {
        shacl::Target::SubjectsOf(p) => Some(p.as_str().to_owned()),
        _ => None,
    })
}

fn s(v: &str) -> Term {
    Literal::new_simple_literal(v).into()
}

/// Checks a batch and commits it: accepted facts replace the batch's graph, rejections replace
/// its quarantine graph. Re-submitting the same batch writes nothing new.
pub fn submit(store: &Store, ont: &Ontology, batch: &Batch, now_ms: i64) -> anyhow::Result<Outcome> {
    submit_with(store, ont, batch, now_ms, &mut None)
}

/// [`submit`] with a catalog shared across batches. A harvest builds the catalog again only
/// after a batch that can change it.
pub fn submit_with(store: &Store, ont: &Ontology, batch: &Batch, now_ms: i64, catalog: &mut Option<Catalog>) -> anyhow::Result<Outcome> {
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
        let node = fact_node(t);
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
    triples.extend(record_provenance(batch, &accepted));
    let mut quarantine = Vec::new();
    for r in &rejected {
        let node = NamedNode::new_unchecked(format!("{}quarantine/{}", vocab::DATA, &hex::encode(sha2_hash(&format!("{}|{}", r.fact, r.reason)))[..20]));
        let f = |p: &str, o: Term| Triple::new(node.clone(), NamedNode::new_unchecked(p), o);
        quarantine.push(f(RDF_TYPE, NamedNode::new_unchecked(harv::QUARANTINED).into()));
        quarantine.push(f(harv::FACT, s(&r.fact)));
        quarantine.push(f(harv::REASON, s(&r.reason)));
        quarantine.push(f(harv::SOURCE, s(&batch.source)));
        for e in &r.records {
            quarantine.push(f(vocab::evt::SOURCE_RECORD, s(e)));
        }
    }
    let outcome = |removed, added| Outcome {
        graph: graph.as_str().to_owned(),
        proposed: batch.facts.len() + batch.withheld.len(),
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

/// In a graph of several files, the records behind each fact about a node that more than one
/// record states: `harv:fact`, `harv:method "rule"` and an `evt:sourceRecord` per record, on the
/// fact's node. Without them a file that sets one field of an incident another file also states
/// could not be told from one that sets none. Model facts carry their own provenance.
fn record_provenance(batch: &Batch, accepted: &[(Triple, usize)]) -> Vec<Triple> {
    if batch.files.is_empty() {
        return vec![];
    }
    let mut by_triple: std::collections::BTreeMap<String, (&Triple, std::collections::BTreeSet<&str>, bool)> = Default::default();
    for (t, i) in accepted {
        let Some(f) = batch.facts.get(*i) else { continue };
        let e = by_triple.entry(t.to_string()).or_insert_with(|| (t, Default::default(), false));
        e.1.extend(f.evidence.iter().map(|r| r.0.as_str()));
        e.2 |= matches!(f.method, Extraction::Llm { .. });
    }
    let mut records: FxHashMap<&oxrdf::NamedOrBlankNode, std::collections::BTreeSet<&str>> = FxHashMap::default();
    for (t, rs, _) in by_triple.values() {
        records.entry(&t.subject).or_default().extend(rs);
    }
    let mut out = Vec::new();
    for (text, (t, rs, llm)) in &by_triple {
        if *llm || records[&t.subject].len() < 2 {
            continue;
        }
        let node = fact_node(t);
        let f = |p: &str, o: Term| Triple::new(node.clone(), NamedNode::new_unchecked(p), o);
        out.push(f(harv::FACT, s(text)));
        out.push(f(harv::METHOD, s("rule")));
        out.extend(rs.iter().map(|r| f(vocab::evt::SOURCE_RECORD, s(r))));
    }
    out
}

/// The node that holds a model-proposed fact's provenance.
pub fn fact_node(t: &Triple) -> NamedNode {
    NamedNode::new_unchecked(format!("{}fact/{}", vocab::DATA, &hex::encode(sha2_hash(&t.to_string()))[..20]))
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

/// Facts the store holds now that a harvest under another ontology would quarantine, for one
/// harvest graph.
#[derive(Debug, serde::Serialize)]
pub struct Impact {
    pub graph: String,
    pub facts: usize,
    /// Each reason and how many facts it rejects, most frequent first.
    pub reasons: Vec<(String, usize)>,
}

/// The stored facts `newer` rejects and `current` keeps, per graph.
#[derive(Debug, Default, serde::Serialize)]
pub struct Impacts {
    /// What a harvest under `newer` would quarantine.
    pub quarantine: Vec<Impact>,
    /// Verdict facts no harvest rewrites. A new `ograph verdict` on the chain re-adds them, so
    /// the gateway would refuse it.
    pub verdicts: Vec<Impact>,
}

/// What `newer` rejects that `current` keeps, per harvest graph. The stored facts of each graph
/// go through the checks that depend on the ontology (membership, closed vocabularies, SHACL)
/// against the rest of the store, as a re-harvest of that graph would see them; the checks that
/// do not (catalog, evidence, confidence) passed when the facts were stored.
pub fn impact(snap: &Snapshot, current: &Ontology, newer: &Ontology) -> anyhow::Result<Impacts> {
    let harvest = format!("{}harvest/", vocab::GRAPH);
    let verdicts = format!("{harvest}verdicts/");
    let mut out = Impacts::default();
    for (gid, _) in snap.graphs()? {
        let Term::NamedNode(g) = snap.term(gid)? else { continue };
        if !g.as_str().starts_with(&harvest) {
            continue;
        }
        let triples = stored(snap, gid)?;
        let before: FxHashSet<Triple> = recheck(snap, current, gid, &triples).into_iter().map(|(t, _)| t).collect();
        let mut reasons: FxHashMap<String, usize> = FxHashMap::default();
        let mut facts = 0;
        for (t, r) in recheck(snap, newer, gid, &triples) {
            if !before.contains(&t) {
                facts += 1;
                *reasons.entry(r).or_default() += 1;
            }
        }
        if facts > 0 {
            let mut reasons: Vec<(String, usize)> = reasons.into_iter().collect();
            reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let into = if g.as_str().starts_with(&verdicts) { &mut out.verdicts } else { &mut out.quarantine };
            into.push(Impact { graph: vocab::compact(g.as_str()), facts, reasons });
        }
    }
    Ok(out)
}

/// The facts a graph holds as its batch proposed them: the gateway writes a graph's own record
/// and the provenance of model facts after its checks.
fn stored(snap: &Snapshot, gid: Id) -> anyhow::Result<Vec<Triple>> {
    let mut triples = Vec::new();
    for q in snap.quads([None; 3], GraphSel::Named(gid)) {
        let q = q?;
        if let (Some(s), Term::NamedNode(p)) = (shacl::as_subject(&snap.term(q[0])?), snap.term(q[1])?) {
            triples.push(Triple::new(s, p, snap.term(q[2])?));
        }
    }
    let bookkeeping: FxHashSet<oxrdf::NamedOrBlankNode> = triples
        .iter()
        .filter(|t| t.predicate.as_str() == harv::FACT || t.predicate.as_str() == RDF_TYPE && matches!(&t.object, Term::NamedNode(c) if c.as_str() == harv::HARVEST || c.as_str() == harv::LLM_FACT))
        .map(|t| t.subject.clone())
        .collect();
    triples.retain(|t| !bookkeeping.contains(&t.subject));
    Ok(triples)
}

/// The facts of `graph` that a harvest of its source over `snap` would quarantine, with the
/// reason, by the checks that depend on the rest of the store.
pub fn rejected_again(snap: &Snapshot, ont: &Ontology, graph: Id) -> anyhow::Result<Vec<(Triple, String)>> {
    Ok(recheck(snap, ont, graph, &stored(snap, graph)?))
}

fn recheck(snap: &Snapshot, ont: &Ontology, graph: Id, triples: &[Triple]) -> Vec<(Triple, String)> {
    let mut rejected = Vec::new();
    let mut kept = Vec::new();
    for t in triples {
        match ontology_problem(ont, t) {
            Some(r) => rejected.push((t.clone(), r)),
            None => kept.push(t.clone()),
        }
    }
    fn itself(t: &Triple) -> &Triple {
        t
    }
    rejected.extend(shacl_pass(snap, ont, Some(graph), kept, itself).1);
    rejected
}

/// The membership and vocabulary checks on a resolved triple, for a batch and for stored facts.
fn ontology_problem(ont: &Ontology, t: &Triple) -> Option<String> {
    let p = t.predicate.as_str();
    if p != RDF_TYPE && p != RDFS_LABEL && ont.property(p).is_none() {
        return Some(format!("{} is not an ontology property", vocab::compact(p)));
    }
    let subject = match &t.subject {
        oxrdf::NamedOrBlankNode::NamedNode(n) => Some(n),
        _ => None,
    };
    let object = match &t.object {
        Term::NamedNode(n) => Some(n),
        _ => None,
    };
    if p == RDF_TYPE
        && let Some(c) = object
        && !ont.is_class(c.as_str())
    {
        return Some(format!("{} is not an ontology class", vocab::compact(c.as_str())));
    }
    for n in subject.into_iter().chain(object) {
        if !n.as_str().starts_with(vocab::DATA) && n.as_str().starts_with("https://ograph.dev/") && !ont.is_class(n.as_str()) {
            return Some(format!("{} is not an ontology class", vocab::compact(n.as_str())));
        }
    }
    if let (Some(v), Term::Literal(l)) = (ont.vocabulary(p), &t.object)
        && !v.iter().any(|x| x.value == l.value()) {
            return Some(format!("\"{}\" is not in the closed vocabulary of {}", l.value(), vocab::compact(p)));
        }
    None
}

/// Full SHACL audit of the store, the `ograph validate` command.
pub fn audit(snap: &Snapshot, ont: &Ontology) -> Vec<shacl::ValidationResult> {
    let view = Overlay { snap, ont, add: oxrdf::Graph::new(), replaced: None, ids: RefCell::default() };
    shacl::Validator::new(&ont.shapes, &view).validate_all()
}

/// How many focus nodes each shape of shapes.ttl that has a target selects in the store. A
/// shape that selects none checks nothing, whether by a misspelled target or by data that
/// never came.
pub fn shape_focus(snap: &Snapshot, ont: &Ontology) -> Vec<(String, usize)> {
    let view = Overlay { snap, ont, add: oxrdf::Graph::new(), replaced: None, ids: RefCell::default() };
    let v = shacl::Validator::new(&ont.shapes, &view);
    let mut out: Vec<(String, usize)> = ont
        .handwritten_shapes()
        .iter()
        .filter(|s| !s.targets.is_empty() && !s.deactivated)
        .map(|s| (crate::ontology::shape_name(&s.id), v.focus_nodes(s).len()))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::{sibling, words};
    use crate::model::{Batch, Extraction, known, lit, obj};
    use crate::ontology::{DEFAULT_SHAPES, DEFAULT_TTL, Ontology};
    use crate::store::Store;

    #[test]
    fn two_applications_of_one_catalog_batch_claiming_one_alias_get_neither() {
        let path = std::env::temp_dir().join(format!("ograph-gateway-alias-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let mut b = Batch::new("catalog", "k", "rule");
        let (ev, m) = (b.record("r1"), Extraction::Catalog { catalog: "test".into() });
        for app in ["ledger-db", "checkout"] {
            b.add(known("app", app), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked(crate::vocab::ops::APPLICATION)), &ev, &m);
            b.add(known("app", app), crate::vocab::ops::NAME, lit(app), &ev, &m);
            b.add(known("app", app), crate::vocab::ops::ALIAS, lit("shared"), &ev, &m);
        }
        let out = super::submit(&store, &Ontology::load_default().unwrap(), &b, 0).unwrap();
        let _ = std::fs::remove_file(&path);
        let reasons: Vec<&str> = out.quarantined.iter().map(|r| r.reason.as_str()).collect();
        assert_eq!(reasons, ["alias is claimed by id:app/checkout and id:app/ledger-db in one batch"; 2]);
    }

    #[test]
    fn an_alias_another_catalog_file_holds_is_not_taken() {
        let path = std::env::temp_dir().join(format!("ograph-gateway-alias-files-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let ont = Ontology::load_default().unwrap();
        let mut outs = vec![];
        for (key, app) in [("a", "ledger-db"), ("b", "checkout")] {
            let mut b = Batch::new("catalog", key, "rule");
            let (ev, m) = (b.record("r1"), Extraction::Catalog { catalog: "test".into() });
            b.add(known("app", app), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked(crate::vocab::ops::APPLICATION)), &ev, &m);
            b.add(known("app", app), crate::vocab::ops::NAME, lit(app), &ev, &m);
            b.add(known("app", app), crate::vocab::ops::ALIAS, lit("shared"), &ev, &m);
            outs.push(super::submit(&store, &ont, &b, 0).unwrap());
        }
        let _ = std::fs::remove_file(&path);
        assert!(outs[0].quarantined.is_empty());
        let reasons: Vec<&str> = outs[1].quarantined.iter().map(|r| r.reason.as_str()).collect();
        assert_eq!(reasons, ["alias \"shared\" already names id:app/ledger-db"]);
    }

    #[test]
    fn an_alias_held_elsewhere_is_not_taken_when_the_holders_own_claim_fails() {
        let path = std::env::temp_dir().join(format!("ograph-gateway-alias-held-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let ont = Ontology::load_default().unwrap();
        let m = Extraction::Catalog { catalog: "test".into() };
        let app = |b: &mut Batch, ev: &crate::model::EvidenceRef, name: &str| {
            b.add(known("app", name), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked(crate::vocab::ops::APPLICATION)), ev, &m);
            b.add(known("app", name), crate::vocab::ops::NAME, lit(name), ev, &m);
        };
        let mut a = Batch::new("catalog", "a", "rule");
        let ev = a.record("r1");
        app(&mut a, &ev, "ledger-db");
        a.add(known("app", "ledger-db"), crate::vocab::ops::ALIAS, lit("shared"), &ev, &m);
        super::submit(&store, &ont, &a, 0).unwrap();
        let mut b = Batch::new("catalog", "b", "rule");
        let ev = b.record("r1");
        app(&mut b, &ev, "checkout");
        b.add(known("app", "checkout"), crate::vocab::ops::ALIAS, lit("shared"), &ev, &m);
        // A claim by the holder that fails on its own: its evidence is no record this batch read.
        b.add(known("app", "ledger-db"), crate::vocab::ops::ALIAS, lit("shared"), &crate::model::EvidenceRef("nope".into()), &m);
        let out = super::submit(&store, &ont, &b, 0).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(out.quarantined.iter().any(|r| r.reason == "alias \"shared\" already names id:app/ledger-db"), "{:?}", out.quarantined);
    }

    #[test]
    fn an_alias_may_not_take_the_name_of_another_application_in_its_batch() {
        for order in [["checkout", "ledger-db"], ["ledger-db", "checkout"]] {
            let path = std::env::temp_dir().join(format!("ograph-gateway-alias-name-{}-{}.redb", std::process::id(), order[0]));
            let _ = std::fs::remove_file(&path);
            let store = Store::open(&path).unwrap();
            let mut b = Batch::new("catalog", "k", "rule");
            let (ev, m) = (b.record("r1"), Extraction::Catalog { catalog: "test".into() });
            for app in order {
                b.add(known("app", app), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked(crate::vocab::ops::APPLICATION)), &ev, &m);
                b.add(known("app", app), crate::vocab::ops::NAME, lit(app), &ev, &m);
                if app == "ledger-db" {
                    b.add(known("app", app), crate::vocab::ops::ALIAS, lit("checkout"), &ev, &m);
                }
            }
            let out = super::submit(&store, &Ontology::load_default().unwrap(), &b, 0).unwrap();
            let _ = std::fs::remove_file(&path);
            let reasons: Vec<&str> = out.quarantined.iter().map(|r| r.reason.as_str()).collect();
            assert_eq!(reasons, ["alias \"checkout\" already names id:app/checkout"], "{order:?}");
        }
    }

    #[test]
    fn a_team_alias_is_checked_against_teams_not_applications() {
        let path = std::env::temp_dir().join(format!("ograph-gateway-team-alias-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let mut b = Batch::new("catalog", "k", "rule");
        let (ev, m) = (b.record("r1"), Extraction::Catalog { catalog: "test".into() });
        b.add(known("app", "payments"), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked(crate::vocab::ops::APPLICATION)), &ev, &m);
        b.add(known("app", "payments"), crate::vocab::ops::NAME, lit("payments"), &ev, &m);
        b.add(known("team", "payments-team"), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked(crate::vocab::ops::TEAM)), &ev, &m);
        b.add(known("team", "payments-team"), crate::vocab::ops::ALIAS, lit("payments"), &ev, &m);
        let out = super::submit(&store, &Ontology::load_default().unwrap(), &b, 0).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(out.quarantined.is_empty(), "{:?}", out.quarantined);
    }

    #[test]
    fn a_name_of_no_words_names_nothing() {
        let path = std::env::temp_dir().join(format!("ograph-gateway-empty-name-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let mut c = super::Catalog::from_snapshot(&store.snapshot().unwrap(), None).unwrap();
        let _ = std::fs::remove_file(&path);
        c.add(crate::vocab::ops::APPLICATION, &crate::model::entity("app", "ledger-db"), &["ledger-db", "--"]);
        assert_eq!(c.extended(crate::vocab::ops::APPLICATION, "payments-api"), None);
        assert!(c.extended(crate::vocab::ops::APPLICATION, "ledger-db-replica").is_some());
    }

    #[test]
    fn a_target_class_selects_instances_of_its_subclasses_in_the_store() {
        let path = std::env::temp_dir().join(format!("ograph-gateway-subclass-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let mut b = Batch::new("test", "k", "rule");
        let (ev, m) = (b.record("r1"), Extraction::Rule { extractor: "rule".into() });
        b.add(known("vendor", "acme"), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked("https://ograph.dev/ops#Vendor")), &ev, &m);
        super::submit(&store, &Ontology::load_default().unwrap(), &b, 0).unwrap();
        let shapes = format!("{DEFAULT_SHAPES}\n<https://example.org/things> a <http://www.w3.org/ns/shacl#NodeShape> ; <http://www.w3.org/ns/shacl#targetClass> <https://ograph.dev/meta#Thing> .\n");
        let ont = Ontology::load(DEFAULT_TTL, &shapes).unwrap();
        let focus = super::shape_focus(&store.snapshot().unwrap(), &ont);
        let _ = std::fs::remove_file(&path);
        assert!(focus.iter().any(|(name, n)| name.contains("things") && *n > 0), "{focus:?}");
    }

    #[test]
    fn a_domain_violation_outside_the_known_prefixes_rejects_only_that_fact() {
        let ttl = format!("{DEFAULT_TTL}\n<https://example.org/x#foo> a owl:DatatypeProperty ; rdfs:domain ops:Application .\n");
        let ont = Ontology::load(&ttl, DEFAULT_SHAPES).unwrap();
        let path = std::env::temp_dir().join(format!("ograph-gateway-domain-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let mut b = Batch::new("test", "k", "rule");
        let (ev, m) = (b.record("r1"), Extraction::Rule { extractor: "rule".into() });
        let acme = known("vendor", "acme");
        b.add(acme.clone(), crate::vocab::RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked("https://ograph.dev/ops#Vendor")), &ev, &m);
        b.add(acme.clone(), crate::vocab::RDFS_LABEL, lit("Acme"), &ev, &m);
        b.add(acme, "https://example.org/x#foo", lit("x"), &ev, &m);
        let out = super::submit(&store, &ont, &b, 0).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(out.quarantined.iter().map(|r| r.fact.contains("example.org/x#foo")).collect::<Vec<_>>(), [true], "{:?}", out.quarantined);
        assert_eq!(out.accepted, 2);
    }

    #[test]
    fn a_name_outside_the_catalog_classes_resolves_to_nothing() {
        let ont = Ontology::load_default().unwrap();
        let path = std::env::temp_dir().join(format!("ograph-gateway-provisional-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let mut b = Batch::new("test", "k", "rule");
        let (ev, m) = (b.record("r1"), Extraction::Rule { extractor: "rule".into() });
        let vendor = crate::model::EntityRef::Provisional { class: "https://ograph.dev/ops#Vendor", name: "Acme".into() };
        b.add(vendor, crate::vocab::RDFS_LABEL, lit("Acme"), &ev, &m);
        let out = super::submit(&store, &ont, &b, 0).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(out.accepted, 0);
        assert!(out.quarantined[0].reason.contains("only the catalog resolves by name"), "{:?}", out.quarantined);
    }

    #[test]
    fn siblings_differ_in_what_they_name() {
        let is = |q: &str, k: &str| sibling(&words(q), &words(k));
        assert!(is("inventory-service-uk", "inventory-service-eu"));
        assert!(is("inventory-service-eu-canary", "inventory-service-eu"));
        assert!(!is("inventory-svc-eu", "inventory-service-eu"));
        assert!(!is("checkout-svc", "checkout-service"));
        assert!(!is("checkout-svc", "checkout"));
        assert!(is("orders-db2", "orders-db1") && is("shard02", "shard01") && is("redis2", "redis1"));
        assert!(!is("orders-db1", "orders-db01") && !is("shard0", "shard00"));
        assert!(!is("orders-db-1", "orders-db-01") && !is("api-v1", "api-v01"));
        assert!(is("orders-db2", "orders-db") && is("redis2", "redis"));
        assert!(is("redis0", "redis") && !is("db0", "db00"));
    }
}
