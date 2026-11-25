//! `ograph cite-check`: the chain ids, incident ids, application names and IRIs an answer cites,
//! each checked against the graph as it is now.

use crate::correlate::{self, Ids, app_name};
use crate::gateway::{Catalog, Resolution};
use crate::store::{GraphSel, Snapshot};
use crate::vocab::{self, corr, inc, ops};
use regex::Regex;
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Chain,
    Incident,
    App,
    Iri,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Present,
    /// A chain the graph no longer produces, or cites at a rank it no longer has.
    Stale,
    Unknown,
}

#[derive(Serialize)]
pub struct Citation {
    pub kind: Kind,
    pub cited: String,
    pub status: Status,
    pub detail: String,
    /// The 1-based line of the answer it first appears on.
    pub line: usize,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

/// Full or prefixed ograph IRIs; SPARQL's `id:app\/x` escape counts too.
fn iris() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"(?:https://ograph\.dev/id/|\bid:)([a-z][a-z0-9-]*)\\?/([A-Za-z0-9._/-]*[A-Za-z0-9])")
}

/// Words that could be application names: letters, digits and inner `-`, `_` or `.`.
fn tokens() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"[A-Za-z][A-Za-z0-9]*(?:[-_.][A-Za-z0-9]+)*")
}

/// The rank an answer gives the chain id written between `before` and `after`, the text that
/// separates it from its neighbouring IRIs: `id:chain/x is rank 2`, `id:chain/x (rank 2)` or
/// `rank 2: id:chain/x`. A list number like `3. id:chain/x` is no rank.
fn cited_rank(before: &str, after: &str) -> Option<i64> {
    static AFTER: OnceLock<Regex> = OnceLock::new();
    static BEFORE: OnceLock<Regex> = OnceLock::new();
    let a = re(&AFTER, r"(?i)^[\s,)]*\(?\s*(?:(?:is|was|at|now)\s+)*(?:the\s+)?rank\s*#?(\d+)\b\s*(:?)");
    let b = re(&BEFORE, r"(?i)\brank\s*#?(\d+)\s*(?:is|was|[:=,-])?\s*(?:the\s+)?(?:chain\s+)?$");
    // `x, rank 2: y` gives the rank to y.
    a.captures(after).filter(|c| c[2].is_empty()).or_else(|| b.captures(before))?[1].parse().ok()
}

pub fn check(s: &Snapshot, text: &str) -> anyhow::Result<Vec<Citation>> {
    let ids = Ids::new(s)?;
    let catalog = Catalog::from_snapshot(s, None)?;
    let incident_ids = incident_pattern(s, &ids)?;
    let mut out: Vec<Citation> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut found: Vec<(usize, Citation)> = Vec::new();
        let matches: Vec<regex::Captures> = iris().captures_iter(line).collect();
        for (j, m) in matches.iter().enumerate() {
            let whole = m.get(0).unwrap();
            let prev_end = if j == 0 { 0 } else { matches[j - 1].get(0).unwrap().end() };
            let next_start = matches.get(j + 1).map_or(line.len(), |n| n.get(0).unwrap().start());
            spans.push((whole.start(), whole.end()));
            let (kind, local) = (&m[1], m[2].to_owned());
            let iri = format!("{}{kind}/{local}", vocab::DATA);
            let shown = format!("id:{kind}/{local}");
            let (k, status, detail) = match kind {
                "chain" => {
                    let (st, d) = chain(s, &ids, &iri, cited_rank(&line[prev_end..whole.start()], &line[whole.end()..next_start]))?;
                    (Kind::Chain, st, d)
                }
                "inc" => match s.iri(&iri)?.filter(|&i| ids.incident.is_some_and(|c| s.contains([i, ids.ty, c]).unwrap_or(false))) {
                    Some(_) => (Kind::Incident, Status::Present, "an incident in the graph".into()),
                    None => (Kind::Incident, Status::Unknown, "no incident with this IRI".into()),
                },
                "app" => match s.iri(&iri)?.filter(|&i| s.iri(ops::APPLICATION).ok().flatten().is_some_and(|c| s.contains([i, ids.ty, c]).unwrap_or(false))) {
                    Some(a) => (Kind::App, Status::Present, format!("the catalog application {}", app_name(s, &ids, a))),
                    None => (Kind::App, Status::Unknown, "not a catalog application".into()),
                },
                _ => match s.iri(&iri)?.filter(|&i| s.quads([Some(i), None, None], GraphSel::Union).next().is_some()) {
                    Some(_) => (Kind::Iri, Status::Present, "a node with facts in the graph".into()),
                    None => (Kind::Iri, Status::Unknown, "no facts about this IRI".into()),
                },
            };
            found.push((whole.start(), Citation { kind: k, cited: shown, status, detail, line: n + 1 }));
        }
        if let Some(r) = &incident_ids {
            for m in r.find_iter(line) {
                if spans.iter().any(|&(a, b)| m.start() >= a && m.end() <= b) {
                    continue;
                }
                spans.push((m.start(), m.end()));
                let (status, detail) = match crate::query::lookup(s, m.as_str()) {
                    Ok(i) if ids.incident.is_some_and(|c| s.contains([i, ids.ty, c]).unwrap_or(false)) => (Status::Present, "an incident in the graph".into()),
                    _ => (Status::Unknown, "no incident has this id".into()),
                };
                found.push((m.start(), Citation { kind: Kind::Incident, cited: m.as_str().to_owned(), status, detail, line: n + 1 }));
            }
        }
        for m in tokens().find_iter(line) {
            if spans.iter().any(|&(a, b)| m.start() < b && m.end() > a) {
                continue;
            }
            // Part of a path or a longer identifier, like `2026-01-ledger-pool.md`, is no name.
            let joined = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || "/._-".contains(c));
            if joined(line[..m.start()].chars().next_back()) || line[m.end()..].starts_with('/') {
                continue;
            }
            let word = m.as_str();
            let named = word.contains(['-', '_']);
            let resolved = catalog.explain(ops::APPLICATION, word);
            let (status, detail) = match &resolved.resolution {
                Resolution::Exact(a) => (Status::Present, format!("the catalog application {}", vocab::compact(a.as_str()))),
                Resolution::Fuzzy(a, sc) if named => (Status::Present, format!("a misspelling of {} ({sc:.2})", vocab::compact(a.as_str()))),
                Resolution::Ambiguous(..) if named => (Status::Unknown, format!("not in the catalog: {}", resolved.why)),
                Resolution::Missing if named => match catalog.extended(ops::APPLICATION, word) {
                    Some(a) => (Status::Unknown, format!("not in the catalog: {} with more words", vocab::compact(a.as_str()))),
                    None => continue,
                },
                _ => continue,
            };
            found.push((m.start(), Citation { kind: Kind::App, cited: word.to_owned(), status, detail, line: n + 1 }));
        }
        found.sort_by_key(|f| f.0);
        for (_, c) in found {
            if !out.iter().any(|o| o.kind == c.kind && o.cited == c.cited && o.status == c.status) {
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// Incident ids shaped like the ones in the graph (`INC-5377` when the graph holds `INC-` ids),
/// so `ISO-8601` is not taken for one.
fn incident_pattern(s: &Snapshot, ids: &Ids) -> anyhow::Result<Option<Regex>> {
    let (Some(c), Some(idp)) = (ids.incident, s.iri(inc::ID)?) else { return Ok(None) };
    let mut prefixes: Vec<String> = Vec::new();
    for i in s.subjects(ids.ty, c)? {
        for v in s.objects(i, idp)? {
            if let Some((p, n)) = correlate::literal(s, v).as_deref().and_then(|l| l.rsplit_once('-'))
                && !p.is_empty()
                && n.chars().all(|c| c.is_ascii_digit())
                && !prefixes.contains(&p.to_owned())
            {
                prefixes.push(p.to_owned());
            }
        }
    }
    if prefixes.is_empty() {
        return Ok(None);
    }
    let alts: Vec<String> = prefixes.iter().map(|p| regex::escape(p)).collect();
    Ok(Some(Regex::new(&format!(r"(?i)\b(?:{})-\d+\b", alts.join("|")))?))
}

fn chain(s: &Snapshot, ids: &Ids, iri: &str, rank: Option<i64>) -> anyhow::Result<(Status, String)> {
    let (rank_p, leads) = (s.iri(corr::RANK)?, s.iri(corr::LEADS_TO)?);
    let node = s.iri(iri)?;
    let now = |c: u64| -> Option<i64> { s.object(c, rank_p?).ok()?.and_then(|v| correlate::literal(s, v)?.parse().ok()) };
    if let Some(c) = node.filter(|&c| now(c).is_some()) {
        let r = now(c).unwrap_or_default();
        let app = s.iri(corr::ROOT_CAUSE)?.and_then(|p| s.object(c, p).ok().flatten()).and_then(|e| s.object(e, ids.on).ok().flatten()).map(|a| app_name(s, ids, a)).unwrap_or_default();
        return Ok(match rank {
            Some(cited) if cited != r => (Status::Stale, format!("cited at rank {cited}, now rank {r} ({app})")),
            _ => (Status::Present, format!("rank {r} ({app})")),
        });
    }
    let local = iri.rsplit('/').next().unwrap_or_default();
    let Some((incident, tail)) = local.rsplit_once('-') else { return Ok((Status::Unknown, "not a chain id".into())) };
    if tail.len() < 16 && tail.chars().all(|c| c.is_ascii_digit()) {
        return Ok(match crate::workspace::find_chain(s, iri) {
            Ok(c) => (Status::Present, format!("the chain at rank {tail} now, {}", vocab::compact(&correlate::literal(s, c).unwrap_or_default()))),
            Err(_) => (Status::Unknown, format!("{incident} has no chain at rank {tail}")),
        });
    }
    let inc_iri = crate::model::entity("inc", incident);
    let Some(i) = s.iri(inc_iri.as_str())?.filter(|&i| ids.incident.is_some_and(|c| s.contains([i, ids.ty, c]).unwrap_or(false))) else {
        return Ok((Status::Unknown, format!("no incident {incident}")));
    };
    if node.is_some_and(|c| leads.is_some_and(|l| s.contains([c, l, i]).unwrap_or(false))) {
        return Ok((Status::Stale, "no longer produced; a verdict still names it".into()));
    }
    // A chain id is a digest of its incident and root event, so an id that some event in the
    // graph would produce names a hypothesis correlation once could make, and one that no event
    // produces was never a chain of this graph.
    for q in s.quads([None, Some(ids.at), None], GraphSel::Union) {
        let e = q?[0];
        if let Some(ev) = correlate::literal(s, e)
            && correlate::chain_iri(inc_iri.as_str(), &ev) == iri
        {
            return Ok((Status::Stale, format!("no longer produced: its root {} is no longer a candidate", vocab::compact(&ev))));
        }
    }
    Ok((Status::Unknown, format!("no event of {incident} roots a chain with this id")))
}
