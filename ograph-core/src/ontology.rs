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

    pub fn is_subclass(&self, sub: &str, sup: &str) -> bool {
        self.classes.get(sub).is_some_and(|s| s.contains(sup))
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

    /// What changed between two ontology versions, and which changes can break stored data.
    pub fn diff(&self, newer: &Ontology) -> Vec<String> {
        let mut out = Vec::new();
        for c in self.classes.keys().filter(|c| !newer.classes.contains_key(*c)) {
            out.push(format!("BREAKING class removed: {}", vocab::compact(c)));
        }
        for c in newer.classes.keys().filter(|c| !self.classes.contains_key(*c)) {
            out.push(format!("added class: {}", vocab::compact(c)));
        }
        for (iri, p) in &self.properties {
            match newer.properties.get(iri) {
                None => out.push(format!("BREAKING property removed: {}", vocab::compact(iri))),
                Some(q) => {
                    if p.domain != q.domain || p.range != q.range {
                        out.push(format!("BREAKING domain or range changed: {}", vocab::compact(iri)));
                    }
                    if !p.functional && q.functional {
                        out.push(format!("BREAKING became functional: {}", vocab::compact(iri)));
                    }
                }
            }
        }
        for iri in newer.properties.keys().filter(|p| !self.properties.contains_key(*p)) {
            out.push(format!("added property: {}", vocab::compact(iri)));
        }
        for (p, vals) in &self.vocabularies {
            let new: BTreeSet<&str> = newer.vocabularies.get(p).map(|v| v.iter().map(|t| t.value.as_str()).collect()).unwrap_or_default();
            for v in vals.iter().filter(|v| !new.contains(v.value.as_str())) {
                out.push(format!("BREAKING vocabulary value removed: {} \"{}\"", vocab::compact(p), v.value));
            }
        }
        for (p, vals) in &newer.vocabularies {
            let old: BTreeSet<&str> = self.vocabularies.get(p).map(|v| v.iter().map(|t| t.value.as_str()).collect()).unwrap_or_default();
            for v in vals.iter().filter(|v| !old.contains(v.value.as_str())) {
                out.push(format!("added vocabulary value: {} \"{}\"", vocab::compact(p), v.value));
            }
        }
        out
    }
}
