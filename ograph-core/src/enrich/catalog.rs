//! Catalog layer: applications and declared dependencies from `ogrep --entities --json`, which
//! already reads Backstage, Compose, Kubernetes and Prometheus rules. ograph keeps no copy of
//! that parsing; ogrep is the code specialist.

use crate::model::{Batch, Extraction, known, lit, obj};
use crate::vocab::{RDF_TYPE, ops};
use oxrdf::NamedNode;
use std::path::Path;
use std::process::Command;

pub fn harvest(root: &Path, path: &Path) -> Batch {
    let dir = if path.is_absolute() { path.to_owned() } else { root.join(path) };
    let key = super::rel(root, &dir);
    let mut b = Batch::new("catalog", &key, "ogrep-entities");
    let out = Command::new(std::env::var("OGRAPH_OGREP").unwrap_or_else(|_| "ogrep".into()))
        .args(["--entities", ".", "--json"])
        .current_dir(&dir)
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            b.degraded.push(format!("ogrep --entities failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
            return b;
        }
        Err(e) => {
            b.degraded.push(format!("ogrep not runnable: {e}"));
            return b;
        }
    };
    let method = Extraction::Catalog { catalog: "ogrep-entities".into() };
    let mut deps = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(e) = v.pointer("/data/entity") else { continue };
        let (Some(id), Some(kind)) = (e["id"].as_str(), e["kind"].as_str()) else { continue };
        if kind != "service" {
            continue;
        }
        let loc = e["locations"][0].clone();
        let ev = b.record(format!("{key}/{}:{}", loc["file"].as_str().unwrap_or("?"), loc["line"]));
        let app = known("app", id);
        b.add(app.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(ops::APPLICATION)), &ev, &method);
        b.add(app.clone(), ops::NAME, lit(id), &ev, &method);
        for edge in v.pointer("/data/edges").and_then(|x| x.as_array()).into_iter().flatten() {
            if edge["relation"] != "depends_on" {
                continue;
            }
            let (Some(from), Some(to)) = (edge["from"].as_str(), edge["to"].as_str()) else { continue };
            let ev = format!("{key}/{}:{}", edge["evidence"]["file"].as_str().unwrap_or("?"), edge["evidence"]["line"]);
            deps.push((from.to_owned(), to.to_owned(), ev, edge["provenance"].as_str().unwrap_or("declared").to_owned()));
        }
    }
    for (from, to, ev, provenance) in deps {
        let ev = b.record(ev);
        let m = Extraction::Catalog { catalog: format!("ogrep-entities:{provenance}") };
        b.add(known("app", &from), ops::DEPENDS_ON, obj(crate::model::entity("app", &to)), &ev, &m);
    }
    b
}
