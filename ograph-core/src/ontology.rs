//! The ontology as data: classes and their superclass closure, properties with domain and range,
//! closed vocabularies with per-value definitions, and SHACL shapes derived from all of it plus
//! the hand-written ones in `ontology/shapes.ttl`.

use crate::shacl::{self, Constraint, Path, Shape, Shapes, Target};
use crate::vocab;
use oxrdf::vocab::{rdf, rdfs, xsd};
use oxrdf::{Graph, NamedNode, Term};
use std::collections::{BTreeMap, BTreeSet};

pub const DEFAULT_TTL: &str = include_str!("../../ontology/ograph.ttl");
pub const DEFAULT_SHAPES: &str = include_str!("../../ontology/shapes.ttl");
const OWL: &str = "http://www.w3.org/2002/07/owl#";
const META: &str = "https://ograph.dev/meta#";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Range {
    Class(String),
    Datatype(String),
}

#[derive(Clone, Debug)]
pub struct Property {
    pub iri: String,
    pub domain: Option<String>,
    pub range: Option<Range>,
    pub functional: bool,
}

#[derive(Clone, Debug)]
pub struct Term_ {
    pub value: String,
    pub definition: Option<String>,
}

pub struct Ontology {
    pub graph: Graph,
    pub classes: BTreeMap<String, BTreeSet<String>>,
    pub properties: BTreeMap<String, Property>,
    pub vocabularies: BTreeMap<String, Vec<Term_>>,
    pub shapes: Shapes,
    pub version: String,
}

fn parse(ttl: &str) -> anyhow::Result<Graph> {
    let mut g = Graph::new();
    for t in oxttl::TurtleParser::new().for_slice(ttl) {
        g.insert(&t?);
    }
    Ok(g)
}

fn iri_of(t: &Term) -> Option<String> {
    match t {
        Term::NamedNode(n) => Some(n.as_str().to_owned()),
        _ => None,
    }
}

impl Ontology {
    pub fn load_default() -> anyhow::Result<Self> {
        Self::load(DEFAULT_TTL, DEFAULT_SHAPES)
    }

    pub fn load(ttl: &str, shapes_ttl: &str) -> anyhow::Result<Self> {
        let graph = parse(ttl)?;
        let ty = rdf::TYPE.into_owned();
        let owl = |l: &str| Term::from(NamedNode::new_unchecked(format!("{OWL}{l}")));
        let mut classes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for c in graph.subjects_for_predicate_object(&ty, &owl("Class")) {
            if let oxrdf::NamedOrBlankNodeRef::NamedNode(n) = c {
                classes.insert(n.as_str().to_owned(), BTreeSet::new());
            }
        }
        let direct: BTreeMap<String, Vec<String>> = classes
            .keys()
            .map(|c| {
                let sups = graph
                    .objects_for_subject_predicate(&NamedNode::new_unchecked(c), rdfs::SUB_CLASS_OF)
                    .filter_map(|o| iri_of(&o.into_owned()))
                    .collect();
                (c.clone(), sups)
            })
            .collect();
        for (c, sups) in classes.iter_mut() {
            let mut stack: Vec<String> = vec![c.clone()];
            while let Some(x) = stack.pop() {
                if sups.insert(x.clone()) {
                    stack.extend(direct.get(&x).cloned().unwrap_or_default());
                }
            }
        }
        let mut properties = BTreeMap::new();
        for kind in ["ObjectProperty", "DatatypeProperty"] {
            for p in graph.subjects_for_predicate_object(&ty, &owl(kind)) {
                let oxrdf::NamedOrBlankNodeRef::NamedNode(p) = p else { continue };
                let one = |q| graph.object_for_subject_predicate(p, q).and_then(|o| iri_of(&o.into_owned()));
                let range = one(rdfs::RANGE).map(|r| if kind == "ObjectProperty" { Range::Class(r) } else { Range::Datatype(r) });
                let functional = graph.contains(oxrdf::TripleRef::new(p, rdf::TYPE, NamedNode::new_unchecked(format!("{OWL}FunctionalProperty")).as_ref()));
                properties.insert(p.as_str().to_owned(), Property { iri: p.as_str().to_owned(), domain: one(rdfs::DOMAIN), range, functional });
            }
        }
        let vocab_p = NamedNode::new_unchecked(format!("{META}vocabulary"));
        let value_p = NamedNode::new_unchecked(format!("{META}value"));
        let mut vocabularies: BTreeMap<String, Vec<Term_>> = BTreeMap::new();
        for t in graph.triples_for_predicate(&vocab_p) {
            let oxrdf::NamedOrBlankNodeRef::NamedNode(p) = t.subject else { continue };
            let node = t.object.into_owned();
            let Some(sub) = shacl::as_subject(&node) else { continue };
            let Some(oxrdf::TermRef::Literal(v)) = graph.object_for_subject_predicate(&sub, &value_p) else { continue };
            let definition = match graph.object_for_subject_predicate(&sub, rdfs::COMMENT) {
                Some(oxrdf::TermRef::Literal(c)) => Some(c.value().to_owned()),
                _ => None,
            };
            vocabularies.entry(p.as_str().to_owned()).or_default().push(Term_ { value: v.value().to_owned(), definition });
        }
        for v in vocabularies.values_mut() {
            v.sort_by(|a, b| a.value.cmp(&b.value));
        }
        let version = graph
            .object_for_subject_predicate(&NamedNode::new_unchecked("https://ograph.dev/ontology"), NamedNode::new_unchecked(format!("{OWL}versionInfo")).as_ref())
            .map(|t| match t {
                oxrdf::TermRef::Literal(l) => l.value().to_owned(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let mut ont = Self { graph, classes, properties, vocabularies, shapes: Shapes::from_shapes(vec![]), version };
        ont.shapes = Shapes::parse(&parse(shapes_ttl)?).after(ont.derived_shapes());
        Ok(ont)
    }

    pub fn is_class(&self, iri: &str) -> bool {
        self.classes.contains_key(iri)
    }

    pub fn property(&self, iri: &str) -> Option<&Property> {
        self.properties.get(iri)
    }

    pub fn vocabulary(&self, property: &str) -> Option<&[Term_]> {
        self.vocabularies.get(property).map(Vec::as_slice)
    }

    /// One shape per property: domain on the subject, range or datatype on the values, the
    /// closed vocabulary and functional cardinality. Generated, so the ontology is the one source.
    fn derived_shapes(&self) -> Vec<Shape> {
        let mut out = Vec::new();
        for p in self.properties.values() {
            let pred = NamedNode::new_unchecked(&p.iri);
            let id = |what: &str| Term::from(NamedNode::new_unchecked(format!("https://ograph.dev/shape/{}/{what}", vocab::compact(&p.iri).replace(':', "-"))));
            if let Some(d) = &p.domain
                && d != "https://ograph.dev/meta#Thing" {
                    out.push(Shape {
                        id: id("domain"),
                        targets: vec![Target::SubjectsOf(pred.clone())],
                        path: None,
                        constraints: vec![Constraint::Class(NamedNode::new_unchecked(d).into())],
                        deactivated: false,
                        message: Some(format!("subject of {} must be a {}", vocab::compact(&p.iri), vocab::compact(d))),
                    });
                }
            let mut c = Vec::new();
            match &p.range {
                Some(Range::Class(r)) if r != "https://ograph.dev/meta#Thing" => c.push(Constraint::Class(NamedNode::new_unchecked(r).into())),
                Some(Range::Datatype(r)) => c.push(Constraint::Datatype(NamedNode::new_unchecked(r))),
                _ => {}
            }
            if p.functional {
                c.push(Constraint::MaxCount(1));
            }
            if let Some(v) = self.vocabularies.get(&p.iri) {
                c.push(Constraint::In(v.iter().map(|t| oxrdf::Literal::new_simple_literal(&t.value).into()).collect()));
            }
            if !c.is_empty() {
                out.push(Shape {
                    id: id("range"),
                    targets: vec![Target::SubjectsOf(pred.clone())],
                    path: Some(Path::Pred(pred)),
                    constraints: c,
                    deactivated: false,
                    message: None,
                });
            }
        }
        out
    }

    /// Code-to-ontology drift: every IRI in `vocab.rs` must be declared here.
    pub fn check(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for iri in vocab::all() {
            if !self.classes.contains_key(iri) && !self.properties.contains_key(iri) {
                problems.push(format!("{} is used by code but not declared in the ontology", vocab::compact(iri)));
            }
        }
        for p in self.properties.values() {
            for c in p.domain.iter().chain(match &p.range {
                Some(Range::Class(c)) => Some(c),
                _ => None,
            }) {
                if !self.classes.contains_key(c) {
                    problems.push(format!("{} refers to undeclared class {}", vocab::compact(&p.iri), vocab::compact(c)));
                }
            }
            if let Some(Range::Datatype(d)) = &p.range
                && !d.starts_with(xsd::STRING.as_str().trim_end_matches("string")) {
                    problems.push(format!("{} has non-XSD datatype {d}", vocab::compact(&p.iri)));
                }
        }
        problems
    }

    /// `class` and every class it is a subclass of, or `None` when it is not declared.
    fn closure(&self, class: &str) -> Option<&BTreeSet<String>> {
        self.classes.get(class)
    }

    /// Whether every value `old` admits, `new` admits too: a superclass, or no constraint at all.
    fn widens(&self, old: Option<&str>, new: Option<&str>) -> bool {
        match (old, new) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(o), Some(n)) => o == n || self.closure(o).is_some_and(|c| c.contains(n)),
        }
    }

    /// What changed between two ontology versions. A change is BREAKING when stored data uses
    /// what it removes or narrows; the same change to a term `usage` never saw is marked unused.
    pub fn diff(&self, newer: &Ontology, usage: &Usage) -> Vec<String> {
        let mut out = Vec::new();
        for c in self.classes.keys().filter(|c| !newer.classes.contains_key(*c)) {
            out.push(breaking(format!("class removed: {}", vocab::compact(c)), usage.instances(c)));
        }
        for (c, sups) in self.classes.iter().filter(|(c, _)| newer.classes.contains_key(*c)) {
            let lost: Vec<String> = sups.iter().filter(|s| !newer.classes[c].contains(*s)).map(|s| vocab::compact(s)).collect();
            if !lost.is_empty() {
                out.push(breaking(format!("superclass lost: {} is no longer a {}", vocab::compact(c), lost.join(", ")), usage.instances(c)));
            }
        }
        for (iri, p) in &self.properties {
            let Some(q) = newer.properties.get(iri) else {
                out.push(breaking(format!("property removed: {}", vocab::compact(iri)), usage.uses(iri)));
                continue;
            };
            let name = |c: Option<&str>| c.map_or("anything".to_owned(), vocab::compact);
            if p.domain != q.domain {
                let (a, b) = (p.domain.as_deref(), q.domain.as_deref());
                if newer.widens(a, b) {
                    out.push(format!("domain widened: {} from {} to {}", vocab::compact(iri), name(a), name(b)));
                } else {
                    out.push(breaking(format!("domain narrowed: {} from {} to {}", vocab::compact(iri), name(a), name(b)), usage.uses(iri)));
                }
            }
            if p.range != q.range {
                fn class(r: &Option<Range>) -> Result<Option<&str>, &str> {
                    match r {
                        Some(Range::Class(c)) => Ok(Some(c.as_str())),
                        None => Ok(None),
                        Some(Range::Datatype(d)) => Err(d.as_str()),
                    }
                }
                let text = |r: &Option<Range>| match class(r) {
                    Ok(c) => name(c),
                    Err(d) => vocab::compact(d),
                };
                let widened = match (class(&p.range), class(&q.range)) {
                    (_, Ok(None)) => true,
                    (Ok(a), Ok(b)) => newer.widens(a, b),
                    _ => false,
                };
                let what = format!("{}: {} from {} to {}", if widened { "range widened" } else { "range narrowed" }, vocab::compact(iri), text(&p.range), text(&q.range));
                if widened {
                    out.push(what);
                } else {
                    out.push(breaking(what, usage.uses(iri)));
                }
            }
            if !p.functional && q.functional {
                out.push(breaking(format!("became functional: {}", vocab::compact(iri)), usage.uses(iri)));
            }
        }
        for c in newer.classes.keys().filter(|c| !self.classes.contains_key(*c)) {
            out.push(format!("added class: {}", vocab::compact(c)));
        }
        for iri in newer.properties.keys().filter(|p| !self.properties.contains_key(*p)) {
            out.push(format!("added property: {}", vocab::compact(iri)));
        }
        for (p, vals) in &self.vocabularies {
            let Some(new) = newer.vocabularies.get(p) else {
                // Without a vocabulary the property takes any value, so nothing stored breaks.
                out.push(format!("vocabulary opened: {}", vocab::compact(p)));
                continue;
            };
            let new: BTreeSet<&str> = new.iter().map(|t| t.value.as_str()).collect();
            for v in vals.iter().filter(|v| !new.contains(v.value.as_str())) {
                out.push(breaking(format!("vocabulary value removed: {} \"{}\"", vocab::compact(p), v.value), usage.value(p, &v.value)));
            }
        }
        for (p, vals) in &newer.vocabularies {
            let new: BTreeSet<&str> = vals.iter().map(|t| t.value.as_str()).collect();
            if !self.vocabularies.contains_key(p) {
                let outside = usage.values.get(p).map_or(0, |vs| vs.iter().filter(|(v, _)| !new.contains(v.as_str())).map(|(_, n)| n).sum());
                out.push(breaking(format!("vocabulary closed: {}", vocab::compact(p)), outside));
            }
            let old: BTreeSet<&str> = self.vocabularies.get(p).map(|v| v.iter().map(|t| t.value.as_str()).collect()).unwrap_or_default();
            for v in vals.iter().filter(|v| !old.contains(v.value.as_str())) {
                out.push(format!("added vocabulary value: {} \"{}\"", vocab::compact(p), v.value));
            }
        }
        out
    }
}

/// A change that removes or narrows what `uses` stored facts rely on.
fn breaking(what: String, uses: usize) -> String {
    if uses == 0 { format!("{what} (unused)") } else { format!("BREAKING {what} ({uses} uses)") }
}

/// How often stored data uses each term: nodes per most specific type, triples per predicate,
/// and literal values per predicate.
#[derive(Default)]
pub struct Usage {
    pub types: BTreeMap<String, usize>,
    pub predicates: BTreeMap<String, usize>,
    pub values: BTreeMap<String, BTreeMap<String, usize>>,
}

impl Usage {
    pub fn from_snapshot(snap: &crate::store::Snapshot) -> crate::store::Result<Self> {
        let mut u = Usage::default();
        let mut terms: BTreeMap<crate::term::Id, Term> = BTreeMap::new();
        let mut term = |id| -> crate::store::Result<Term> {
            if let Some(t) = terms.get(&id) {
                return Ok(t.clone());
            }
            let t = snap.term(id)?;
            terms.insert(id, t.clone());
            Ok(t)
        };
        for q in snap.quads([None; 3], crate::store::GraphSel::Union) {
            let q = q?;
            let Term::NamedNode(p) = term(q[1])? else { continue };
            *u.predicates.entry(p.as_str().to_owned()).or_default() += 1;
            match term(q[2])? {
                Term::NamedNode(c) if p.as_str() == rdf::TYPE.as_str() => *u.types.entry(c.as_str().to_owned()).or_default() += 1,
                Term::Literal(l) => *u.values.entry(p.as_str().to_owned()).or_default().entry(l.value().to_owned()).or_default() += 1,
                _ => {}
            }
        }
        Ok(u)
    }

    fn instances(&self, class: &str) -> usize {
        self.types.get(class).copied().unwrap_or(0)
    }

    fn uses(&self, property: &str) -> usize {
        self.predicates.get(property).copied().unwrap_or(0)
    }

    fn value(&self, property: &str, value: &str) -> usize {
        self.values.get(property).and_then(|v| v.get(value)).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inc_fault() -> String {
        vocab::inc::FAULT_TYPE.to_owned()
    }

    #[test]
    fn closing_a_free_property_is_breaking() {
        let old = Ontology::load_default().unwrap();
        let ttl = format!("{DEFAULT_TTL}\ninc:faultType og:vocabulary [ og:value \"cpu\" ] .\n");
        let new = Ontology::load(&ttl, DEFAULT_SHAPES).unwrap();
        let stored = Usage { values: [(inc_fault(), [("cpu".to_owned(), 4), ("disk".to_owned(), 2)].into())].into(), ..Usage::default() };
        assert!(old.diff(&new, &stored).iter().any(|d| d == "BREAKING vocabulary closed: inc:faultType (2 uses)"));
        assert!(old.diff(&new, &Usage::default()).iter().any(|d| d == "vocabulary closed: inc:faultType (unused)"));
        assert_eq!(new.diff(&old, &stored), ["vocabulary opened: inc:faultType"]);
    }

    #[test]
    fn a_range_change_is_widening_only_towards_a_superclass() {
        let old = Ontology::load_default().unwrap();
        let edit = |from: &str, to: &str| Ontology::load(&DEFAULT_TTL.replacen(from, to, 1), DEFAULT_SHAPES).unwrap();
        let used = Usage { predicates: [(vocab::evt::ON.to_owned(), 7)].into(), ..Usage::default() };
        let on = "evt:on a owl:ObjectProperty ; rdfs:domain evt:Event ; rdfs:range ops:Application";
        let wider = edit(on, &on.replace("ops:Application", "og:Thing"));
        assert_eq!(old.diff(&wider, &used), ["range widened: evt:on from ops:Application to <https://ograph.dev/meta#Thing>"]);
        let narrower = edit(on, &on.replace("ops:Application", "ops:Team"));
        assert_eq!(old.diff(&narrower, &used), ["BREAKING range narrowed: evt:on from ops:Application to ops:Team (7 uses)"]);
        assert_eq!(wider.diff(&old, &Usage::default()), ["range narrowed: evt:on from <https://ograph.dev/meta#Thing> to ops:Application (unused)"]);
    }

    #[test]
    fn losing_a_superclass_breaks_only_a_class_with_instances() {
        let old = Ontology::load_default().unwrap();
        let commit = "evt:Commit a owl:Class ; rdfs:subClassOf evt:Event";
        let new = Ontology::load(&DEFAULT_TTL.replacen(commit, "evt:Commit a owl:Class ; rdfs:subClassOf og:Thing", 1), DEFAULT_SHAPES).unwrap();
        let used = Usage { types: [(vocab::evt::COMMIT.to_owned(), 3)].into(), ..Usage::default() };
        assert_eq!(old.diff(&new, &used), ["BREAKING superclass lost: evt:Commit is no longer a evt:Event (3 uses)"]);
        assert_eq!(old.diff(&new, &Usage::default()), ["superclass lost: evt:Commit is no longer a evt:Event (unused)"]);
    }
}
