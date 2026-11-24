//! Catalog layer: applications and their dependencies from `ogrep --entities --json`, which
//! already reads Backstage, Compose, Kubernetes and Prometheus rules. ograph keeps no copy of
//! that parsing; ogrep is the code specialist.

use super::ogrep;
use crate::model::{Batch, Extraction, known, lit, obj};
use crate::vocab::{RDF_TYPE, ops};
use oxrdf::NamedNode;
use std::path::{Path, PathBuf};

/// The files `ogrep --entities` reads in a repository: the YAML and `.env` files among those
/// ogrep searches, which honours the repository's ignore rules.
pub fn files(root: &Path, path: &Path) -> Vec<PathBuf> {
    let dir = if path.is_absolute() { path.to_owned() } else { root.join(path) };
    let ops = |name: &str| name.ends_with(".yaml") || name.ends_with(".yml") || name.ends_with(".env");
    ogrep::files(&dir).iter().map(|l| dir.join(l)).filter(|p| p.file_name().is_some_and(|n| ops(&n.to_string_lossy()))).collect()
}

pub fn harvest(root: &Path, path: &Path, inferred: bool) -> Batch {
    let dir = if path.is_absolute() { path.to_owned() } else { root.join(path) };
    let key = super::rel(root, &dir);
    let mut b = Batch::new("catalog", &key, "ogrep-entities");
    let entities = match ogrep::entities(&dir) {
        Ok(e) => e,
        Err(e) => {
            b.degraded.push(e.to_string());
            return b;
        }
    };
    let method = Extraction::Catalog { catalog: "ogrep-entities".into() };
    let mut deps = Vec::new();
    for e in entities.into_iter().filter(|e| e.kind == "service") {
        let ev = b.record(format!("{key}/{}", e.location));
        let app = known("app", &e.id);
        b.add(app.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(ops::APPLICATION)), &ev, &method);
        b.add(app.clone(), ops::NAME, lit(e.id.as_str()), &ev, &method);
        for alias in &e.aliases {
            b.add(app.clone(), ops::ALIAS, lit(alias.as_str()), &ev, &method);
        }
        deps.extend(e.edges.into_iter().filter(|edge| edge.relation == "depends_on" || (inferred && edge.relation == "calls")));
    }
    // A dependency a file declares is not also recorded as inferred.
    let declared: Vec<(String, String)> = deps.iter().filter(|e| e.relation == "depends_on").map(|e| (e.from.clone(), e.to.clone())).collect();
    deps.retain(|e| e.relation == "depends_on" || !declared.iter().any(|(from, to)| *from == e.from && *to == e.to));
    let deps = deps.into_iter().map(|edge| (edge.from, edge.to, format!("{key}/{}", edge.evidence), edge.provenance));
    for (from, to, ev, provenance) in deps {
        let ev = b.record(ev);
        let m = Extraction::Catalog { catalog: format!("ogrep-entities:{provenance}") };
        b.add(known("app", &from), ops::DEPENDS_ON, obj(crate::model::entity("app", &to)), &ev, &m);
    }
    b
}
