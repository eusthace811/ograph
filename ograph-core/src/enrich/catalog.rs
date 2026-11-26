//! Catalog layer: applications and their dependencies from `ogrep --entities --json`, which
//! already reads Backstage, Compose, Kubernetes and Prometheus rules. ograph keeps no copy of
//! that parsing; ogrep is the code specialist.

use super::ogrep::{self, Edge, Since};
use crate::model::{Batch, EntityRef, Extraction, entity, known, lit, obj, slug};
use std::collections::BTreeMap;
use crate::vocab::{RDF_TYPE, ops};
use oxrdf::NamedNode;
use std::path::{Path, PathBuf};

/// The files `ogrep --entities` reads in a repository: the YAML and `.env` files among those
/// ogrep searches, which honours the repository's ignore rules.
pub fn files(root: &Path, path: &Path) -> Vec<PathBuf> {
    let dir = root.join(path);
    let ops = |name: &str| name.ends_with(".yaml") || name.ends_with(".yml") || name.ends_with(".env");
    ogrep::files(&dir).iter().map(|l| dir.join(l)).filter(|p| p.file_name().is_some_and(|n| ops(&n.to_string_lossy()))).collect()
}

pub fn harvest(root: &Path, path: &Path, inferred: bool) -> Batch {
    let dir = root.join(path);
    let key = super::rel(root, &dir);
    let mut b = Batch::new("catalog", &key, "ogrep-entities");
    let read = match ogrep::dated_entities(&dir) {
        Ok(Some(e)) => Ok(e),
        Ok(None) => {
            b.degraded.push(format!("{key}: dependencies carry no date, so each holds at every time; dating them needs an ogrep release after 0.3.0"));
            ogrep::entities(&dir)
        }
        Err(e) => Err(e),
    };
    let entities = match read {
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
        let kept = e.edges.into_iter().filter(|edge| edge.relation == "depends_on" || (inferred && edge.relation == "calls"));
        deps.extend(kept.map(|edge| {
            let added = added_to_service(&edge, e.since.as_ref());
            (edge, added)
        }));
    }
    // A dependency a file declares is not also recorded as inferred.
    let declared: Vec<(String, String)> = deps.iter().filter(|(e, _)| e.relation == "depends_on").map(|(e, _)| (e.from.clone(), e.to.clone())).collect();
    deps.retain(|(e, _)| e.relation == "depends_on" || !declared.iter().any(|(from, to)| *from == e.from && *to == e.to));
    // One ops:dependsOn per pair holds from the earliest of its edges, and at every time if any
    // edge of it is undated.
    let mut dates: BTreeMap<(String, String), Option<Since>> = BTreeMap::new();
    for (edge, added) in &deps {
        let pair = dates.entry((edge.from.clone(), edge.to.clone())).or_insert_with(|| added.clone());
        *pair = match (pair.take(), added) {
            (Some(a), Some(b)) => Some(if b.time < a.time { b.clone() } else { a }),
            _ => None,
        };
    }
    for (edge, _) in deps {
        let ev = b.record(format!("{key}/{}", edge.evidence));
        let m = Extraction::Catalog { catalog: format!("ogrep-entities:{}", edge.provenance) };
        b.add(known("app", &edge.from), ops::DEPENDS_ON, obj(entity("app", &edge.to)), &ev, &m);
    }
    for ((from, to), since) in dates {
        let Some(since) = since else { continue };
        let ev = b.record(format!("{key}@{}", since.commit));
        let dep = EntityRef::Known(NamedNode::new_unchecked(format!("{}dependency/{}/{}", crate::vocab::DATA, slug(&from), slug(&to))));
        b.add(dep.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(ops::DEPENDENCY)), &ev, &method);
        b.add(dep.clone(), ops::CALLER, obj(entity("app", &from)), &ev, &method);
        b.add(dep.clone(), ops::CALLEE, obj(entity("app", &to)), &ev, &method);
        b.add(dep.clone(), ops::VALID_FROM, super::dt(since.time * 1000), &ev, &method);
        b.add(dep, ops::ADDED_IN, lit(since.commit.as_str()), &ev, &method);
    }
    b
}

/// The commit that added `edge` to a service the files already declared. An edge that came in
/// the commit that declared its service is not dated: before it the files said nothing about the
/// service, which is no sign that the dependency was missing. A catalog written today for a
/// system that ran for years would otherwise withhold every edge from every past incident.
fn added_to_service(edge: &Edge, service: Option<&Since>) -> Option<Since> {
    let since = edge.since.as_ref()?;
    (service.map(|s| &s.commit) != Some(&since.commit)).then(|| since.clone())
}
