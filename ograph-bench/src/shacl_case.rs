//! Runs one W3C SHACL core test and prints `conforms=<bool> results=<n>`, the interface
//! `scripts/eval/shacl_suite.py` expects. Data and shapes come from `sht:dataGraph` and
//! `sht:shapesGraph`, which usually point back at the test file itself.

use oxrdf::{Graph, NamedNode, Term};
use std::path::Path;

fn load(path: &Path) -> anyhow::Result<(Graph, String)> {
    let base = format!("file://{}", std::fs::canonicalize(path)?.display()).replace(' ', "%20");
    let mut g = Graph::new();
    for t in oxttl::TurtleParser::new().with_base_iri(base.clone())?.for_slice(&std::fs::read(path)?) {
        g.insert(&t?);
    }
    Ok((g, base))
}

fn main() -> anyhow::Result<()> {
    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: shacl_case FILE.ttl"));
    let (manifest, base) = load(&path)?;
    let sht = |l: &str| NamedNode::new_unchecked(format!("http://www.w3.org/ns/shacl-test#{l}"));
    let pick = |p: NamedNode| -> anyhow::Result<Graph> {
        let target = manifest.triples_for_predicate(&p).next().map(|t| t.object.into_owned());
        match target {
            Some(Term::NamedNode(n)) if n.as_str() != base => {
                let file = n.as_str().trim_start_matches("file://").replace("%20", " ");
                Ok(load(Path::new(&file))?.0)
            }
            _ => Ok(manifest.clone()),
        }
    };
    let data = pick(sht("dataGraph"))?;
    let shapes_graph = pick(sht("shapesGraph"))?;
    let shapes = ograph_core::shacl::Shapes::parse(&shapes_graph);
    let results = ograph_core::shacl::Validator::new(&shapes, &data).validate_all();
    if std::env::var_os("SHACL_DEBUG").is_some() {
        for r in &results {
            eprintln!("{} {} {:?} {:?}", r.component, r.focus, r.path, r.value);
        }
    }
    println!("conforms={} results={}", results.is_empty(), results.len());
    Ok(())
}
