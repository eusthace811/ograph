//! SHACL Core over any [`Graph`]. One implementation serves the W3C core suite (in-memory
//! graphs), the gateway (a batch overlaid on the store) and `ograph validate` (the whole store).
//! Validating only chosen focus nodes is what the gateway needs and what no crate offers.

use oxrdf::vocab::{rdf, rdfs, xsd};
use oxrdf::{BlankNode, Literal, NamedNode, NamedNodeRef, Term};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;

pub const SH: &str = "http://www.w3.org/ns/shacl#";

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{SH}{local}"))
}

/// The read access validation needs. Subjects and objects are `Term`s so literals can be focus nodes.
pub trait Graph {
    fn objects(&self, s: &Term, p: &NamedNode) -> Vec<Term>;
    fn subjects(&self, p: &NamedNode, o: &Term) -> Vec<Term>;
    fn outgoing(&self, s: &Term) -> Vec<(NamedNode, Term)>;
    fn with_predicate(&self, p: &NamedNode) -> Vec<(Term, Term)>;
    /// `class` and every class it is a subclass of. Override when the closure is precomputed.
    fn superclasses(&self, class: &Term) -> Vec<Term> {
        closure(&Path::Pred(rdfs::SUB_CLASS_OF.into_owned()), self.as_dyn(), class, true)
    }
    fn as_dyn(&self) -> &dyn Graph;
}

impl Graph for oxrdf::Graph {
    fn objects(&self, s: &Term, p: &NamedNode) -> Vec<Term> {
        let Some(s) = as_subject(s) else { return vec![] };
        self.objects_for_subject_predicate(&s, p).map(|t| t.into_owned()).collect()
    }
    fn subjects(&self, p: &NamedNode, o: &Term) -> Vec<Term> {
        self.subjects_for_predicate_object(p, o).map(|s| Term::from(s.into_owned())).collect()
    }
    fn outgoing(&self, s: &Term) -> Vec<(NamedNode, Term)> {
        let Some(s) = as_subject(s) else { return vec![] };
        self.triples_for_subject(&s).map(|t| (t.predicate.into_owned(), t.object.into_owned())).collect()
    }
    fn with_predicate(&self, p: &NamedNode) -> Vec<(Term, Term)> {
        self.triples_for_predicate(p).map(|t| (Term::from(t.subject.into_owned()), t.object.into_owned())).collect()
    }
    fn as_dyn(&self) -> &dyn Graph {
        self
    }
}

pub fn as_subject(t: &Term) -> Option<oxrdf::NamedOrBlankNode> {
    match t {
        Term::NamedNode(n) => Some(n.clone().into()),
        Term::BlankNode(b) => Some(b.clone().into()),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub enum Path {
    Pred(NamedNode),
    Inverse(Box<Path>),
    Seq(Vec<Path>),
    Alt(Vec<Path>),
    ZeroOrMore(Box<Path>),
    OneOrMore(Box<Path>),
    ZeroOrOne(Box<Path>),
}

impl Path {
    fn eval(&self, g: &dyn Graph, node: &Term) -> Vec<Term> {
        let mut out: Vec<Term> = match self {
            Path::Pred(p) => g.objects(node, p),
            Path::Inverse(inner) => match &**inner {
                Path::Pred(p) => g.subjects(p, node),
                other => inverse_eval(other, g, node),
            },
            Path::Seq(parts) => {
                let mut cur = vec![node.clone()];
                for part in parts {
                    cur = dedup(cur.iter().flat_map(|n| part.eval(g, n)).collect());
                }
                cur
            }
            Path::Alt(parts) => parts.iter().flat_map(|p| p.eval(g, node)).collect(),
            Path::ZeroOrMore(inner) => closure(inner, g, node, true),
            Path::OneOrMore(inner) => closure(inner, g, node, false),
            Path::ZeroOrOne(inner) => std::iter::once(node.clone()).chain(inner.eval(g, node)).collect(),
        };
        out = dedup(out);
        out
    }
}

fn inverse_eval(path: &Path, g: &dyn Graph, node: &Term) -> Vec<Term> {
    match path {
        Path::Pred(p) => g.subjects(p, node),
        Path::Inverse(inner) => inner.eval(g, node),
        Path::Seq(parts) => {
            let mut cur = vec![node.clone()];
            for part in parts.iter().rev() {
                cur = dedup(cur.iter().flat_map(|n| inverse_eval(part, g, n)).collect());
            }
            cur
        }
        Path::Alt(parts) => dedup(parts.iter().flat_map(|p| inverse_eval(p, g, node)).collect()),
        Path::ZeroOrMore(inner) => closure(&Path::Inverse(inner.clone()), g, node, true),
        Path::OneOrMore(inner) => closure(&Path::Inverse(inner.clone()), g, node, false),
        Path::ZeroOrOne(inner) => dedup(std::iter::once(node.clone()).chain(inverse_eval(inner, g, node)).collect()),
    }
}

fn closure(step: &Path, g: &dyn Graph, node: &Term, reflexive: bool) -> Vec<Term> {
    let mut seen: Vec<Term> = if reflexive { vec![node.clone()] } else { vec![] };
    let mut set: HashSet<Term> = seen.iter().cloned().collect();
    let mut frontier = vec![node.clone()];
    while let Some(n) = frontier.pop() {
        for m in step.eval(g, &n) {
            if set.insert(m.clone()) {
                seen.push(m.clone());
                frontier.push(m);
            }
        }
    }
    seen
}

fn dedup(v: Vec<Term>) -> Vec<Term> {
    let mut seen = HashSet::with_capacity(v.len());
    v.into_iter().filter(|t| seen.insert(t.clone())).collect()
}

#[derive(Clone, Debug)]
pub enum Target {
    Class(Term),
    Node(Term),
    SubjectsOf(NamedNode),
    ObjectsOf(NamedNode),
}

#[derive(Clone, Debug)]
pub enum Constraint {
    Class(Term),
    Datatype(NamedNode),
    NodeKind(NamedNode),
    MinCount(u64),
    MaxCount(u64),
    MinExclusive(Term),
    MinInclusive(Term),
    MaxExclusive(Term),
    MaxInclusive(Term),
    MinLength(u64),
    MaxLength(u64),
    Pattern(regex::Regex),
    LanguageIn(Vec<String>),
    UniqueLang,
    Equals(NamedNode),
    Disjoint(NamedNode),
    LessThan(NamedNode),
    LessThanOrEquals(NamedNode),
    Not(Term),
    And(Vec<Term>),
    Or(Vec<Term>),
    Xone(Vec<Term>),
    Node(Term),
    Property(Term),
    Qualified { shape: Term, min: Option<u64>, max: Option<u64>, disjoint: bool },
    Closed(Vec<NamedNode>),
    HasValue(Term),
    In(Vec<Term>),
}

impl Constraint {
    pub fn component(&self) -> &'static str {
        match self {
            Constraint::Class(_) => "ClassConstraintComponent",
            Constraint::Datatype(_) => "DatatypeConstraintComponent",
            Constraint::NodeKind(_) => "NodeKindConstraintComponent",
            Constraint::MinCount(_) => "MinCountConstraintComponent",
            Constraint::MaxCount(_) => "MaxCountConstraintComponent",
            Constraint::MinExclusive(_) => "MinExclusiveConstraintComponent",
            Constraint::MinInclusive(_) => "MinInclusiveConstraintComponent",
            Constraint::MaxExclusive(_) => "MaxExclusiveConstraintComponent",
            Constraint::MaxInclusive(_) => "MaxInclusiveConstraintComponent",
            Constraint::MinLength(_) => "MinLengthConstraintComponent",
            Constraint::MaxLength(_) => "MaxLengthConstraintComponent",
            Constraint::Pattern(_) => "PatternConstraintComponent",
            Constraint::LanguageIn(_) => "LanguageInConstraintComponent",
            Constraint::UniqueLang => "UniqueLangConstraintComponent",
            Constraint::Equals(_) => "EqualsConstraintComponent",
            Constraint::Disjoint(_) => "DisjointConstraintComponent",
            Constraint::LessThan(_) => "LessThanConstraintComponent",
            Constraint::LessThanOrEquals(_) => "LessThanOrEqualsConstraintComponent",
            Constraint::Not(_) => "NotConstraintComponent",
            Constraint::And(_) => "AndConstraintComponent",
            Constraint::Or(_) => "OrConstraintComponent",
            Constraint::Xone(_) => "XoneConstraintComponent",
            Constraint::Node(_) => "NodeConstraintComponent",
            Constraint::Property(_) => "PropertyConstraintComponent",
            Constraint::Qualified { .. } => "QualifiedValueShapeConstraintComponent",
            Constraint::Closed(_) => "ClosedConstraintComponent",
            Constraint::HasValue(_) => "HasValueConstraintComponent",
            Constraint::In(_) => "InConstraintComponent",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Shape {
    pub id: Term,
    pub targets: Vec<Target>,
    pub path: Option<Path>,
    pub constraints: Vec<Constraint>,
    pub deactivated: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ValidationResult {
    pub focus: String,
    pub path: Option<String>,
    pub value: Option<String>,
    pub component: &'static str,
    pub shape: String,
    pub message: String,
}

pub struct Shapes {
    pub shapes: Vec<Shape>,
    index: HashMap<Term, usize>,
    sibling_qualified: HashMap<Term, Vec<Term>>,
}

fn list(g: &dyn Graph, head: &Term) -> Vec<Term> {
    let mut out = Vec::new();
    let mut cur = head.clone();
    let nil: Term = rdf::NIL.into_owned().into();
    let mut guard = 0;
    while cur != nil && guard < 10_000 {
        guard += 1;
        let Some(first) = g.objects(&cur, &rdf::FIRST.into_owned()).into_iter().next() else { break };
        out.push(first);
        let Some(rest) = g.objects(&cur, &rdf::REST.into_owned()).into_iter().next() else { break };
        cur = rest;
    }
    out
}

fn parse_path(g: &dyn Graph, t: &Term) -> Option<Path> {
    match t {
        Term::NamedNode(n) => Some(Path::Pred(n.clone())),
        Term::BlankNode(_) => {
            let one = |p: &str| g.objects(t, &sh(p)).into_iter().next();
            if let Some(i) = one("inversePath") {
                return Some(Path::Inverse(Box::new(parse_path(g, &i)?)));
            }
            if let Some(a) = one("alternativePath") {
                return Some(Path::Alt(list(g, &a).iter().filter_map(|x| parse_path(g, x)).collect()));
            }
            if let Some(i) = one("zeroOrMorePath") {
                return Some(Path::ZeroOrMore(Box::new(parse_path(g, &i)?)));
            }
            if let Some(i) = one("oneOrMorePath") {
                return Some(Path::OneOrMore(Box::new(parse_path(g, &i)?)));
            }
            if let Some(i) = one("zeroOrOnePath") {
                return Some(Path::ZeroOrOne(Box::new(parse_path(g, &i)?)));
            }
            let items = list(g, t);
            (!items.is_empty()).then(|| Path::Seq(items.iter().filter_map(|x| parse_path(g, x)).collect()))
        }
        _ => None,
    }
}

fn path_string(p: &Path) -> String {
    match p {
        Path::Pred(n) => n.to_string(),
        Path::Inverse(i) => format!("^{}", path_string(i)),
        Path::Seq(v) => v.iter().map(path_string).collect::<Vec<_>>().join("/"),
        Path::Alt(v) => format!("({})", v.iter().map(path_string).collect::<Vec<_>>().join("|")),
        Path::ZeroOrMore(i) => format!("{}*", path_string(i)),
        Path::OneOrMore(i) => format!("{}+", path_string(i)),
        Path::ZeroOrOne(i) => format!("{}?", path_string(i)),
    }
}

fn int(t: &Term) -> Option<u64> {
    match t {
        Term::Literal(l) => u64::from_str(l.value()).ok(),
        _ => None,
    }
}

fn named(t: &Term) -> Option<NamedNode> {
    match t {
        Term::NamedNode(n) => Some(n.clone()),
        _ => None,
    }
}

impl Shapes {
    pub fn parse(g: &dyn Graph) -> Self {
        let ty = rdf::TYPE.into_owned();
        let mut ids: Vec<Term> = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |t: Term, ids: &mut Vec<Term>| {
            if seen.insert(t.clone()) {
                ids.push(t);
            }
        };
        for class in ["NodeShape", "PropertyShape"] {
            for s in g.subjects(&ty, &sh(class).into()) {
                push(s, &mut ids);
            }
        }
        for p in ["targetClass", "targetNode", "targetSubjectsOf", "targetObjectsOf", "path"] {
            for (s, _) in g.with_predicate(&sh(p)) {
                push(s, &mut ids);
            }
        }
        for p in ["property", "node", "not", "qualifiedValueShape"] {
            for (_, o) in g.with_predicate(&sh(p)) {
                push(o, &mut ids);
            }
        }
        for p in ["and", "or", "xone"] {
            for (_, o) in g.with_predicate(&sh(p)) {
                for m in list(g, &o) {
                    push(m, &mut ids);
                }
            }
        }
        let mut shapes = Vec::new();
        let mut sibling_qualified: HashMap<Term, Vec<Term>> = HashMap::new();
        for (_, prop) in g.with_predicate(&sh("property")) {
            let _ = prop;
        }
        for (parent, _) in g.with_predicate(&sh("property")) {
            let props = g.objects(&parent, &sh("property"));
            let qs: Vec<Term> = props.iter().flat_map(|p| g.objects(p, &sh("qualifiedValueShape"))).collect();
            for p in &props {
                let mine: BTreeSet<String> = g.objects(p, &sh("qualifiedValueShape")).iter().map(|t| t.to_string()).collect();
                let others: Vec<Term> = qs.iter().filter(|q| !mine.contains(&q.to_string())).cloned().collect();
                sibling_qualified.insert(p.clone(), others);
            }
        }
        for id in ids {
            shapes.push(Self::parse_shape(g, id));
        }
        let index = shapes.iter().enumerate().map(|(i, s)| (s.id.clone(), i)).collect();
        Self { shapes, index, sibling_qualified }
    }

    fn parse_shape(g: &dyn Graph, id: Term) -> Shape {
        let all = |p: &str| g.objects(&id, &sh(p));
        let one = |p: &str| all(p).into_iter().next();
        let mut targets = Vec::new();
        for c in all("targetClass") {
            targets.push(Target::Class(c));
        }
        let ty = rdf::TYPE.into_owned();
        let types = g.objects(&id, &ty);
        if types.contains(&rdfs::CLASS.into_owned().into()) || types.contains(&NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#Class").into()) {
            if types.iter().any(|t| *t == sh("NodeShape").into() || *t == sh("PropertyShape").into()) {
                targets.push(Target::Class(id.clone()));
            }
        }
        for n in all("targetNode") {
            targets.push(Target::Node(n));
        }
        for p in all("targetSubjectsOf").iter().filter_map(named) {
            targets.push(Target::SubjectsOf(p));
        }
        for p in all("targetObjectsOf").iter().filter_map(named) {
            targets.push(Target::ObjectsOf(p));
        }
        let path = one("path").and_then(|p| parse_path(g, &p));
        let mut c = Vec::new();
        for v in all("class") {
            c.push(Constraint::Class(v));
        }
        if let Some(v) = one("datatype").and_then(|t| named(&t)) {
            c.push(Constraint::Datatype(v));
        }
        if let Some(v) = one("nodeKind").and_then(|t| named(&t)) {
            c.push(Constraint::NodeKind(v));
        }
        if let Some(v) = one("minCount").and_then(|t| int(&t)) {
            c.push(Constraint::MinCount(v));
        }
        if let Some(v) = one("maxCount").and_then(|t| int(&t)) {
            c.push(Constraint::MaxCount(v));
        }
        for (p, f) in [
            ("minExclusive", Constraint::MinExclusive as fn(Term) -> Constraint),
            ("minInclusive", Constraint::MinInclusive),
            ("maxExclusive", Constraint::MaxExclusive),
            ("maxInclusive", Constraint::MaxInclusive),
        ] {
            if let Some(v) = one(p) {
                c.push(f(v));
            }
        }
        if let Some(v) = one("minLength").and_then(|t| int(&t)) {
            c.push(Constraint::MinLength(v));
        }
        if let Some(v) = one("maxLength").and_then(|t| int(&t)) {
            c.push(Constraint::MaxLength(v));
        }
        if let Some(Term::Literal(p)) = one("pattern") {
            let flags = match one("flags") {
                Some(Term::Literal(f)) => f.value().to_owned(),
                _ => String::new(),
            };
            let prefix = if flags.is_empty() { String::new() } else { format!("(?{})", flags.replace('q', "")) };
            let body = if flags.contains('q') { regex::escape(p.value()) } else { p.value().to_owned() };
            if let Ok(re) = regex::Regex::new(&format!("{prefix}{body}")) {
                c.push(Constraint::Pattern(re));
            }
        }
        if let Some(l) = one("languageIn") {
            c.push(Constraint::LanguageIn(list(g, &l).iter().filter_map(|t| match t {
                Term::Literal(l) => Some(l.value().to_lowercase()),
                _ => None,
            }).collect()));
        }
        if matches!(one("uniqueLang"), Some(Term::Literal(l)) if l.value() == "true") {
            c.push(Constraint::UniqueLang);
        }
        for (p, f) in [
            ("equals", Constraint::Equals as fn(NamedNode) -> Constraint),
            ("disjoint", Constraint::Disjoint),
            ("lessThan", Constraint::LessThan),
            ("lessThanOrEquals", Constraint::LessThanOrEquals),
        ] {
            for v in all(p).iter().filter_map(named) {
                c.push(f(v));
            }
        }
        for v in all("not") {
            c.push(Constraint::Not(v));
        }
        for v in all("and") {
            c.push(Constraint::And(list(g, &v)));
        }
        for v in all("or") {
            c.push(Constraint::Or(list(g, &v)));
        }
        for v in all("xone") {
            c.push(Constraint::Xone(list(g, &v)));
        }
        for v in all("node") {
            c.push(Constraint::Node(v));
        }
        for v in all("property") {
            c.push(Constraint::Property(v));
        }
        if let Some(q) = one("qualifiedValueShape") {
            let disjoint = matches!(one("qualifiedValueShapesDisjoint"), Some(Term::Literal(l)) if l.value() == "true");
            c.push(Constraint::Qualified {
                shape: q,
                min: one("qualifiedMinCount").and_then(|t| int(&t)),
                max: one("qualifiedMaxCount").and_then(|t| int(&t)),
                disjoint,
            });
        }
        if matches!(one("closed"), Some(Term::Literal(l)) if l.value() == "true") {
            let mut allowed: Vec<NamedNode> = all("property")
                .iter()
                .filter_map(|p| g.objects(p, &sh("path")).into_iter().next().and_then(|t| named(&t)))
                .collect();
            if let Some(ign) = one("ignoredProperties") {
                allowed.extend(list(g, &ign).iter().filter_map(named));
            }
            c.push(Constraint::Closed(allowed));
        }
        for v in all("hasValue") {
            c.push(Constraint::HasValue(v));
        }
        if let Some(l) = one("in") {
            c.push(Constraint::In(list(g, &l)));
        }
        let deactivated = matches!(one("deactivated"), Some(Term::Literal(l)) if l.value() == "true");
        let message = match one("message") {
            Some(Term::Literal(l)) => Some(l.value().to_owned()),
            _ => None,
        };
        Shape { id, targets, path, constraints: c, deactivated, message }
    }

    pub fn get(&self, id: &Term) -> Option<&Shape> {
        self.index.get(id).map(|&i| &self.shapes[i])
    }

    pub fn from_shapes(shapes: Vec<Shape>) -> Self {
        let index = shapes.iter().enumerate().map(|(i, s)| (s.id.clone(), i)).collect();
        Self { shapes, index, sibling_qualified: HashMap::new() }
    }
}

/// Instances of `class` including subclasses, following `rdfs:subClassOf` in the data graph.
pub fn instances(g: &dyn Graph, class: &Term) -> Vec<Term> {
    let sub = rdfs::SUB_CLASS_OF.into_owned();
    let ty = rdf::TYPE.into_owned();
    let classes = closure(&Path::Inverse(Box::new(Path::Pred(sub))), g, class, true);
    dedup(classes.iter().flat_map(|c| g.subjects(&ty, c)).collect())
}

pub fn is_instance(g: &dyn Graph, node: &Term, class: &Term) -> bool {
    let ty = rdf::TYPE.into_owned();
    g.objects(node, &ty).iter().any(|t| t == class || g.superclasses(t).contains(class))
}

pub struct Validator<'a> {
    pub shapes: &'a Shapes,
    pub data: &'a dyn Graph,
    depth: std::cell::Cell<usize>,
}

impl<'a> Validator<'a> {
    pub fn new(shapes: &'a Shapes, data: &'a dyn Graph) -> Self {
        Self { shapes, data, depth: std::cell::Cell::new(0) }
    }

    pub fn focus_nodes(&self, shape: &Shape) -> Vec<Term> {
        let g = self.data;
        let mut out = Vec::new();
        for t in &shape.targets {
            match t {
                Target::Class(c) => out.extend(instances(g, c)),
                Target::Node(n) => out.push(n.clone()),
                Target::SubjectsOf(p) => out.extend(g.with_predicate(p).into_iter().map(|(s, _)| s)),
                Target::ObjectsOf(p) => out.extend(g.with_predicate(p).into_iter().map(|(_, o)| o)),
            }
        }
        dedup(out)
    }

    /// Validates every target of every shape.
    pub fn validate_all(&self) -> Vec<ValidationResult> {
        let mut out = Vec::new();
        for shape in &self.shapes.shapes {
            if shape.deactivated {
                continue;
            }
            for focus in self.focus_nodes(shape) {
                self.validate_shape(shape, &focus, &mut out);
            }
        }
        out
    }

    /// Validates only `focus` nodes against the shapes that target them. One `outgoing` read per
    /// node decides the `targetSubjectsOf` and `targetClass` shapes, so cost follows the batch size
    /// rather than the number of shapes.
    pub fn validate_focus(&self, focus: &[Term]) -> Vec<ValidationResult> {
        let mut out = Vec::new();
        let ty = rdf::TYPE.into_owned();
        for f in focus {
            let edges = self.data.outgoing(f);
            let preds: HashSet<&NamedNode> = edges.iter().map(|(p, _)| p).collect();
            let classes: HashSet<Term> = edges.iter().filter(|(p, _)| *p == ty).flat_map(|(_, c)| self.data.superclasses(c)).collect();
            for shape in &self.shapes.shapes {
                let hit = !shape.deactivated && shape.targets.iter().any(|t| match t {
                    Target::Class(c) => classes.contains(c),
                    Target::Node(n) => n == f,
                    Target::SubjectsOf(p) => preds.contains(p),
                    Target::ObjectsOf(p) => !self.data.subjects(p, f).is_empty(),
                });
                if hit {
                    self.validate_shape(shape, f, &mut out);
                }
            }
        }
        out
    }

    pub fn conforms(&self, shape_id: &Term, focus: &Term) -> bool {
        let Some(shape) = self.shapes.get(shape_id) else { return true };
        if self.depth.get() > 64 {
            return true;
        }
        self.depth.set(self.depth.get() + 1);
        let mut out = Vec::new();
        self.validate_shape(shape, focus, &mut out);
        self.depth.set(self.depth.get() - 1);
        out.is_empty()
    }

    pub fn validate_shape(&self, shape: &Shape, focus: &Term, out: &mut Vec<ValidationResult>) {
        if shape.deactivated {
            return;
        }
        let values = match &shape.path {
            Some(p) => p.eval(self.data, focus),
            None => vec![focus.clone()],
        };
        for c in &shape.constraints {
            self.check(shape, c, focus, &values, out);
        }
    }

    fn result(&self, shape: &Shape, c: &Constraint, focus: &Term, value: Option<&Term>, why: String) -> ValidationResult {
        ValidationResult {
            focus: focus.to_string(),
            path: shape.path.as_ref().map(path_string),
            value: value.map(Term::to_string),
            component: c.component(),
            shape: shape.id.to_string(),
            message: shape.message.clone().unwrap_or(why),
        }
    }

    fn check(&self, shape: &Shape, c: &Constraint, focus: &Term, values: &[Term], out: &mut Vec<ValidationResult>) {
        let g = self.data;
        let mut each = |ok: &dyn Fn(&Term) -> bool, why: &dyn Fn(&Term) -> String| {
            for v in values {
                if !ok(v) {
                    out.push(self.result(shape, c, focus, Some(v), why(v)));
                }
            }
        };
        match c {
            Constraint::Class(class) => each(&|v| is_instance(g, v, class), &|v| format!("{v} is not a {class}")),
            Constraint::Datatype(dt) => each(&|v| matches!(v, Term::Literal(l) if l.datatype() == dt.as_ref() && well_formed(l)), &|v| format!("{v} is not a well-formed {dt}")),
            Constraint::NodeKind(k) => each(&|v| node_kind_ok(k, v), &|v| format!("{v} is not of kind {k}")),
            Constraint::MinCount(n) => {
                if (values.len() as u64) < *n {
                    out.push(self.result(shape, c, focus, None, format!("{} values, at least {n} required", values.len())));
                }
            }
            Constraint::MaxCount(n) => {
                if (values.len() as u64) > *n {
                    out.push(self.result(shape, c, focus, None, format!("{} values, at most {n} allowed", values.len())));
                }
            }
            Constraint::MinExclusive(b) => each(&|v| compare(v, b) == Some(Ordering::Greater), &|v| format!("{v} not > {b}")),
            Constraint::MinInclusive(b) => each(&|v| matches!(compare(v, b), Some(Ordering::Greater | Ordering::Equal)), &|v| format!("{v} not >= {b}")),
            Constraint::MaxExclusive(b) => each(&|v| compare(v, b) == Some(Ordering::Less), &|v| format!("{v} not < {b}")),
            Constraint::MaxInclusive(b) => each(&|v| matches!(compare(v, b), Some(Ordering::Less | Ordering::Equal)), &|v| format!("{v} not <= {b}")),
            Constraint::MinLength(n) => each(&|v| str_of(v).is_some_and(|s| s.chars().count() as u64 >= *n), &|v| format!("{v} shorter than {n}")),
            Constraint::MaxLength(n) => each(&|v| str_of(v).is_some_and(|s| s.chars().count() as u64 <= *n), &|v| format!("{v} longer than {n}")),
            Constraint::Pattern(re) => each(&|v| str_of(v).is_some_and(|s| re.is_match(&s)), &|v| format!("{v} does not match {}", re.as_str())),
            Constraint::LanguageIn(langs) => each(
                &|v| matches!(v, Term::Literal(l) if l.language().is_some_and(|lang| { let lang = lang.to_lowercase(); langs.iter().any(|r| lang == *r || lang.starts_with(&format!("{r}-"))) })),
                &|v| format!("{v} language not allowed"),
            ),
            Constraint::UniqueLang => {
                let mut count: HashMap<String, usize> = HashMap::new();
                for v in values {
                    if let Term::Literal(l) = v {
                        if let Some(lang) = l.language() {
                            *count.entry(lang.to_lowercase()).or_default() += 1;
                        }
                    }
                }
                for (lang, n) in count {
                    if n > 1 {
                        out.push(self.result(shape, c, focus, None, format!("language {lang} used {n} times")));
                    }
                }
            }
            Constraint::Equals(p) => {
                let other: HashSet<Term> = g.objects(focus, p).into_iter().collect();
                let mine: HashSet<Term> = values.iter().cloned().collect();
                for v in mine.symmetric_difference(&other) {
                    out.push(self.result(shape, c, focus, Some(v), format!("{v} not in both")));
                }
            }
            Constraint::Disjoint(p) => {
                let other: HashSet<Term> = g.objects(focus, p).into_iter().collect();
                each(&|v| !other.contains(v), &|v| format!("{v} also a value of {p}"));
            }
            Constraint::LessThan(p) | Constraint::LessThanOrEquals(p) => {
                let strict = matches!(c, Constraint::LessThan(_));
                let others = g.objects(focus, p);
                for v in values {
                    for o in &others {
                        let ok = match compare(v, o) {
                            Some(Ordering::Less) => true,
                            Some(Ordering::Equal) => !strict,
                            _ => false,
                        };
                        if !ok {
                            out.push(self.result(shape, c, focus, Some(v), format!("{v} not less than {o}")));
                        }
                    }
                }
            }
            Constraint::Not(s) => each(&|v| !self.conforms(s, v), &|v| format!("{v} conforms to {s}")),
            Constraint::And(ss) => each(&|v| ss.iter().all(|s| self.conforms(s, v)), &|v| format!("{v} fails sh:and")),
            Constraint::Or(ss) => each(&|v| ss.iter().any(|s| self.conforms(s, v)), &|v| format!("{v} fails sh:or")),
            Constraint::Xone(ss) => each(&|v| ss.iter().filter(|s| self.conforms(s, v)).count() == 1, &|v| format!("{v} fails sh:xone")),
            Constraint::Node(s) => each(&|v| self.conforms(s, v), &|v| format!("{v} does not conform to {s}")),
            Constraint::Property(s) => {
                if let Some(ps) = self.shapes.get(s) {
                    for v in values {
                        self.validate_shape(ps, v, out);
                    }
                }
            }
            Constraint::Qualified { shape: q, min, max, disjoint } => {
                let siblings = if *disjoint { self.shapes.sibling_qualified.get(&shape.id).cloned().unwrap_or_default() } else { vec![] };
                let n = values
                    .iter()
                    .filter(|v| self.conforms(q, v) && !siblings.iter().any(|s| self.conforms(s, v)))
                    .count() as u64;
                if min.is_some_and(|m| n < m) {
                    out.push(self.result(shape, c, focus, None, format!("{n} values conform, at least {} required", min.unwrap())));
                }
                if max.is_some_and(|m| n > m) {
                    out.push(self.result(shape, c, focus, None, format!("{n} values conform, at most {} allowed", max.unwrap())));
                }
            }
            Constraint::Closed(allowed) => {
                for v in values {
                    for (p, o) in g.outgoing(v) {
                        if !allowed.contains(&p) {
                            let mut r = self.result(shape, c, focus, Some(&o), format!("{p} not allowed"));
                            r.path = Some(p.to_string());
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::HasValue(h) => {
                if !values.contains(h) {
                    out.push(self.result(shape, c, focus, None, format!("missing value {h}")));
                }
            }
            Constraint::In(allowed) => each(&|v| allowed.contains(v), &|v| format!("{v} not in the closed list")),
        }
    }
}

fn node_kind_ok(kind: &NamedNode, v: &Term) -> bool {
    let k = kind.as_str().strip_prefix(SH).unwrap_or("");
    match v {
        Term::NamedNode(_) => matches!(k, "IRI" | "BlankNodeOrIRI" | "IRIOrLiteral"),
        Term::BlankNode(_) => matches!(k, "BlankNode" | "BlankNodeOrIRI" | "BlankNodeOrLiteral"),
        Term::Literal(_) => matches!(k, "Literal" | "BlankNodeOrLiteral" | "IRIOrLiteral"),
    }
}

fn str_of(v: &Term) -> Option<String> {
    match v {
        Term::NamedNode(n) => Some(n.as_str().to_owned()),
        Term::Literal(l) => Some(l.value().to_owned()),
        Term::BlankNode(_) => None,
    }
}

const INT_TYPES: &[(&str, i128, i128)] = &[
    ("integer", i128::MIN, i128::MAX),
    ("long", i64::MIN as i128, i64::MAX as i128),
    ("int", i32::MIN as i128, i32::MAX as i128),
    ("short", i16::MIN as i128, i16::MAX as i128),
    ("byte", i8::MIN as i128, i8::MAX as i128),
    ("nonNegativeInteger", 0, i128::MAX),
    ("positiveInteger", 1, i128::MAX),
    ("nonPositiveInteger", i128::MIN, 0),
    ("negativeInteger", i128::MIN, -1),
    ("unsignedLong", 0, u64::MAX as i128),
    ("unsignedInt", 0, u32::MAX as i128),
    ("unsignedShort", 0, u16::MAX as i128),
    ("unsignedByte", 0, u8::MAX as i128),
];

pub fn well_formed(l: &Literal) -> bool {
    let dt = l.datatype().as_str();
    let Some(local) = dt.strip_prefix("http://www.w3.org/2001/XMLSchema#") else {
        return dt != rdf::LANG_STRING.as_str() || l.language().is_some();
    };
    let v = l.value();
    if let Some((_, lo, hi)) = INT_TYPES.iter().find(|(n, _, _)| *n == local) {
        let t = v.trim_start_matches('+');
        return !t.is_empty() && t.trim_start_matches('-').chars().all(|c| c.is_ascii_digit()) && i128::from_str(t).is_ok_and(|n| n >= *lo && n <= *hi);
    }
    match local {
        "decimal" => oxsdatatypes::Decimal::from_str(v).is_ok(),
        "double" => oxsdatatypes::Double::from_str(v).is_ok(),
        "float" => oxsdatatypes::Float::from_str(v).is_ok(),
        "boolean" => oxsdatatypes::Boolean::from_str(v).is_ok(),
        "dateTime" => oxsdatatypes::DateTime::from_str(v).is_ok(),
        "date" => oxsdatatypes::Date::from_str(v).is_ok(),
        "time" => oxsdatatypes::Time::from_str(v).is_ok(),
        "duration" => oxsdatatypes::Duration::from_str(v).is_ok(),
        "dayTimeDuration" => oxsdatatypes::DayTimeDuration::from_str(v).is_ok(),
        "gYear" => oxsdatatypes::GYear::from_str(v).is_ok(),
        _ => true,
    }
}

fn numeric(l: &Literal) -> Option<f64> {
    let local = l.datatype().as_str().strip_prefix("http://www.w3.org/2001/XMLSchema#")?;
    if INT_TYPES.iter().any(|(n, _, _)| *n == local) || matches!(local, "decimal" | "double" | "float") {
        f64::from_str(l.value().trim_start_matches('+')).ok().or(match l.value() {
            "INF" => Some(f64::INFINITY),
            "-INF" => Some(f64::NEG_INFINITY),
            _ => None,
        })
    } else {
        None
    }
}

pub fn compare(a: &Term, b: &Term) -> Option<Ordering> {
    let (Term::Literal(a), Term::Literal(b)) = (a, b) else { return None };
    if let (Some(x), Some(y)) = (numeric(a), numeric(b)) {
        return x.partial_cmp(&y);
    }
    let same = |dt: NamedNodeRef<'_>| a.datatype() == dt && b.datatype() == dt;
    if same(xsd::DATE_TIME) {
        return oxsdatatypes::DateTime::from_str(a.value()).ok()?.partial_cmp(&oxsdatatypes::DateTime::from_str(b.value()).ok()?);
    }
    if same(xsd::DATE) {
        return oxsdatatypes::Date::from_str(a.value()).ok()?.partial_cmp(&oxsdatatypes::Date::from_str(b.value()).ok()?);
    }
    if same(xsd::STRING) {
        return Some(a.value().cmp(b.value()));
    }
    None
}

pub fn blank() -> Term {
    BlankNode::default().into()
}
