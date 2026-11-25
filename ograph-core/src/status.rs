//! `ograph status`: what is due in a workspace. Source files changed since the last full harvest,
//! and incidents whose chains nobody has judged. Cheap enough for an agent's session-start hook.

use crate::correlate::{self, Ids};
use crate::store::GraphSel;
use crate::vocab::{corr, inc, rca};
use crate::workspace::{HARVESTED, Workspace};
use serde::Serialize;
use std::time::UNIX_EPOCH;

#[derive(Serialize)]
pub struct Status {
    /// When the last full harvest started, if one has run since ograph began recording it.
    pub harvested_at: Option<String>,
    /// Inputs and settings changed since then, relative to the workspace root.
    pub changed: Vec<String>,
    /// Incidents with chains and no verdict on any of them.
    pub unjudged: Vec<String>,
}

impl Status {
    pub fn due(&self) -> bool {
        !self.changed.is_empty() || !self.unjudged.is_empty()
    }

    /// One line for a person or an agent, or nothing when nothing is due.
    pub fn note(&self) -> Option<String> {
        let list = |v: &[String]| {
            let mut s = v.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
            if v.len() > 5 {
                s.push_str(&format!(" and {} more", v.len() - 5));
            }
            s
        };
        let mut parts = Vec::new();
        if !self.changed.is_empty() {
            let since = self.harvested_at.as_deref().map_or("no full harvest is recorded".to_owned(), |t| format!("since the harvest at {t}"));
            parts.push(format!("the graph is stale: {} source files changed {since} ({}); run `ograph harvest`", self.changed.len(), list(&self.changed)));
        }
        if !self.unjudged.is_empty() {
            parts.push(format!("{} incidents have chains but no verdict ({})", self.unjudged.len(), list(&self.unjudged)));
        }
        (!parts.is_empty()).then(|| format!("ograph: {}.", parts.join("; ")))
    }
}

pub fn status(ws: &Workspace) -> anyhow::Result<Status> {
    let harvested: Option<i64> = std::fs::read_to_string(ws.dir.join(HARVESTED)).ok().and_then(|s| s.trim().parse().ok());
    let mut changed: Vec<String> = crate::serve::inventory(ws)
        .into_iter()
        .filter(|(_, m, _)| {
            // A file that does not exist, such as an optional shapes.ttl, has nothing to re-read.
            let Some(ms) = m.and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64) else { return false };
            harvested.is_none_or(|h| ms > h)
        })
        .map(|(f, _, _)| f.strip_prefix(&ws.root).unwrap_or(&f).display().to_string())
        .collect();
    changed.sort();
    Ok(Status { harvested_at: harvested.map(crate::term::format_millis), changed, unjudged: unjudged(&ws.store.snapshot()?)? })
}

fn unjudged(s: &crate::store::Snapshot) -> anyhow::Result<Vec<String>> {
    let (Some(rank), Some(leads)) = (s.iri(corr::RANK)?, s.iri(corr::LEADS_TO)?) else { return Ok(vec![]) };
    let Ok(ids) = Ids::new(s) else { return Ok(vec![]) };
    let judged: Vec<u64> = match s.iri(rca::ON_CHAIN)? {
        Some(on) => s.quads([None, Some(on), None], GraphSel::Union).filter_map(Result::ok).filter_map(|q| s.object(q[2], leads).ok().flatten()).collect(),
        None => vec![],
    };
    let id_p = s.iri(inc::ID)?;
    let mut out: Vec<(i64, String)> = Vec::new();
    for q in s.quads([None, Some(rank), None], GraphSel::Union) {
        let Some(i) = s.object(q?[0], leads)? else { continue };
        if judged.contains(&i) {
            continue;
        }
        let id = id_p.and_then(|p| s.object(i, p).ok().flatten()).and_then(|v| correlate::literal(s, v)).unwrap_or_else(|| crate::vocab::compact(&correlate::literal(s, i).unwrap_or_default()));
        let at = correlate::incident_anchor(s, &ids, i).map_or(0, |a| a.1);
        if !out.iter().any(|(_, x)| *x == id) {
            out.push((at, id));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(out.into_iter().map(|x| x.1).collect())
}
