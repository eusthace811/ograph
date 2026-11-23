//! SPARQL over the store through `spareval::QueryableDataset`. This module is the only place that
//! touches spareval, so a breaking change in the pinned 0.x crates lands here alone.

use crate::store::{GraphSel, Result as StoreResult, Snapshot, corrupted};
use crate::term::{self, EPHEMERAL, Id};
use crate::vocab;
use oxrdf::{Term, Triple, Variable};
use spareval::{ExpressionTerm, InternalQuad, QueryEvaluator, QueryResults, QueryableDataset};
use spargebra::SparqlParser;
use std::cell::RefCell;
use rustc_hash::FxHashMap;

/// One query's view: the snapshot plus query-time terms that are not in the dictionary.
pub struct View<'a> {
    snap: &'a Snapshot,
    extra: RefCell<(Vec<Term>, FxHashMap<Term, Id>)>,
    memo: RefCell<FxHashMap<Id, ExpressionTerm>>,
}

impl<'a> View<'a> {
    pub fn new(snap: &'a Snapshot) -> Self {
        Self { snap, extra: RefCell::default(), memo: RefCell::default() }
    }
}

impl<'a> QueryableDataset<'a> for View<'a> {
    type InternalTerm = Id;
    type Error = redb::Error;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&Id>,
        predicate: Option<&Id>,
        object: Option<&Id>,
        graph_name: Option<Option<&Id>>,
    ) -> impl Iterator<Item = StoreResult<InternalQuad<Id>>> + use<'a> {
        let spo = [subject.copied(), predicate.copied(), object.copied()];
        let sel = match graph_name {
            Some(None) => GraphSel::Union,
            Some(Some(&g)) => GraphSel::Named(g),
            None => GraphSel::AnyNamed,
        };
        let snap: &'a Snapshot = self.snap;
        snap.quads(spo, sel).map(move |q| {
            q.map(|[s, p, o, g]| InternalQuad {
                subject: s,
                predicate: p,
                object: o,
                graph_name: (sel != GraphSel::Union).then_some(g),
            })
        })
    }

    fn internal_named_graphs(&self) -> impl Iterator<Item = StoreResult<Id>> + use<'a> {
        let graphs = self.snap.graphs().map(|g| g.into_iter().map(|(g, _)| g).collect::<Vec<_>>());
        let (ok, err) = match graphs {
            Ok(g) => (g, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        err.map(Err).into_iter().chain(ok.into_iter().map(Ok))
    }

    fn contains_internal_graph_name(&self, g: &Id) -> StoreResult<bool> {
        Ok(self.snap.graphs()?.iter().any(|(id, _)| id == g))
    }

    fn internalize_term(&self, term: Term) -> StoreResult<Id> {
        if let Some(id) = self.snap.id(&term)? {
            return Ok(id);
        }
        let (terms, ids) = &mut *self.extra.borrow_mut();
        Ok(*ids.entry(term).or_insert_with_key(|t| {
            terms.push(t.clone());
            EPHEMERAL | (terms.len() as Id - 1)
        }))
    }

    fn externalize_term(&self, id: Id) -> StoreResult<Term> {
        if id & EPHEMERAL != 0 {
            return self.extra.borrow().0.get((id & !EPHEMERAL) as usize).cloned().ok_or_else(|| corrupted("bad ephemeral id"));
        }
        self.snap.term(id)
    }

    fn externalize_expression_term(&self, id: Id) -> StoreResult<ExpressionTerm> {
        if id & EPHEMERAL == 0
            && let Some(t) = term::inline_expression(id) {
                return Ok(t);
            }
        if let Some(t) = self.memo.borrow().get(&id) {
            return Ok(t.clone());
        }
        let t = ExpressionTerm::from(self.externalize_term(id)?);
        self.memo.borrow_mut().insert(id, t.clone());
        Ok(t)
    }

    fn internalize_expression_term(&self, term: ExpressionTerm) -> StoreResult<Id> {
        if let ExpressionTerm::DateTimeLiteral(dt) = &term
            && let Some(m) = term::millis_of(*dt) {
                return Ok(term::time_id(m));
            }
        self.internalize_term(term.into())
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Answer {
    Solutions { variables: Vec<String>, rows: Vec<Vec<Option<String>>> },
    Boolean { value: bool },
    Graph { triples: Vec<[String; 3]> },
}

pub fn parse(query: &str) -> anyhow::Result<spargebra::Query> {
    let mut parser = SparqlParser::new();
    for (p, ns) in vocab::PREFIXES {
        parser = parser.with_prefix(*p, *ns)?;
    }
    parser.clone().parse_query(query).or_else(|e| {
        anyhow::ensure!(parser.parse_update(query).is_err(), "read-only: SPARQL updates are not allowed");
        Err(e.into())
    })
}

pub fn show(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => vocab::compact(n.as_str()),
        Term::Literal(l) if l.datatype() == oxrdf::vocab::xsd::STRING => l.value().to_owned(),
        Term::Literal(l) if l.datatype() == oxrdf::vocab::xsd::DATE_TIME || l.datatype() == oxrdf::vocab::xsd::INTEGER || l.datatype() == oxrdf::vocab::xsd::DECIMAL => l.value().to_owned(),
        other => other.to_string(),
    }
}

/// Text outside the string literals of a query's canonical serialization, where spargebra
/// writes every literal in double quotes, escaping `"` and backslash.
fn outside_literals(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in text.char_indices() {
        match (quoted, escaped, c) {
            (true, true, _) => escaped = false,
            (true, false, '\\') => escaped = true,
            (true, false, '"') => (quoted, start) = (false, i + 1),
            (false, _, '"') => {
                out.push(&text[start..i]);
                quoted = true;
            }
            _ => {}
        }
    }
    if !quoted {
        out.push(&text[start..]);
    }
    out
}

/// Parses a query and rejects ontology names it cannot match: a class or property in one of the
/// ontology's namespaces that it does not declare (a typo returns no rows and looks like an
/// answer), with the nearest declared name, and a class with subclasses, since the store holds
/// only each node's most specific type.
pub fn check(query: &str, ont: &crate::ontology::Ontology) -> anyhow::Result<spargebra::Query> {
    static TOKEN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = TOKEN.get_or_init(|| regex::Regex::new(r"<[^<>\s]*>|[?$]\w+|_:\w+|\w+|\S").unwrap());
    let schema = [vocab::ops::NS, vocab::evt::NS, vocab::inc::NS, vocab::disc::NS, vocab::corr::NS, vocab::rca::NS, vocab::harv::NS];
    let declared: std::collections::BTreeSet<String> =
        ont.graph.iter().filter_map(|t| match t.subject {
            oxrdf::NamedOrBlankNodeRef::NamedNode(n) => Some(n.as_str().to_owned()),
            _ => None,
        }).collect();
    let parsed = parse(query)?;
    let text = parsed.to_string();
    let (mut unknown, mut abstract_): (Vec<String>, Vec<String>) = Default::default();
    let segments: Vec<Vec<&str>> = outside_literals(&text).into_iter().map(|t| re.find_iter(t).map(|m| m.as_str()).collect()).collect();
    let iris = segments.iter().flat_map(|t| {
        t.iter().enumerate().filter_map(move |(i, x)| {
            let iri = x.strip_prefix('<')?.strip_suffix('>')?;
            iri.starts_with("https://ograph.dev/").then(|| (iri, matched_as_type(t, i)))
        })
    });
    for (iri, typed) in iris {
        let name = vocab::compact(iri);
        let is_leaf = |c: &String| !ont.classes.iter().any(|(d, sups)| d != c && sups.contains(c));
        let leaves: Vec<String> = ont.classes.iter().filter(|(c, sups)| typed && *c != iri && sups.contains(iri) && is_leaf(c)).map(|(c, _)| vocab::compact(c)).collect();
        let (list, msg) = if !leaves.is_empty() {
            (&mut abstract_, format!("{name} has subclasses, and each node stores only its most specific type, so match those: VALUES ?type {{ {} }} ?x a ?type", leaves.join(" ")))
        } else if !schema.iter().any(|ns| iri.starts_with(ns)) || declared.contains(iri) {
            continue;
        } else {
            let local = |s: &str| s.rsplit([':', '#', '/']).next().unwrap_or(s).to_lowercase();
            let nearest = declared
                .iter()
                .map(|d| vocab::compact(d))
                .map(|d| (strsim::jaro_winkler(&local(&name), &local(&d)), d))
                .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
                .filter(|(s, _)| *s >= 0.8);
            (&mut unknown, match nearest {
                Some((_, d)) => format!("{name} (did you mean {d}?)"),
                None => name,
            })
        };
        if !list.contains(&msg) {
            list.push(msg);
        }
    }
    anyhow::ensure!(unknown.is_empty(), "the ontology declares no {}; a query with an undeclared name matches nothing", unknown.join(", "));
    anyhow::ensure!(abstract_.is_empty(), "{}", abstract_.join("; "));
    Ok(parsed)
}

/// Whether the term at `t[i]`, a token of a canonical query, is the object of a predicate path
/// through `rdf:type` or `rdfs:subClassOf`: where a class with subclasses matches nothing.
fn matched_as_type(t: &[&str], i: usize) -> bool {
    let mut j = i;
    while j > 0 && matches!(t[j - 1], "*" | "+" | "?") {
        j -= 1;
    }
    let mut depth = 0;
    loop {
        let Some(k) = j.checked_sub(1) else { return false };
        j = k;
        match t[j] {
            "(" if depth == 0 => return false,
            "(" => depth -= 1,
            ")" => depth += 1,
            _ => {}
        }
        if depth == 0 {
            break;
        }
    }
    while j > 0 && t[j - 1] == "^" {
        j -= 1;
    }
    let subject = j.checked_sub(1).is_some_and(|k| ["?", "$", "_:", "<"].iter().any(|p| t[k].starts_with(p)));
    let typing = ["<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>", "<http://www.w3.org/2000/01/rdf-schema#subClassOf>"];
    subject && t[j..i].iter().any(|x| typing.contains(x))
}

/// Runs a parsed query and collects the answer. `limit` caps solution rows.
pub fn run(snap: &Snapshot, parsed: &spargebra::Query, limit: Option<usize>) -> anyhow::Result<Answer> {
    let evaluator = QueryEvaluator::new();
    let view = View::new(snap);
    Ok(match evaluator.prepare(parsed).execute(view)? {
        QueryResults::Solutions(sols) => {
            let variables: Vec<String> = sols.variables().iter().map(|v| v.as_str().to_owned()).collect();
            let vars: Vec<Variable> = sols.variables().to_vec();
            let mut rows = Vec::new();
            for s in sols.take(limit.unwrap_or(usize::MAX)) {
                let s = s?;
                rows.push(vars.iter().map(|v| s.get(v).map(show)).collect());
            }
            Answer::Solutions { variables, rows }
        }
        QueryResults::Boolean(value) => Answer::Boolean { value },
        QueryResults::Graph(triples) => Answer::Graph {
            triples: triples
                .map(|t| t.map(|t: Triple| [show(&t.subject.into()), show(&t.predicate.into()), show(&t.object)]))
                .collect::<Result<_, _>>()?,
        },
    })
}

/// Raw solutions as terms, for tests and the correctness gate.
pub fn solutions(snap: &Snapshot, query: &str) -> anyhow::Result<Vec<Vec<Option<Term>>>> {
    let parsed = parse(query)?;
    let evaluator = QueryEvaluator::new();
    let QueryResults::Solutions(sols) = evaluator.prepare(&parsed).execute(View::new(snap))? else {
        anyhow::bail!("not a SELECT query");
    };
    let vars: Vec<Variable> = sols.variables().to_vec();
    sols.map(|s| s.map(|s| vars.iter().map(|v| s.get(v).cloned()).collect()).map_err(Into::into)).collect()
}

#[cfg(test)]
mod tests {
    use super::check;

    #[test]
    fn check_reads_names_not_literals() {
        let ont = crate::ontology::Ontology::load_default().unwrap();
        let err = |q: &str| check(q, &ont).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err(r#"SELECT ("<https://ograph.dev/ops#typo> \" <https://ograph.dev/ops#typo2>" AS ?t) WHERE {}"#), "");
        assert_eq!(err("SELECT ?x WHERE { ?x evt:severty ?s FILTER(?s < 2) }"), "the ontology declares no evt:severty (did you mean evt:severity?); a query with an undeclared name matches nothing");
        assert!(err("SELECT ?x WHERE { ?x a/rdfs:subClassOf* evt:Event }").starts_with("evt:Event has subclasses, and each node stores only its most specific type, so match those: VALUES ?type { evt:Alert "));
        assert_eq!(err("SELECT ?x WHERE { VALUES ?t { evt:Alert inc:Incident } ?x a ?t }"), "");
    }

    #[test]
    fn the_subclass_hint_applies_only_where_a_type_is_matched() {
        let ont = crate::ontology::Ontology::load_default().unwrap();
        let err = |q: &str| check(q, &ont).err().map(|e| e.to_string()).unwrap_or_default();
        for q in ["SELECT (evt:Event AS ?class) WHERE {}", "SELECT ?x WHERE { ?x evt:on ?a FILTER(?a != evt:Event) }", "SELECT ?x WHERE { VALUES (?a ?c) { (ops:Application evt:Event) } ?x a ?a }"] {
            assert_eq!(err(q), "", "{q}");
        }
        for q in ["SELECT ?x WHERE { ?x a evt:Event }", "SELECT ?x WHERE { ?x rdf:type evt:Event }", "SELECT ?x WHERE { ?x (a|evt:on)+ evt:Event }", "SELECT ?x WHERE { ?x evt:on ?a FILTER EXISTS { ?y ^evt:on/a evt:Event } }"] {
            assert!(err(q).starts_with("evt:Event has subclasses"), "{q}: {}", err(q));
        }
    }
}
