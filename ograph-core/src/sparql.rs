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
    Ok(parser.parse_query(query)?)
}

pub fn show(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => vocab::compact(n.as_str()),
        Term::Literal(l) if l.datatype() == oxrdf::vocab::xsd::STRING => l.value().to_owned(),
        Term::Literal(l) if l.datatype() == oxrdf::vocab::xsd::DATE_TIME || l.datatype() == oxrdf::vocab::xsd::INTEGER || l.datatype() == oxrdf::vocab::xsd::DECIMAL => l.value().to_owned(),
        other => other.to_string(),
    }
}

/// Rejects a query that names a class or property in one of the ontology's namespaces that the
/// ontology does not declare (a typo returns no rows and looks like an answer), suggesting the
/// nearest declared name.
pub fn check_names(query: &str, ont: &crate::ontology::Ontology) -> anyhow::Result<()> {
    static IRI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = IRI.get_or_init(|| regex::Regex::new(r"<(https://ograph\.dev/[^>\s]+)>").unwrap());
    let schema = [vocab::ops::NS, vocab::evt::NS, vocab::inc::NS, vocab::disc::NS, vocab::corr::NS, vocab::rca::NS, vocab::harv::NS];
    let declared: std::collections::BTreeSet<String> =
        ont.graph.iter().filter_map(|t| match t.subject {
            oxrdf::NamedOrBlankNodeRef::NamedNode(n) => Some(n.as_str().to_owned()),
            _ => None,
        }).collect();
    let text = parse(query)?.to_string();
    let mut unknown: Vec<String> = Vec::new();
    for c in re.captures_iter(&text) {
        let iri = &c[1];
        if !schema.iter().any(|ns| iri.starts_with(ns)) || declared.contains(iri) {
            continue;
        }
        let name = vocab::compact(iri);
        let local = |s: &str| s.rsplit([':', '#', '/']).next().unwrap_or(s).to_lowercase();
        let nearest = declared
            .iter()
            .map(|d| vocab::compact(d))
            .map(|d| (strsim::jaro_winkler(&local(&name), &local(&d)), d))
            .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
            .filter(|(s, _)| *s >= 0.8);
        let msg = match nearest {
            Some((_, d)) => format!("{name} (did you mean {d}?)"),
            None => name,
        };
        if !unknown.contains(&msg) {
            unknown.push(msg);
        }
    }
    anyhow::ensure!(unknown.is_empty(), "the ontology declares no {}; a query with an undeclared name matches nothing", unknown.join(", "));
    Ok(())
}

/// Runs a query and collects the answer. `limit` caps solution rows.
pub fn run(snap: &Snapshot, query: &str, limit: Option<usize>) -> anyhow::Result<Answer> {
    let parsed = parse(query)?;
    let evaluator = QueryEvaluator::new();
    let view = View::new(snap);
    Ok(match evaluator.prepare(&parsed).execute(view)? {
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
