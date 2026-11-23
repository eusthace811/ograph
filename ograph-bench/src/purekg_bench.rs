//! Why not a pure knowledge graph: load one RCAEval case's raw telemetry (every metric point,
//! log line and span) as triples, the way a "put everything in the graph" design would, and
//! compare it with ograph's hybrid harvest of the same case (episodes plus pointers).
//!
//! Usage: purekg_bench CASE_DIR INCIDENT_EPOCH_S
//! CASE_DIR is an exported case (metrics.csv, logs.jsonl, traces.jsonl, .ograph/ already harvested).

use ograph_core::sparql::{self, Answer};
use ograph_core::store::Store;
use ograph_core::term;
use oxrdf::vocab::xsd;
use oxrdf::{Literal, NamedNode, Term};
use std::path::Path;
use std::time::Instant;

const RAW: &str = "https://ograph.dev/raw#";

fn n(local: &str) -> Term {
    NamedNode::new_unchecked(format!("{RAW}{local}")).into()
}
fn id(local: &str) -> Term {
    NamedNode::new_unchecked(format!("{}{local}", ograph_core::vocab::DATA)).into()
}
fn dt(ms: i64) -> Term {
    Literal::new_typed_literal(term::format_millis(ms), xsd::DATE_TIME).into()
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let case = Path::new(&a[1]);
    let incident_s: i64 = a[2].parse()?;
    let tmp = std::env::temp_dir().join(format!("purekg-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let store = Store::open(&tmp)?;
    let ty: Term = NamedNode::new_unchecked(ograph_core::vocab::RDF_TYPE).into();
    let on: Term = NamedNode::new_unchecked(ograph_core::vocab::evt::ON).into();
    let at: Term = NamedNode::new_unchecked(ograph_core::vocab::evt::AT).into();
    let mut triples: Vec<[Term; 3]> = Vec::new();
    let t_parse = Instant::now();
    let csv = std::fs::read_to_string(case.join("metrics.csv"))?;
    let mut lines = csv.lines();
    let head: Vec<&str> = lines.next().unwrap().split(',').collect();
    let mut points = 0;
    for (r, line) in lines.enumerate() {
        let cells: Vec<&str> = line.split(',').collect();
        let t = (cells[0].parse::<f64>()? * 1000.0) as i64;
        for (c, v) in cells.iter().enumerate().skip(1) {
            let Ok(v) = v.parse::<f64>() else { continue };
            let p = id(&format!("point/{r}/{c}"));
            let (svc, metric) = head[c].split_once('_').unwrap_or((head[c], ""));
            triples.push([p.clone(), ty.clone(), n("MetricPoint")]);
            triples.push([p.clone(), on.clone(), id(&format!("app/{svc}"))]);
            triples.push([p.clone(), n("metric"), Literal::new_simple_literal(metric).into()]);
            triples.push([p.clone(), at.clone(), dt(t)]);
            triples.push([p, n("value"), Literal::new_typed_literal(format!("{v}"), xsd::DOUBLE).into()]);
            points += 1;
        }
    }
    let mut log_lines = 0;
    for (i, line) in std::fs::read_to_string(case.join("logs.jsonl"))?.lines().enumerate() {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let p = id(&format!("log/{i}"));
        triples.push([p.clone(), ty.clone(), n("LogLine")]);
        triples.push([p.clone(), on.clone(), id(&format!("app/{}", v["service"].as_str().unwrap_or("?")))]);
        triples.push([p.clone(), at.clone(), dt(v["ts"].as_i64().unwrap_or(0) * 1000)]);
        triples.push([p, n("message"), Literal::new_simple_literal(v["message"].as_str().unwrap_or("")).into()]);
        log_lines += 1;
    }
    let mut spans = 0;
    if let Ok(text) = std::fs::read_to_string(case.join("traces.jsonl")) {
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line)?;
            let p = id(&format!("span/{}", v["span_id"].as_str().unwrap_or("?")));
            triples.push([p.clone(), ty.clone(), n("Span")]);
            let svc = v["service"].as_str().unwrap_or("?");
            let svc = if svc == "frontendservice" { "frontend" } else { svc };
            triples.push([p.clone(), on.clone(), id(&format!("app/{svc}"))]);
            triples.push([p.clone(), at.clone(), dt(v["start_ms"].as_i64().unwrap_or(0))]);
            triples.push([p.clone(), n("durationUs"), Literal::from(v["duration_us"].as_i64().unwrap_or(0)).into()]);
            triples.push([p.clone(), n("status"), Literal::from(v["status"].as_i64().unwrap_or(0)).into()]);
            triples.push([p.clone(), n("trace"), Literal::new_simple_literal(v["trace_id"].as_str().unwrap_or("")).into()]);
            if let Some(parent) = v["parent_span_id"].as_str().filter(|s| !s.is_empty()) {
                triples.push([p, n("parent"), id(&format!("span/{parent}"))]);
            }
            spans += 1;
        }
    }
    let parse = t_parse.elapsed();
    let t_load = Instant::now();
    let g: Term = NamedNode::new_unchecked("https://ograph.dev/graph/raw").into();
    for chunk in triples.chunks(500_000) {
        store.transact(|tx| {
            let gid = tx.intern(&g)?;
            let mut ids = Vec::with_capacity(chunk.len());
            for [s, p, o] in chunk {
                ids.push([tx.intern(s)?, tx.intern(p)?, tx.intern(o)?, gid]);
            }
            tx.insert(ids)
        })?;
    }
    let load = t_load.elapsed();
    let raw_size = std::fs::metadata(&tmp)?.len();
    let snap = store.snapshot()?;
    let window = |a: i64, b: i64| (term::format_millis(a * 1000), term::format_millis(b * 1000));
    let (w0, w1) = window(incident_s - 600, incident_s);
    let deps = "id:app\\/frontend id:app\\/checkoutservice id:app\\/cartservice id:app\\/currencyservice id:app\\/productcatalogservice id:app\\/recommendationservice id:app\\/shippingservice id:app\\/adservice id:app\\/emailservice id:app\\/paymentservice id:app\\/redis";
    let time_q = |q: &str| -> anyhow::Result<(f64, Answer)> {
        let t = Instant::now();
        let r = sparql::run(&snap, q, None)?;
        Ok((t.elapsed().as_secs_f64() * 1e3, r))
    };
    let count = |r: &Answer| match r {
        Answer::Solutions { rows, .. } => rows.len(),
        _ => 0,
    };
    let (q1_ms, q1) = time_q(&format!("SELECT ?x WHERE {{ VALUES ?app {{ {deps} }} ?x evt:on ?app ; evt:at ?t . FILTER(?t >= \"{w0}\"^^xsd:dateTime && ?t <= \"{w1}\"^^xsd:dateTime) }}"))?;
    let (b0, _) = window(incident_s - 1200, incident_s - 600);
    let (q2_ms, q2) = time_q(&format!(
        "PREFIX raw: <{RAW}> SELECT ?app ?m ?late (AVG(?v) AS ?mean) (AVG(?v * ?v) AS ?sq) (COUNT(?v) AS ?n) WHERE {{ ?p a raw:MetricPoint ; evt:on ?app ; raw:metric ?m ; evt:at ?t ; raw:value ?v . FILTER(?t >= \"{b0}\"^^xsd:dateTime && ?t <= \"{w1}\"^^xsd:dateTime) BIND(?t >= \"{w0}\"^^xsd:dateTime AS ?late) }} GROUP BY ?app ?m ?late"
    ))?;
    let bytes_q1: usize = match &q1 {
        Answer::Solutions { rows, .. } => rows.iter().map(|r| r.iter().map(|c| c.as_ref().map_or(0, String::len) + 1).sum::<usize>()).sum(),
        _ => 0,
    };
    drop(snap);
    drop(store);
    let _ = std::fs::remove_file(&tmp);

    let ws = ograph_core::workspace::Workspace::open(case)?;
    let hsnap = ws.store.snapshot()?;
    let hybrid_quads: u64 = hsnap.graphs()?.iter().map(|g| g.1).sum();
    let hybrid_size = std::fs::metadata(ws.store.path())?.len();
    let inc_id = std::fs::read_to_string(case.join("incidents.json"))?;
    let inc: serde_json::Value = serde_json::from_str(&inc_id)?;
    let t = Instant::now();
    let before = ograph_core::query::before(&hsnap, inc[0]["id"].as_str().unwrap(), 600, 4)?;
    let before_ms = t.elapsed().as_secs_f64() * 1e3;
    let before_bytes = serde_json::to_string(&before.events)?.len();
    let raw_bytes: u64 = ["metrics.csv", "logs.jsonl", "traces.jsonl"].iter().filter_map(|f| std::fs::metadata(case.join(f)).ok()).map(|m| m.len()).sum();
    println!("case {} ({:.0} MiB of raw exports: {points} metric points, {log_lines} log lines, {spans} spans)\n", case.display(), raw_bytes as f64 / 1048576.0);
    println!("| | pure knowledge graph (every record a node) | ograph hybrid (episodes + pointers) |\n|---|---|---|");
    println!("| quads | {} | {} |", triples.len(), hybrid_quads);
    println!("| store size | {:.1} MiB | {:.2} MiB |", raw_size as f64 / 1048576.0, hybrid_size as f64 / 1048576.0);
    println!("| ingest | parse {:.1} s + load {:.1} s ({:.0} quads/s) | full harvest per case, see the median in rcaeval-final.txt |", parse.as_secs_f64(), load.as_secs_f64(), triples.len() as f64 / load.as_secs_f64());
    println!("| Q1 'what happened in the 10 min before', rows | {} raw records ({:.1} MiB of text for an agent) in {q1_ms:.0} ms | {} episodes ({:.1} KiB) in {before_ms:.2} ms |", count(&q1), bytes_q1 as f64 / 1048576.0, before.events.len(), before_bytes as f64 / 1024.0);
    println!("| anomaly per series in SPARQL (mean before vs after; SPARQL 1.1 has no median, MAD or window functions) | {} series in {q2_ms:.0} ms | robust z detector in Rust during harvest |", count(&q2));
    let secs = 1441.0;
    println!("\nextrapolated to a day of this 11-service system: pure KG {:.0} M quads and {:.0} GiB per day; ograph {:.2} M quads per day", triples.len() as f64 * 86400.0 / secs / 1e6, raw_size as f64 * 86400.0 / secs / 1073741824.0, hybrid_quads as f64 * 86400.0 / secs / 1e6);
    Ok(())
}
