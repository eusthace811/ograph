//! The only way anything enters the graph: enrichers emit [`ProposedFact`]s, the gateway decides.

use oxrdf::{Literal, NamedNode};
use serde::{Deserialize, Serialize};

/// A model's confidence, in `[0, 1]`. The only constructor checks the range.
#[derive(Copy, Clone, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Confidence(f64);

impl Confidence {
    pub fn new(v: f64) -> Option<Self> {
        (0.0..=1.0).contains(&v).then_some(Self(v))
    }
    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Extraction {
    Rule { extractor: String },
    Catalog { catalog: String },
    Llm { model: String, prompt_hash: String, confidence: Confidence },
    Human { who: String },
}

/// An entity: an IRI, or the name of an application or team, which the gateway resolves against
/// the catalog.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntityRef {
    Known(NamedNode),
    Provisional { class: &'static str, name: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Object {
    Entity(EntityRef),
    Literal(Literal),
}

/// Where a fact came from: a source record id, a `file:line`, or a query.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvidenceRef(pub String);

#[derive(Clone, Debug, PartialEq)]
pub struct ProposedFact {
    pub subject: EntityRef,
    pub predicate: NamedNode,
    pub object: Object,
    pub evidence: Vec<EvidenceRef>,
    pub method: Extraction,
}

impl ProposedFact {
    pub fn describe(&self) -> String {
        let e = |r: &EntityRef| match r {
            EntityRef::Known(n) => crate::vocab::compact(n.as_str()),
            EntityRef::Provisional { class, name } => format!("{}?\"{name}\"", crate::vocab::compact(class)),
        };
        let o = match &self.object {
            Object::Entity(r) => e(r),
            Object::Literal(l) => l.to_string(),
        };
        format!("{} {} {}", e(&self.subject), crate::vocab::compact(self.predicate.as_str()), o)
    }
}

/// One harvest batch: a source, a stable key (so re-running replaces it) and its facts.
#[derive(Clone, Debug)]
pub struct Batch {
    pub source: String,
    pub key: String,
    pub extractor: String,
    pub facts: Vec<ProposedFact>,
    /// Record ids this batch read. Evidence must point at one of them or at an existing fact.
    pub records: std::collections::HashSet<String>,
    pub degraded: Vec<String>,
    /// Files a batch that spans several files read, kept as `harv:file`.
    pub files: Vec<String>,
    /// Facts the enricher itself refuses, with the reason: the gateway quarantines them
    /// unchecked, so the refusal is on record.
    pub withheld: Vec<(ProposedFact, String)>,
}

impl Batch {
    pub fn new(source: &str, key: &str, extractor: &str) -> Self {
        Self { source: source.into(), key: key.into(), extractor: extractor.into(), facts: vec![], records: Default::default(), degraded: vec![], files: vec![], withheld: vec![] }
    }

    pub fn graph(&self) -> NamedNode {
        NamedNode::new_unchecked(format!("{}harvest/{}/{}", crate::vocab::GRAPH, slug(&self.source), path_iri(&self.key)))
    }

    pub fn quarantine_graph(&self) -> NamedNode {
        NamedNode::new_unchecked(format!("{}/{}/{}", crate::vocab::QUARANTINE, slug(&self.source), path_iri(&self.key)))
    }

    /// A batch whose source could not be read at all: it carries no facts, only the reason.
    pub fn failed(&self) -> bool {
        self.facts.is_empty() && !self.degraded.is_empty()
    }

    pub fn record(&mut self, id: impl Into<String>) -> EvidenceRef {
        let id = id.into();
        self.records.insert(id.clone());
        EvidenceRef(id)
    }

    pub fn add(&mut self, subject: EntityRef, predicate: &str, object: Object, evidence: &EvidenceRef, method: &Extraction) {
        self.add_stated(subject, predicate, object, vec![evidence.clone()], method);
    }

    /// [`Batch::add`] for a fact several records state.
    pub fn add_stated(&mut self, subject: EntityRef, predicate: &str, object: Object, evidence: Vec<EvidenceRef>, method: &Extraction) {
        self.facts.push(ProposedFact { subject, predicate: NamedNode::new_unchecked(predicate), object, evidence, method: method.clone() });
    }

    pub fn withhold(&mut self, subject: EntityRef, predicate: &str, object: Object, evidence: &EvidenceRef, method: &Extraction, reason: String) {
        let fact = ProposedFact { subject, predicate: NamedNode::new_unchecked(predicate), object, evidence: vec![evidence.clone()], method: method.clone() };
        self.withheld.push((fact, reason));
    }
}

/// A stable IRI-safe slug: lowercase ASCII letters, digits, `-`, `.` and `_`.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// A relative path as an IRI path, percent-encoding everything but unreserved characters and
/// `/`. Unlike [`slug`] it is injective, so two source files never share a graph.
pub fn path_iri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn entity(kind: &str, name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{}{kind}/{}", crate::vocab::DATA, slug(name)))
}

pub fn known(kind: &str, name: &str) -> EntityRef {
    EntityRef::Known(entity(kind, name))
}

pub fn lit(v: impl Into<Literal>) -> Object {
    Object::Literal(v.into())
}

pub fn obj(n: NamedNode) -> Object {
    Object::Entity(EntityRef::Known(n))
}
