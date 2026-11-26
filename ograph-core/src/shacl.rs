//! SHACL Core over any [`Graph`]. One implementation serves the W3C core suite (in-memory
//! graphs), the gateway (a batch overlaid on the store) and `ograph validate` (the whole store).
//! Validating only chosen focus nodes is what the gateway needs and what no crate offers.

use oxrdf::vocab::{rdf, rdfs, xsd};
use oxrdf::{Literal, NamedNode, NamedNodeRef, Term};
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
    /// `class` and every subclass of it. Override when the closure is precomputed.
    fn subclasses(&self, class: &Term) -> Vec<Term> {
        closure(&Path::Inverse(Box::new(Path::Pred(rdfs::SUB_CLASS_OF.into_owned()))), self.as_dyn(), class, true)
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
    /// The compiled regex and the pattern as authored, which messages quote.
    Pattern(regex::Regex, String),
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
            Constraint::Pattern(..) => "PatternConstraintComponent",
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
    /// Constraints that could not be read, such as an `sh:pattern` the regex engine cannot
    /// compile. Each would otherwise check nothing without a word.
    pub problems: Vec<String>,
    index: HashMap<Term, usize>,
    sibling_qualified: HashMap<Term, Vec<Term>>,
}

/// A regex for an XPath `fn:matches` pattern and its flags. `q` quotes the pattern and leaves
/// only `i`. XPath's `x` drops whitespace outside a character class, unlike the regex crate's
/// (where `#` starts a comment and a class loses its spaces), so it is done here. Without `s`,
/// XPath's `.` matches neither `\n` nor `\r`.
fn xpath_regex(pattern: &str, flags: &str) -> Result<regex::Regex, regex::Error> {
    if flags.contains('q') {
        let prefix = if flags.contains('i') { "(?i)" } else { "" };
        return regex::Regex::new(&format!("{prefix}{}", regex::escape(pattern)));
    }
    let (x, dot_all) = (flags.contains('x'), flags.contains('s'));
    let mut body = String::with_capacity(pattern.len());
    let (mut depth, mut escaped, mut dash) = (0usize, false, false);
    for c in pattern.chars() {
        let was_dash = std::mem::take(&mut dash);
        match c {
            _ if escaped => {
                escaped = false;
                // XPath's `\w` is anything but punctuation, separators and control characters,
                // and its `\s` only the four ASCII spaces; the regex crate's are wider.
                match c {
                    'w' => body.push_str(r"[^\p{P}\p{Z}\p{C}]"),
                    'W' => body.push_str(r"[\p{P}\p{Z}\p{C}]"),
                    's' => body.push_str(r"[ \t\n\r]"),
                    'S' => body.push_str(r"[^ \t\n\r]"),
                    _ => {
                        body.push('\\');
                        body.push(c);
                    }
                }
            }
            '\\' => escaped = true,
            // In a class the regex crate reads `&&` and `~~` as set operations; in XPath they are
            // characters.
            '&' | '~' if depth > 0 => {
                body.push('\\');
                body.push(c);
            }
            '[' => {
                // XPath's `[a-z-[aeiou]]` subtracts; the regex crate spells that `--[`.
                if depth > 0 && was_dash {
                    body.push('-');
                }
                depth += 1;
                body.push(c);
            }
            ']' => {
                depth = depth.saturating_sub(1);
                body.push(c);
            }
            '-' if depth > 0 => {
                dash = true;
                body.push(c);
            }
            ' ' | '\t' | '\n' | '\r' if x && depth == 0 => {}
            '.' if depth == 0 && !dot_all => body.push_str(r"[^\n\r]"),
            _ => body.push(c),
        }
    }
    let inline: String = flags.chars().filter(|c| matches!(c, 'i' | 'm' | 's')).collect();
    let prefix = if inline.is_empty() { String::new() } else { format!("(?{inline})") };
    regex::Regex::new(&format!("{prefix}{body}"))
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
        for (parent, _) in g.with_predicate(&sh("property")) {
            let props = g.objects(&parent, &sh("property"));
            let qs: Vec<Term> = props.iter().flat_map(|p| g.objects(p, &sh("qualifiedValueShape"))).collect();
            for p in &props {
                let mine: BTreeSet<String> = g.objects(p, &sh("qualifiedValueShape")).iter().map(|t| t.to_string()).collect();
                let others: Vec<Term> = qs.iter().filter(|q| !mine.contains(&q.to_string())).cloned().collect();
                sibling_qualified.insert(p.clone(), others);
            }
        }
        let mut problems = Vec::new();
        for id in ids {
            shapes.push(Self::parse_shape(g, id, &mut problems));
        }
        let index = shapes.iter().enumerate().map(|(i, s)| (s.id.clone(), i)).collect();
        Self { shapes, problems, index, sibling_qualified }
    }

    fn parse_shape(g: &dyn Graph, id: Term, problems: &mut Vec<String>) -> Shape {
        let all = |p: &str| g.objects(&id, &sh(p));
        let one = |p: &str| all(p).into_iter().next();
        let mut targets = Vec::new();
        for c in all("targetClass") {
            targets.push(Target::Class(c));
        }
        let ty = rdf::TYPE.into_owned();
        let types = g.objects(&id, &ty);
        if (types.contains(&rdfs::CLASS.into_owned().into()) || types.contains(&NamedNode::new_unchecked("http://www.w3.org/2002/07/owl#Class").into()))
            && types.iter().any(|t| *t == sh("NodeShape").into() || *t == sh("PropertyShape").into()) {
                targets.push(Target::Class(id.clone()));
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
            match xpath_regex(p.value(), &flags) {
                Ok(re) => c.push(Constraint::Pattern(re, p.value().to_owned())),
                Err(e) => problems.push(format!("{id}: sh:pattern {:?} cannot be used: {e}", p.value())),
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
        Self { shapes, problems: vec![], index, sibling_qualified: HashMap::new() }
    }

    /// These shapes with `first` ahead of them, keeping what parsing learned about them.
    pub fn after(mut self, mut first: Vec<Shape>) -> Self {
        first.append(&mut self.shapes);
        self.index = first.iter().enumerate().map(|(i, s)| (s.id.clone(), i)).collect();
        self.shapes = first;
        self
    }
}

/// Instances of `class` including its subclasses.
pub fn instances(g: &dyn Graph, class: &Term) -> Vec<Term> {
    let ty = rdf::TYPE.into_owned();
    dedup(g.subclasses(class).iter().flat_map(|c| g.subjects(&ty, c)).collect())
}

fn is_instance(g: &dyn Graph, node: &Term, class: &Term) -> bool {
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

    fn validate_shape(&self, shape: &Shape, focus: &Term, out: &mut Vec<ValidationResult>) {
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
            Constraint::Pattern(re, source) => each(&|v| str_of(v).is_some_and(|s| re.is_match(&s)), &|v| format!("{v} does not match {source}")),
            Constraint::LanguageIn(langs) => each(
                &|v| matches!(v, Term::Literal(l) if l.language().is_some_and(|lang| { let lang = lang.to_lowercase(); langs.iter().any(|r| lang == *r || lang.starts_with(&format!("{r}-"))) })),
                &|v| format!("{v} language not allowed"),
            ),
            Constraint::UniqueLang => {
                let mut count: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
                for v in values {
                    if let Term::Literal(l) = v
                        && let Some(lang) = l.language() {
                            *count.entry(lang.to_lowercase()).or_default() += 1;
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
                // Hash order would vary from run to run, and the first reason names a quarantine.
                let mut diff: Vec<&Term> = mine.symmetric_difference(&other).collect();
                diff.sort_by_cached_key(|v| v.to_string());
                for v in diff {
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

fn well_formed(l: &Literal) -> bool {
    let dt = l.datatype().as_str();
    let Some(local) = dt.strip_prefix("http://www.w3.org/2001/XMLSchema#") else {
        return dt != rdf::LANG_STRING.as_str() || l.language().is_some();
    };
    let v = l.value();
    if let Some((_, lo, hi)) = INT_TYPES.iter().find(|(n, _, _)| *n == local) {
        let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
        if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        // Beyond i128, only a type with no bound on that side (xsd:integer, nonNegativeInteger, …).
        return match i128::from_str(v.strip_prefix('+').unwrap_or(v)) {
            Ok(n) => n >= *lo && n <= *hi,
            Err(_) if v.starts_with('-') => *lo == i128::MIN,
            Err(_) => *hi == i128::MAX,
        };
    }
    match local {
        "decimal" => decimal_numeral(v),
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

/// The xsd:decimal lexical form `(+|-)?(digits(.digits?)?|.digits)`, whatever its size or precision.
fn decimal_numeral(v: &str) -> bool {
    let n = v.strip_prefix(['+', '-']).unwrap_or(v);
    let (int, frac) = n.split_once('.').unwrap_or((n, ""));
    !(int.is_empty() && frac.is_empty()) && int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit())
}

/// A number: exact for the integer types and decimal, a double for double and float.
enum Number {
    Exact(oxsdatatypes::Decimal),
    Approx(f64),
}

impl Number {
    fn approx(&self) -> f64 {
        match self {
            Self::Exact(d) => f64::from_str(&d.to_string()).unwrap_or(f64::NAN),
            Self::Approx(f) => *f,
        }
    }
}

fn numeric(l: &Literal) -> Option<Number> {
    let local = l.datatype().as_str().strip_prefix("http://www.w3.org/2001/XMLSchema#")?;
    let v = l.value();
    if INT_TYPES.iter().any(|(n, _, _)| *n == local) || local == "decimal" {
        // A number too large or too precise for a Decimal is still compared, as a double; text
        // that is no decimal numeral (`inf`) is not compared at all.
        return match oxsdatatypes::Decimal::from_str(v) {
            Ok(d) => Some(Number::Exact(d)),
            Err(_) if decimal_numeral(v) => f64::from_str(v.strip_prefix('+').unwrap_or(v)).ok().map(Number::Approx),
            Err(_) => None,
        };
    }
    if matches!(local, "double" | "float") {
        return oxsdatatypes::Double::from_str(v).ok().map(|d| Number::Approx(d.into()));
    }
    None
}

pub fn compare(a: &Term, b: &Term) -> Option<Ordering> {
    let (Term::Literal(a), Term::Literal(b)) = (a, b) else { return None };
    match (numeric(a), numeric(b)) {
        (Some(Number::Exact(x)), Some(Number::Exact(y))) => return Some(x.cmp(&y)),
        (Some(x), Some(y)) => return x.approx().partial_cmp(&y.approx()),
        _ => {}
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

#[cfg(test)]
mod tests {
    use super::{compare, well_formed};
    use oxrdf::{Literal, Term, vocab::xsd};
    use std::cmp::Ordering;

    #[test]
    fn an_integer_has_one_sign_at_most() {
        let int = |v: &str| well_formed(&Literal::new_typed_literal(v, xsd::INTEGER));
        assert!(int("5") && int("+5") && int("-5"));
        assert!(!int("++5") && !int("+-5") && !int("") && !int("+"));
        let dec = |v: &str| well_formed(&Literal::new_typed_literal(v, xsd::DECIMAL));
        assert!(dec("0.1234567890123456789") && dec("-1.5") && dec("123456789012345678901234567890"));
        assert!(!dec("1.2.3") && !dec("inf") && !dec("."));
        let big = "1234567890123456789012345678901234567890";
        let typed = |v: &str, t: &str| well_formed(&Literal::new_typed_literal(v, oxrdf::NamedNode::new_unchecked(format!("http://www.w3.org/2001/XMLSchema#{t}"))));
        assert!(int(big) && typed(big, "nonNegativeInteger") && typed(&format!("-{big}"), "negativeInteger"));
        assert!(!typed(&format!("-{big}"), "nonNegativeInteger") && !typed(big, "long"));
    }

    #[test]
    fn a_pattern_is_checked_whatever_its_flags_and_one_that_cannot_compile_is_an_error() {
        let shape = |pattern: &str, flags: &str| format!(
            "@prefix sh: <http://www.w3.org/ns/shacl#> . <https://example.org/s> a sh:NodeShape ; sh:targetNode <https://example.org/n> ; sh:property [ sh:path <https://example.org/p> ; sh:pattern {pattern:?} {flags} ] .\n<https://example.org/n> <https://example.org/p> \"xyz\" ."
        );
        let results = |ttl: &str| {
            let mut g = oxrdf::Graph::new();
            for t in oxttl::TurtleParser::new().for_slice(ttl.as_bytes()) {
                g.insert(&t.unwrap());
            }
            let shapes = super::Shapes::parse(&g);
            (super::Validator::new(&shapes, &g).validate_all().len(), shapes.problems.len())
        };
        assert_eq!(results(&shape("a.c", "")), (1, 0));
        assert_eq!(results(&shape("a.c", "; sh:flags \"q\"")), (1, 0));
        // With `q`, `x` does nothing: the space stays in the quoted pattern.
        assert_eq!(results(&shape("x yz", "; sh:flags \"qx\"")), (1, 0));
        // XPath's `x`: whitespace outside a class goes, `#` is a character, `[ ]` keeps its space.
        assert_eq!(results(&shape("x y z", "; sh:flags \"x\"")), (0, 0));
        assert_eq!(results(&shape("^x#z$", "; sh:flags \"x\"")), (1, 0));
        assert_eq!(results(&shape("^x[ ]z$", "; sh:flags \"x\"")), (1, 0));
        assert_eq!(results(&shape("^(?=x)y", "")).1, 1);
    }

    #[test]
    fn a_dot_matches_no_line_end_without_the_s_flag() {
        assert!(!super::xpath_regex("^a.b$", "").unwrap().is_match("a\rb"));
        assert!(super::xpath_regex("^a.b$", "s").unwrap().is_match("a\rb"));
        assert!(super::xpath_regex("^a[.]b$", "").unwrap().is_match("a.b"));
        assert!(super::xpath_regex("^a\\.b$", "").unwrap().is_match("a.b"));
    }

    #[test]
    fn word_and_space_escapes_and_class_operators_mean_what_xpath_says() {
        let m = |p: &str, v: &str| super::xpath_regex(p, "").unwrap().is_match(v);
        assert!(m("^\\w+$", "a+b") && !m("^\\w+$", "a,b") && !m("^\\w$", " "));
        assert!(m("^\\W$", ",") && !m("^\\W$", "+"));
        assert!(m("^\\s$", "\t") && !m("^\\s$", "\u{a0}") && m("^\\S$", "\u{a0}"));
        assert!(m("^[\\s,]$", ",") && m("^[\\w-]+$", "a-b"));
        assert!(m("^[a&&b]$", "&") && m("^[a~~b]$", "~") && !m("^[a&&b]$", "c"));
    }

    #[test]
    fn a_class_subtraction_subtracts() {
        let re = super::xpath_regex("^[a-z-[aeiou]]+$", "x").unwrap();
        assert!(re.is_match("b") && !re.is_match("a") && !re.is_match("-"));
        // A dash before a plain class is still a range end or a character.
        assert!(super::xpath_regex("^[a\\-]$", "").unwrap().is_match("-"));
        // `x` keeps a space inside the subtracted class, which is still open after its `]`.
        let spaced = super::xpath_regex("^[a-z -[aeiou]]$", "x").unwrap();
        assert!(spaced.is_match(" ") && !spaced.is_match("e"));
    }

    #[test]
    fn a_pattern_violation_quotes_the_pattern_as_written() {
        let ttl = "@prefix sh: <http://www.w3.org/ns/shacl#> . <https://example.org/s> a sh:NodeShape ; sh:targetNode <https://example.org/n> ; sh:property [ sh:path <https://example.org/p> ; sh:pattern \"^a.b$\" ] .\n<https://example.org/n> <https://example.org/p> \"xyz\" .";
        let mut g = oxrdf::Graph::new();
        for t in oxttl::TurtleParser::new().for_slice(ttl.as_bytes()) {
            g.insert(&t.unwrap());
        }
        let shapes = super::Shapes::parse(&g);
        let found = super::Validator::new(&shapes, &g).validate_all();
        assert!(found[0].message.ends_with("does not match ^a.b$"), "{}", found[0].message);
    }

    #[test]
    fn integers_and_decimals_compare_exactly() {
        let n = |v: &str, dt| Term::from(Literal::new_typed_literal(v, dt));
        assert_eq!(compare(&n("9007199254740993", xsd::INTEGER), &n("9007199254740992", xsd::INTEGER)), Some(Ordering::Greater));
        assert_eq!(compare(&n("0.100000000000000001", xsd::DECIMAL), &n("0.1", xsd::DECIMAL)), Some(Ordering::Greater));
        assert_eq!(compare(&n("123456789012345678901234567890", xsd::INTEGER), &n("5", xsd::INTEGER)), Some(Ordering::Greater));
        assert_eq!(compare(&n("2", xsd::INTEGER), &n("2.5", xsd::DOUBLE)), Some(Ordering::Less));
        assert_eq!(compare(&n("inf", xsd::INTEGER), &n("5", xsd::INTEGER)), None);
        assert_eq!(compare(&n("INF", xsd::DOUBLE), &n("5", xsd::INTEGER)), Some(Ordering::Greater));
    }
}
