#![allow(dead_code)]
//! Regenerates the redb-quadstore graph with its own generator (included verbatim), writes it
//! as CSV for the servers, and writes the expected rows computed by redb-quadstore's native
//! path over its own data/store.redb.

#[path = "../../../redb-quadstore/src/store.rs"]
mod store;

use oxrdf::vocab::{rdf, xsd};
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
use redb::Database;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use store::Tables;

type Rows = Vec<Vec<Term>>;
include!("../generator.in.rs");

fn local(t: &Term) -> String {
    match t {
        Term::NamedNode(n) => n.as_str().strip_prefix(OPS).unwrap().to_string(),
        Term::Literal(l) => l.value().to_string(),
        _ => unreachable!(),
    }
}
fn num(s: &str) -> u64 {
    s.rsplit('/').next().unwrap().parse().unwrap()
}

#[derive(Default)]
struct Node {
    kind: String,
    props: BTreeMap<String, String>,
}

fn main() {
    let out = Path::new("data/export");
    std::fs::create_dir_all(out).unwrap();
    let world = generate();
    assert_eq!(world.quads.len(), 3_520_089, "generator drifted from redb-quadstore");

    let mut nodes: BTreeMap<(String, u64), Node> = BTreeMap::new();
    let mut edges: BTreeMap<&str, Vec<(u64, u64)>> = BTreeMap::new();
    for q in &world.quads {
        let s = local(&q.subject.clone().into());
        let (kind, n) = (s.split('/').next().unwrap().to_string(), num(&s));
        let p = q.predicate.as_str();
        let node = nodes.entry((kind.clone(), n)).or_default();
        if p == rdf::TYPE.as_str() {
            node.kind = local(&q.object);
            continue;
        }
        let p = p.strip_prefix(OPS).unwrap();
        match p {
            "dependsOn" | "affects" | "about" => edges.entry(match p { "dependsOn" => "depends_on", "affects" => "affects", _ => "about" }).or_default().push((n, num(&local(&q.object)))),
            "at" | "openedAt" => {
                let ms = store::date_time_millis(&q.object).unwrap();
                node.props.insert(p.to_string(), ms.to_string());
            }
            _ => {
                node.props.insert(p.to_string(), local(&q.object));
            }
        }
    }
    let mut files: BTreeMap<&str, std::io::BufWriter<std::fs::File>> = BTreeMap::new();
    let mut open = |name: &'static str, header: &str| {
        let mut f = std::io::BufWriter::new(std::fs::File::create(out.join(format!("{name}.csv"))).unwrap());
        writeln!(f, "{header}").unwrap();
        files.insert(name, f);
    };
    open("app", "id,team,region");
    open("incident", "id,severity,opened_at");
    open("event", "id,kind,at");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for ((kind, n), node) in &nodes {
        let line = match kind.as_str() {
            "app" => format!("{n},{},{}", node.props["ownedBy"], node.props["deployedIn"]),
            "inc" => format!("{n},{},{}", node.props["severity"], node.props["openedAt"]),
            "ev" => format!("{n},{},{}", node.kind, node.props["at"]),
            _ => unreachable!(),
        };
        let file = match kind.as_str() { "app" => "app", "inc" => "incident", _ => "event" };
        writeln!(files.get_mut(file).unwrap(), "{line}").unwrap();
        *counts.entry(file.to_string()).or_default() += 1;
    }
    let ends = [("depends_on", "app", "app"), ("affects", "incident", "app"), ("about", "event", "app")];
    for (name, from, to) in ends {
        let mut f = std::io::BufWriter::new(std::fs::File::create(out.join(format!("{name}.csv"))).unwrap());
        writeln!(f, "start_id,start_vertex_type,end_id,end_vertex_type").unwrap();
        for (a, b) in &edges[name] {
            writeln!(f, "{a},{from},{b},{to}").unwrap();
        }
        counts.insert(name.to_string(), edges[name].len());
    }
    drop(files);
    let triples = counts["app"] * 3 + counts["incident"] * 4 + counts["event"] * 3 + counts["depends_on"];
    println!("exported {counts:?}, reconstructs {triples} triples");
    assert_eq!(triples, world.quads.len());

    let db = Database::open("../redb-quadstore/data/store.redb").unwrap();
    let tables = Tables::open(&db).unwrap();
    let native = Native { t: &tables };
    let exp = Path::new("data/expected");
    std::fs::create_dir_all(exp).unwrap();
    let line = |r: &Vec<Term>| r.iter().map(local).collect::<Vec<_>>().join(" ");
    let w1: Vec<String> = native.incidents_of_bedrock_us_east().iter().map(line).collect();
    std::fs::write(exp.join("w1.txt"), w1.join("\n") + "\n").unwrap();
    let (picked, starts) = instances(&world);
    let mut w2 = String::new();
    for inc in &picked {
        let mut rows: Vec<String> = native.correlated_events(&inc.iri).iter().map(line).collect();
        rows.sort();
        writeln!(w2, "{}\t{}", local(&inc.iri.clone().into()), rows.join(" ")).unwrap();
    }
    std::fs::write(exp.join("w2.tsv"), w2).unwrap();
    let mut w3 = String::new();
    for &i in &starts {
        let mut rows: Vec<String> = native.blast_radius(&world.apps[i]).iter().map(line).collect();
        rows.sort();
        writeln!(w3, "app/{i}\t{}", rows.join(" ")).unwrap();
    }
    std::fs::write(exp.join("w3.tsv"), w3).unwrap();
    println!("expected: W1 {} rows, W2 {} incidents, W3 {} starts", w1.len(), picked.len(), starts.len());
}
