//! "Model proposes, telemetry confirms": how well each kind of support separates real
//! dependencies from invented ones. Every ordered pair of applications in every harvested case
//! is treated as a model-proposed `ops:dependsOn`; the catalog's declared edges are the truth.
//!
//! Usage: confirm_eval CASES_DIR

use ograph_core::gateway;
use ograph_core::store::Snapshot;
use ograph_core::term::{self, Id};
use ograph_core::vocab::{GRAPH, RDF_TYPE, ops};
use ograph_core::workspace::Workspace;
use oxrdf::{NamedNode, Term};
use std::collections::BTreeMap;

/// Distinct minutes with an event on `a` that has an event on `b` within a minute of it.
fn co_occurrences(snap: &Snapshot, a: Id, b: Id) -> usize {
    let times = |app| -> Vec<i64> {
        snap.events_on(app, term::time_id(0), term::time_id(4_102_444_800_000)).unwrap_or_default().into_iter().filter_map(|(t, _)| term::millis(t)).collect()
    };
    let (ta, tb) = (times(a), times(b));
    let mut minutes = std::collections::BTreeSet::new();
    let mut j = 0;
    for t in ta {
        while j < tb.len() && tb[j] < t - 60_000 {
            j += 1;
        }
        if j < tb.len() && tb[j] <= t + 60_000 {
            minutes.insert(t.div_euclid(60_000));
        }
    }
    minutes.len()
}

#[derive(Default)]
struct Counts {
    tp: usize,
    fp: usize,
    fn_: usize,
}

impl Counts {
    fn add(&mut self, accepted: bool, real: bool) {
        match (accepted, real) {
            (true, true) => self.tp += 1,
            (true, false) => self.fp += 1,
            (false, true) => self.fn_ += 1,
            _ => {}
        }
    }
    fn row(&self, name: &str) -> String {
        let p = self.tp as f64 / (self.tp + self.fp).max(1) as f64;
        let r = self.tp as f64 / (self.tp + self.fn_).max(1) as f64;
        format!("| {name} | {} | {} | {} | {p:.3} | {r:.3} |", self.tp, self.fp, self.fn_)
    }
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).expect("CASES_DIR");
    let mut rules: BTreeMap<String, Counts> = BTreeMap::new();
    let (mut cases, mut pairs, mut real_pairs) = (0, 0, 0);
    let thresholds = [1usize, 2, 3, 5, 8, 13, 21];
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).filter(|p| p.join(".ograph").exists()).collect();
    entries.sort();
    for case in entries {
        let ws = Workspace::open(&case)?;
        let snap = ws.store.snapshot()?;
        let (Some(ty), Some(app_c), Some(dep)) = (snap.iri(RDF_TYPE)?, snap.iri(ops::APPLICATION)?, snap.iri(ops::DEPENDS_ON)?) else { continue };
        let apps: Vec<NamedNode> = snap.subjects(ty, app_c)?.into_iter().filter_map(|a| match snap.term(a) {
            Ok(Term::NamedNode(n)) => Some(n),
            _ => None,
        }).collect();
        let catalog_prefix = format!("{GRAPH}harvest/catalog/");
        let in_source = |a: &NamedNode, b: &NamedNode, prefix: &str| -> bool {
            let (Some(x), Some(y)) = (snap.id(&a.clone().into()).ok().flatten(), snap.id(&b.clone().into()).ok().flatten()) else { return false };
            snap.graphs_of([x, dep, y]).unwrap_or_default().iter().any(|g| matches!(snap.term(*g), Ok(Term::NamedNode(n)) if n.as_str().starts_with(prefix)))
        };
        let p = NamedNode::new_unchecked(ops::DEPENDS_ON);
        cases += 1;
        for a in &apps {
            for b in &apps {
                if a == b {
                    continue;
                }
                pairs += 1;
                let real = in_source(a, b, &catalog_prefix);
                real_pairs += real as usize;
                let real_undirected = real || in_source(b, a, &catalog_prefix);
                let traces = in_source(a, b, &format!("{GRAPH}harvest/traces/"));
                let ids = (snap.id(&a.clone().into())?, snap.id(&b.clone().into())?);
                let co = match ids {
                    (Some(x), Some(y)) => co_occurrences(&snap, x, y),
                    _ => 0,
                };
                rules.entry("1 trace edge only".into()).or_default().add(traces, real);
                for k in thresholds {
                    rules.entry(format!("2 co-occurrence >= {k:02} (directed truth)")).or_default().add(co >= k, real);
                    rules.entry(format!("3 co-occurrence >= {k:02} (either direction is truth)")).or_default().add(co >= k, real_undirected);
                    rules.entry(format!("4 trace edge, else co-occurrence >= {k:02}")).or_default().add(traces || co >= k, real);
                }
                let gate = gateway::supported_by(&snap, a, &p, b, None);
                rules.entry("5 gateway as shipped (catalog or trace edge)".into()).or_default().add(gate.is_some(), real);
            }
        }
    }
    println!("{cases} cases, {pairs} ordered application pairs proposed as ops:dependsOn, {real_pairs} declared in the catalog\n");
    println!("| support rule | accepted real | accepted invented | rejected real | precision | recall |\n|---|---|---|---|---|---|");
    for (name, c) in &rules {
        println!("{}", c.row(name));
    }
    Ok(())
}
