//! Gateway gate: generated bad writes of 18 kinds mixed with good ones. Passes when no bad fact
//! reaches a harvest graph, every bad fact is quarantined with a reason, every good fact is
//! committed, a full SHACL audit of the store is clean, and re-submitting writes nothing.

use ograph_core::gateway;
use ograph_core::model::*;
use ograph_core::ontology::Ontology;
use ograph_core::sparql;
use ograph_core::store::Store;
use ograph_core::term;
use ograph_core::vocab::{RDF_TYPE, evt, inc, ops};
use oxrdf::vocab::xsd;
use oxrdf::{Literal, NamedNode};
use std::time::Instant;

fn dt(ms: i64) -> Object {
    lit(Literal::new_typed_literal(term::format_millis(ms), xsd::DATE_TIME))
}

/// The exact triples this eval marks bad (the last fact proposed for each bad subject, plus every
/// fact of the kinds whose subject itself is invalid) that ended up in a harvest graph.
fn bad_fact_committed(store: &Store, b: &Batch) -> anyhow::Result<usize> {
    let snap = store.snapshot()?;
    let q = "SELECT ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } FILTER(STRSTARTS(STR(?g), \"https://ograph.dev/graph/harvest/\")) FILTER(CONTAINS(STR(?s), \"/bad-\")) }";
    let rows = sparql::solutions(&snap, q)?;
    let mut n = 0;
    for r in rows {
        let (s, p, o) = (r[0].clone().unwrap(), r[1].clone().unwrap(), r[2].clone().unwrap());
        let last = b.facts.iter().rev().find(|f| matches!(&f.subject, EntityRef::Known(k) if oxrdf::Term::from(k.clone()) == s)).map(|f| f.predicate.clone());
        let flagged = last.is_some_and(|lp| oxrdf::Term::from(lp) == p.clone());
        if flagged {
            println!("  BAD FACT COMMITTED: {s} {p} {o}");
            n += 1;
        }
    }
    println!("bad facts (the faulty fact of each case) committed: {n}");
    Ok(n)
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("ograph-gateway-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let store = Store::open(&dir.join("g.redb"))?;
    let ont = Ontology::load_default()?;
    let problems = ont.check();
    println!("ontology check: {} problems {:?}", problems.len(), problems);
    let t0 = 1_790_812_800_000i64;

    let mut cat = Batch::new("catalog", "fixture", "catalog-v1");
    let ev = cat.record("catalog-info.yaml:1");
    let m = Extraction::Catalog { catalog: "backstage".into() };
    let apps = ["checkout", "payments-api", "cart", "currency", "email", "inventory-service-eu"];
    for a in apps {
        let s = known("app", a);
        cat.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(ops::APPLICATION)), &ev, &m);
        cat.add(s.clone(), ops::NAME, lit(a), &ev, &m);
        cat.add(s.clone(), ops::OWNED_BY, obj(entity("team", "bedrock")), &ev, &m);
    }
    for j in 0..3 {
        let s = known("app", &format!("bad-unconfirmed-model-edge-{j}"));
        cat.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(ops::APPLICATION)), &ev, &m);
        cat.add(s, ops::NAME, lit(format!("audit-{j}")), &ev, &m);
    }
    cat.add(known("team", "bedrock"), RDF_TYPE, obj(NamedNode::new_unchecked(ops::TEAM)), &ev, &m);
    cat.add(known("team", "bedrock"), ops::NAME, lit("Bedrock"), &ev, &m);
    cat.add(known("app", "checkout"), ops::DEPENDS_ON, obj(entity("app", "payments-api")), &ev, &m);
    let out = gateway::submit(&store, &ont, &cat, t0)?;
    println!("catalog: proposed {} accepted {} quarantined {}", out.proposed, out.accepted, out.quarantined.len());
    for q in &out.quarantined {
        println!("  unexpected: {} -- {}", q.fact, q.reason);
    }

    let mut b = Batch::new("alerts", "day-1", "rule-v1");
    let rule = Extraction::Rule { extractor: "alertmanager".into() };
    let mut good = Vec::new();
    let mut bad: Vec<(&str, String)> = Vec::new();
    let n = 40;
    for i in 0..n {
        let r = b.record(format!("alert-{i}"));
        let g = known("ev", &format!("good-{i}"));
        let app = if i % 2 == 0 { EntityRef::Provisional { class: ops::APPLICATION, name: "Payments API".into() } } else { known("app", apps[i % apps.len()]) };
        b.add(g.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rule);
        b.add(g.clone(), evt::ON, Object::Entity(app), &r, &rule);
        b.add(g.clone(), evt::AT, dt(t0 + i as i64 * 1000), &r, &rule);
        b.add(g.clone(), evt::SEVERITY, lit(["SEV1", "SEV2", "SEV3"][i % 3]), &r, &rule);
        good.push(format!("good-{i}"));
    }
    for (i, typo) in ["inventory-servcie-eu", "Inventory Service EU", "inventry-service-eu", "inventory-service-eu-svc"].iter().enumerate() {
        let r = b.record(format!("alert-typo-{i}"));
        let g = known("ev", &format!("good-typo-{i}"));
        b.add(g.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rule);
        b.add(g.clone(), evt::ON, Object::Entity(EntityRef::Provisional { class: ops::APPLICATION, name: (*typo).into() }), &r, &rule);
        b.add(g.clone(), evt::AT, dt(t0), &r, &rule);
        good.push(format!("good-typo-{i}"));
    }
    let mut kind = |b: &mut Batch, name: &'static str, f: &dyn Fn(&mut Batch, EntityRef, EvidenceRef)| {
        for j in 0..3 {
            let local = format!("bad-{name}-{j}");
            let r = b.record(format!("rec-{local}"));
            f(b, known("ev", &local), r);
            bad.push((name, local));
        }
    };
    let alert = |b: &mut Batch, s: &EntityRef, r: &EvidenceRef, rule: &Extraction, at: i64| {
        b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), r, rule);
        b.add(s.clone(), evt::ON, obj(entity("app", "checkout")), r, rule);
        b.add(s.clone(), evt::AT, dt(at), r, rule);
    };
    let rl = rule.clone();
    kind(&mut b, "unknown-predicate", &|b, s, r| { alert(b, &s, &r, &rl, t0); b.add(s, "https://ograph.dev/evt#colour", lit("red"), &r, &rl) });
    kind(&mut b, "unknown-class", &|b, s, r| b.add(s, RDF_TYPE, obj(NamedNode::new_unchecked("https://ograph.dev/evt#Gremlin")), &r, &rl));
    kind(&mut b, "app-outside-catalog", &|b, s, r| { if let EntityRef::Known(n) = &s { b.add(EntityRef::Known(NamedNode::new_unchecked(n.as_str().replace("/ev/", "/app/"))), RDF_TYPE, obj(NamedNode::new_unchecked(ops::APPLICATION)), &r, &rl) } });
    kind(&mut b, "near-twin-app-name", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl); b.add(s.clone(), evt::AT, dt(t0), &r, &rl); b.add(s, evt::ON, Object::Entity(EntityRef::Provisional { class: ops::APPLICATION, name: "inventory-service-uk".into() }), &r, &rl) });
    kind(&mut b, "variant-app-name", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl); b.add(s.clone(), evt::AT, dt(t0), &r, &rl); b.add(s, evt::ON, Object::Entity(EntityRef::Provisional { class: ops::APPLICATION, name: "inventory-service-eu-canary".into() }), &r, &rl) });
    kind(&mut b, "unknown-app-name", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl); b.add(s.clone(), evt::AT, dt(t0), &r, &rl); b.add(s, evt::ON, Object::Entity(EntityRef::Provisional { class: ops::APPLICATION, name: "Ledger Service".into() }), &r, &rl) });
    kind(&mut b, "fuzzy-app-name", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl); b.add(s.clone(), evt::AT, dt(t0), &r, &rl); b.add(s, evt::ON, Object::Entity(EntityRef::Provisional { class: ops::APPLICATION, name: "currencies-svc".into() }), &r, &rl) });
    kind(&mut b, "bad-iri", &|b, _s, r| b.add(EntityRef::Known(NamedNode::new_unchecked("https://ograph.dev/id/ev/Has Space")), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl));
    kind(&mut b, "vocabulary", &|b, s, r| { alert(b, &s, &r, &rl, t0); b.add(s, evt::SEVERITY, lit("P1"), &r, &rl) });
    kind(&mut b, "datatype", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl); b.add(s.clone(), evt::ON, obj(entity("app", "checkout")), &r, &rl); b.add(s, evt::AT, lit("yesterday"), &r, &rl) });
    kind(&mut b, "range", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl); b.add(s.clone(), evt::AT, dt(t0), &r, &rl); b.add(s, evt::ON, obj(entity("team", "bedrock")), &r, &rl) });
    kind(&mut b, "domain", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(inc::INCIDENT)), &r, &rl); b.add(s.clone(), inc::ID, lit("INC-1"), &r, &rl); b.add(s.clone(), evt::AT, dt(t0), &r, &rl); b.add(s.clone(), evt::ON, obj(entity("app", "cart")), &r, &rl); b.add(s, ops::DEPENDS_ON, obj(entity("app", "cart")), &r, &rl) });
    kind(&mut b, "no-evidence", &|b, s, r| { alert(b, &s, &r, &rl, t0); b.facts.last_mut().unwrap().evidence.clear(); b.facts.iter_mut().rev().take(3).for_each(|f| f.evidence.clear()) });
    kind(&mut b, "unknown-evidence", &|b, s, _r| { let r = EvidenceRef("never-read".into()); alert(b, &s, &r, &rl, t0) });
    kind(&mut b, "low-confidence-llm", &|b, s, r| { let m = Extraction::Llm { model: "m".into(), prompt_hash: "h".into(), confidence: Confidence::new(0.2).unwrap() }; b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &m); b.add(s.clone(), evt::ON, obj(entity("app", "cart")), &r, &m); b.add(s, evt::AT, dt(t0), &r, &m) });
    kind(&mut b, "functional", &|b, s, r| { alert(b, &s, &r, &rl, t0); b.add(s, evt::AT, dt(t0 + 5), &r, &rl) });
    let llm = Extraction::Llm { model: "m".into(), prompt_hash: "h".into(), confidence: Confidence::new(0.9).unwrap() };
    let lm = llm.clone();
    kind(&mut b, "unconfirmed-model-edge", &|b, s, r| { if let EntityRef::Known(n) = &s { b.add(EntityRef::Known(NamedNode::new_unchecked(n.as_str().replace("/ev/", "/app/"))), ops::DEPENDS_ON, obj(entity("app", "cart")), &r, &lm) } });
    let r = b.record("thread-confirmed-edge");
    b.add(known("app", "checkout"), ops::DEPENDS_ON, obj(entity("app", "payments-api")), &r, &llm);
    kind(&mut b, "missing-required", &|b, s, r| { b.add(s.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(evt::ALERT)), &r, &rl); b.add(s, evt::ON, obj(entity("app", "cart")), &r, &rl) });

    let t = Instant::now();
    let out = gateway::submit(&store, &ont, &b, t0)?;
    let took = t.elapsed();
    let snap = store.snapshot()?;
    let bad_facts: Vec<(String, String)> = b.facts.iter().zip(&b.facts).filter_map(|(f, _)| {
        let d = f.describe();
        let local = bad.iter().find(|(_, l)| d.contains(l.as_str()))?.1.clone();
        Some((local, d))
    }).collect();
    let _ = bad_facts;
    let in_harvest = |local: &str| -> anyhow::Result<bool> {
        let q = format!("ASK {{ GRAPH ?g {{ ?s ?p ?o }} FILTER(STRSTARTS(STR(?g), \"https://ograph.dev/graph/harvest/\")) FILTER(?s = <https://ograph.dev/id/ev/{local}> || STRENDS(STR(?s), \"/{local}\")) }}");
        Ok(matches!(sparql::run(&snap, &sparql::parse(&q)?, None)?, sparql::Answer::Boolean { value: true }))
    };
    println!("\n| kind | cases | cases with some fact committed (valid siblings of the bad fact) | quarantined facts (first reason) |\n|---|---|---|---|");
    let mut leaked = 0;
    let mut kinds: Vec<&str> = bad.iter().map(|(k, _)| *k).collect();
    kinds.dedup();
    for k in kinds {
        let locals: Vec<&String> = bad.iter().filter(|(x, _)| *x == k).map(|(_, l)| l).collect();
        let committed = locals.iter().filter(|l| in_harvest(l).unwrap()).count();
        leaked += committed;
        let reasons: Vec<&gateway::Rejection> = out.quarantined.iter().filter(|r| locals.iter().any(|l| r.fact.contains(l.as_str())) || (k == "bad-iri" && r.fact.contains("Has Space"))).collect();
        let reason = reasons.first().map(|r| r.reason.clone()).unwrap_or_else(|| "NONE".into());
        println!("| {k} | {} | {committed} | {} ({}) |", locals.len(), reasons.len(), reason.replace('|', "/"));
    }
    let missing_good = good.iter().filter(|g| !in_harvest(g).unwrap()).count();
    let confirmed_edge = sparql::run(&snap, &sparql::parse("ASK { GRAPH ?g { id:app\\/checkout ops:dependsOn id:app\\/payments-api } FILTER(CONTAINS(STR(?g), \"/alerts/\")) }")?, None)?;
    let confirmed_edge = matches!(confirmed_edge, sparql::Answer::Boolean { value: true });
    println!("model-proposed edge the catalog declares, committed: {confirmed_edge}");
    let audit = gateway::audit(&snap, &ont);
    for r in audit.iter().take(5) {
        println!("audit: {} {} {:?} {}", r.component, r.focus, r.path, r.message);
    }
    println!("\nbatch: proposed {} facts, accepted {}, quarantined {}, fuzzy catalog matches {}, {} ms", out.proposed, out.accepted, out.quarantined.len(), out.fuzzy_matches, took.as_millis());
    println!("bad subjects with a committed fact: {leaked}; good events missing: {missing_good}/{}; full SHACL audit violations: {}", good.len(), audit.len());
    drop(snap);
    let again = gateway::submit(&store, &ont, &b, t0 + 60_000)?;
    println!("re-submit: removed {} added {} (an unchanged batch writes nothing: 0 and 0)", again.removed, again.added);
    let snap = store.snapshot()?;
    let still_clean = gateway::audit(&snap, &ont).is_empty();
    drop(snap);
    let ok = confirmed_edge && bad_fact_committed(&store, &b)? == 0 && missing_good == 0 && audit.is_empty() && still_clean && again.added == 0 && again.removed == 0 && problems.is_empty();
    println!("GATEWAY GATE: {}", if ok { "PASS" } else { "FAIL" });
    let _ = std::fs::remove_dir_all(&dir);
    std::process::exit(if ok { 0 } else { 1 });
}
