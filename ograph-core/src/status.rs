//! `ograph status`: what is due in a workspace. Source files changed, added, deleted or renamed
//! since the last full harvest, and incidents whose chains nobody has judged. Cheap enough for an
//! agent's session-start hook.

use crate::correlate::{self, Ids};
use crate::store::GraphSel;
use crate::vocab::{corr, inc, rca};
use crate::workspace::{HARVESTED, Workspace};
use rustc_hash::FxHashSet;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::time::UNIX_EPOCH;

#[derive(Serialize)]
pub struct Status {
    /// When the last full harvest started, if one has run since ograph began recording it.
    pub harvested_at: Option<String>,
    /// Inputs and settings changed, added, deleted or renamed since then, relative to the
    /// workspace root.
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
            let since = self.harvested_at.as_deref().map_or("with no full harvest recorded".to_owned(), |t| format!("since the harvest at {t}"));
            parts.push(format!("the graph is stale: {} source files changed {since} ({}); run `ograph harvest`", self.changed.len(), list(&self.changed)));
        }
        if !self.unjudged.is_empty() {
            parts.push(format!("{} incidents have chains but no verdict ({})", self.unjudged.len(), list(&self.unjudged)));
        }
        (!parts.is_empty()).then(|| format!("ograph: {}.", parts.join("; ")))
    }
}

pub fn status(ws: &Workspace) -> anyhow::Result<Status> {
    let marker = std::fs::read_to_string(ws.dir.join(HARVESTED)).unwrap_or_default();
    let mut lines = marker.lines();
    let harvested: Option<i64> = lines.next().and_then(|s| s.trim().parse().ok());
    // Each file read, with its modification time and size when the harvest began. A marker from
    // before ograph recorded those has only the names, and an older one only the time.
    let read: Option<BTreeMap<&str, Option<Stamp>>> = harvested.and_then(|_| {
        let files: BTreeMap<&str, Option<Stamp>> = lines.map(stamped).collect();
        (!files.is_empty()).then_some(files)
    });
    let now = inputs(ws);
    let mut changed: BTreeSet<String> = now
        .iter()
        .filter(|(f, stamp)| match read.as_ref().and_then(|r| r.get(f.as_str())) {
            Some(Some(then)) => then != stamp,
            _ => harvested.is_none_or(|h| stamp.0 > h),
        })
        .map(|(f, _)| f.clone())
        .collect();
    if let Some(read) = &read {
        let present: BTreeSet<&str> = now.iter().map(|(f, _)| f.as_str()).collect();
        changed.extend(read.keys().filter(|f| !present.contains(*f)).map(|f| f.to_string()));
        changed.extend(present.iter().filter(|f| !read.contains_key(*f)).map(|f| f.to_string()));
    }
    Ok(Status { harvested_at: harvested.map(crate::term::format_millis), changed: changed.into_iter().collect(), unjudged: unjudged(&ws.store.snapshot()?)? })
}

/// A file's modification time in Unix ms and its size in bytes.
pub type Stamp = (i64, u64);

/// One marker line: `path`, or `path<TAB>mtime<TAB>size`.
fn stamped(line: &str) -> (&str, Option<Stamp>) {
    let mut parts = line.rsplitn(3, '\t');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(size), Some(ms), Some(f)) => match (ms.parse(), size.parse()) {
            (Ok(ms), Ok(size)) => (f, Some((ms, size))),
            _ => (line, None),
        },
        _ => (line, None),
    }
}

/// A marker line for a file and its stamp.
pub fn marker_line((f, (ms, size)): &(String, Stamp)) -> String {
    format!("{f}\t{ms}\t{size}")
}

/// The files a harvest would read that exist now, relative to the workspace root, with their
/// stamps. A missing file, such as an optional shapes.ttl, is not read.
pub fn inputs(ws: &Workspace) -> Vec<(String, Stamp)> {
    crate::serve::inventory(ws)
        .into_iter()
        .filter_map(|(f, m, size)| {
            let ms = m?.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
            Some((f.strip_prefix(&ws.root).unwrap_or(&f).display().to_string(), (ms, size)))
        })
        .collect()
}

fn unjudged(s: &crate::store::Snapshot) -> anyhow::Result<Vec<String>> {
    let (Some(rank), Some(leads)) = (s.iri(corr::RANK)?, s.iri(corr::LEADS_TO)?) else { return Ok(vec![]) };
    let Ok(ids) = Ids::new(s) else { return Ok(vec![]) };
    let mut judged: FxHashSet<u64> = FxHashSet::default();
    if let Some(on) = s.iri(rca::ON_CHAIN)? {
        for q in s.quads([None, Some(on), None], GraphSel::Union) {
            judged.extend(s.object(q?[2], leads)?);
        }
    }
    let id_p = s.iri(inc::ID)?;
    let mut out: Vec<(i64, String)> = Vec::new();
    let mut ids_seen: BTreeSet<String> = BTreeSet::new();
    for q in s.quads([None, Some(rank), None], GraphSel::Union) {
        // Each incident ranks several chains; one judged or listed is done.
        let Some(i) = s.object(q?[0], leads)? else { continue };
        if !judged.insert(i) {
            continue;
        }
        let id = match id_p {
            Some(p) => s.object(i, p)?.and_then(|v| correlate::literal(s, v)),
            None => None,
        };
        let id = id.unwrap_or_else(|| crate::vocab::compact(&correlate::literal(s, i).unwrap_or_default()));
        if ids_seen.insert(id.clone()) {
            out.push((correlate::incident_anchor(s, &ids, i).map_or(0, |a| a.1), id));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(out.into_iter().map(|x| x.1).collect())
}
