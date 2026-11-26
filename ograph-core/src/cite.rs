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
    // A SPARQL query escapes `/` and `.` in a prefixed name: `id:ev\/deploy-2\.3\.1`.
    re(&R, r"(?:https://ograph\.dev/id/|\bid:)([a-z][a-z0-9-]*)\\?/((?:[A-Za-z0-9._/-]|\\[./])*[A-Za-z0-9])")
}

/// Words that could be application names: letters, digits and inner `-`, `_` or `.`.
fn tokens() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"[A-Za-z][A-Za-z0-9]*(?:[-_.][A-Za-z0-9]+)*")
}

/// What, after `rank N`, introduces the chain id that follows: `: `, ` is `, ` is: `, ` chain: `,
/// ` is the chain `, ` chain is the `. The rank text before a chain accepts all of it too.
macro_rules! points_on {
    () => {
        r"(?:chain\b\s*)?(?::|(?:is|was)\b\s*:?|=)\s*(?:the\s+)?(?:chain\b\s*[:=-]?\s*)?(?:(?:is|was)\b\s*)?(?:the\s+)?$"
    };
}

/// Markdown emphasis and dashes, read as plain text: `**rank 1**`, `id:chain/x` — rank 1.
fn plain(s: &str) -> String {
    s.replace(['*', '`'], "").replace(['\u{2013}', '\u{2014}'], "-")
}

/// A rank stated right after a chain id: `is rank 2`, `is ranked 2`, `(rank 2)`, `— rank 2`, `moved from rank 3
/// to rank 2`. `on` is set when the rank ends in `:` or `is`, which can point to what follows.
fn after_rank() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, concat!(r"(?i)^[\s,):|-]*\(?\s*(?P<tied>which\s+(?:is\s+|now\s+)+|(?:(?:is|was|at|now)\s+)+)?(?P<moved>\w+\s+from\s+rank\s*[#-]?\d+\s+to\s+)?(?:the\s+)?rank(?:ed)?\s*[#-]?(?P<rank>\d+)\b\s*(?P<on>", points_on!(), ")?"))
}

/// A rank that points to the chain after it: `x, rank 2: y`. A verb ties it to x instead:
/// `x is rank 2: why`.
fn points_on(c: &regex::Captures) -> bool {
    c.name("on").is_some() && c.name("tied").is_none() && c.name("moved").is_none()
}

/// The rank a sentence says a chain has now, after saying what it was: `was rank 2 before the
/// deploy and is now rank 1`. The clause ends at `;` or a `.` that ends a sentence, not one in
/// `0.91` or `v1.4`.
fn now_rank(after: &str, before_chain: bool) -> Option<i64> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = re(&R, r"(?i)^(?:[^.;]|\.\S)*?\bwas\s+(?:at\s+)?(?:the\s+)?rank(?:ed)?\s*[#-]?\d+\b(?P<mid>(?:[^.;]|\.\S)*?)\b(?:(?P<is>is\s+)?now|now\s+(?P<is2>is))\s+(?:at\s+)?(?:rank(?:ed)?\s*[#-]?)(?P<now>\d+)\b");
    let c = r.captures(after)?;
    // Another rank in between is another subject's: `x was rank 1, and y was rank 3 but is now 2`.
    static MID: OnceLock<Regex> = OnceLock::new();
    if re(&MID, r"(?i)\brank(?:ed)?\s*[#-]?\d").is_match(&c["mid"]) {
        return None;
    }
    // Before a chain with no rank of its own, only `is now rank M` that hands nothing on is ours:
    // `now rank 1 is held by y` and `what is now rank 1: y` are y's.
    static ON: OnceLock<Regex> = OnceLock::new();
    // `the chain that is now rank 1, y` describes y.
    static WHO: OnceLock<Regex> = OnceLock::new();
    let rest = &after[c.get(0)?.end()..];
    let handed_on = re(&ON, r"(?i)^\s*(?:(?:is|was)\b|[:=])").is_match(rest);
    // `that is now rank 1` describes something else, and `isn't now rank 1` denies it.
    let relative = re(&WHO, r"(?i)(?:\b(?:that|which|what|whatever|who|not|never|longer)|n['’]t)\s+$").is_match(&c["mid"]);
    // Before a chain, a `now rank 1` no `is` ties to this one is as likely the next chain's
    // (`now rank 1 belongs to y`) as this one's (`now rank 1, ahead of y`): the stated rank stands.
    // `is now` or `now is` ties the rank to this chain.
    let tied = c.name("is").is_some() || c.name("is2").is_some();
    if relative || (!tied && handed_on) || (before_chain && (!tied || handed_on)) {
        return None;
    }
    c["now"].parse().ok()
}

/// Whether the text after a chain id states its own rank.
fn states_rank(after: &str) -> bool {
    after_rank().captures(&plain(after)).is_some_and(|c| !points_on(&c))
}

/// The rank an answer gives the chain id written between `before` and `after`, the text that
/// separates it from its neighbouring IRIs: `id:chain/x is rank 2`, `id:chain/x (rank 2)`,
/// `id:chain/x — rank 2`, `rank 2: id:chain/x`, `id:chain/x moved from rank 3 to rank 2` or a
/// table cell, in markdown backticks or bold too. A list number like `3. id:chain/x` is no rank,
/// and neither is a rank later in a clause (`not rank 1`, `unlike the rank 1 chain`).
fn cited_rank(before: &str, after: &str, follows_chain: bool, precedes_chain: bool) -> Option<i64> {
    static BEFORE: OnceLock<Regex> = OnceLock::new();
    let b = re(&BEFORE, concat!(r"(?i)\brank\s*[#-]?(\d+)\s*(?:", points_on!(), r"|(?:is|was|[:=,-])?\s*(?:the\s+)?(?:chain\b\s*[:=-]?\s*)?(?:(?:is|was)\s+)?$)"));
    // `x, rank 2: y` and `x, rank 2 is y` give the rank to y, when y states none of its own; in
    // `x is rank 1, y` the rank is x's alone, and only what follows it in the gap can give y one.
    let (before, after) = (plain(before), plain(after));
    let own = |gap: &str, forward: bool| after_rank().captures(gap).filter(|c| !forward || !points_on(c)).map(|c| (c["rank"].parse().ok(), c[0].len(), c.name("moved").is_none() && c.name("tied").is_some_and(|t| t.as_str().to_lowercase().contains("was"))));
    // A rank the chain `was` gives way to the one it has `now`, when the clause says so.
    if let Some((rank, _, was)) = own(&after, precedes_chain) {
        return if was { now_rank(&after, precedes_chain).or(rank) } else { rank };
    }
    let before = match own(&before, true) {
        Some((_, taken, _)) if follows_chain => &before[taken..],
        _ => &before[..],
    };
    b.captures(before)?[1].parse().ok()
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
        // The text between IRI `k` and the next one, or the end of the line.
        let gap_after = |k: usize| &line[matches[k].get(0).unwrap().end()..matches.get(k + 1).map_or(line.len(), |n| n.get(0).unwrap().start())];
        let is_chain = |k: usize| matches.get(k).is_some_and(|m| &m[1] == "chain");
        for (j, m) in matches.iter().enumerate() {
            let whole = m.get(0).unwrap();
            let prev_end = if j == 0 { 0 } else { matches[j - 1].get(0).unwrap().end() };
            spans.push((whole.start(), whole.end()));
            let (kind, local) = (&m[1], m[2].replace('\\', ""));
            let iri = format!("{}{kind}/{local}", vocab::DATA);
            let shown = format!("id:{kind}/{local}");
            let (k, status, detail) = match kind {
                "chain" => {
                    let (st, d) = chain(s, &ids, &iri, cited_rank(&line[prev_end..whole.start()], gap_after(j), j > 0 && is_chain(j - 1), is_chain(j + 1) && !states_rank(gap_after(j + 1))))?;
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
            // A plain word counts only as an exact name: fuzzy matching is for name-like words.
            if !word.contains(['-', '_']) {
                if let Some(a) = catalog.exact(ops::APPLICATION, word) {
                    found.push((m.start(), Citation { kind: Kind::App, cited: word.to_owned(), status: Status::Present, detail: format!("the catalog application {}", vocab::compact(a.as_str())), line: n + 1 }));
                }
                continue;
            }
            let resolved = catalog.explain(ops::APPLICATION, word);
            let (status, detail) = match &resolved.resolution {
                Resolution::Exact(a) => (Status::Present, format!("the catalog application {}", vocab::compact(a.as_str()))),
                Resolution::Fuzzy(a, sc) => (Status::Present, format!("a misspelling of {} ({sc:.2})", vocab::compact(a.as_str()))),
                Resolution::Ambiguous(..) => (Status::Unknown, format!("not in the catalog: {}", resolved.why)),
                Resolution::Missing => match catalog.extended(ops::APPLICATION, word) {
                    Some(a) => (Status::Unknown, format!("not in the catalog: {} with more words", vocab::compact(a.as_str()))),
                    None => continue,
                },
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
            if let Some(p) = correlate::literal(s, v).as_deref().and_then(id_prefix)
                && !prefixes.iter().any(|q| q == p)
            {
                prefixes.push(p.to_owned());
            }
        }
    }
    incident_regex(&prefixes)
}

/// The prefix of an incident id, its leading parts with no digit, when every part after it is
/// letters and digits with a digit: `INC` of `INC-4120`, `INC-011F3B` or `INC-2024-001`, and
/// `SEC-INC` of `SEC-INC-0042`.
fn id_prefix(id: &str) -> Option<&str> {
    let has_digit = |p: &str| p.bytes().any(|c| c.is_ascii_digit());
    let part = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric()) && has_digit(p);
    let cut = id.split('-').take_while(|p| !has_digit(p)).map(|p| p.len() + 1).sum::<usize>();
    let (prefix, rest) = (id.get(..cut.checked_sub(1)?)?, id.get(cut..)?);
    (!prefix.is_empty() && !rest.is_empty() && rest.split('-').all(part)).then_some(prefix)
}

/// Bare ids with one of `prefixes`, each part after it holding a digit.
fn incident_regex(prefixes: &[String]) -> anyhow::Result<Option<Regex>> {
    if prefixes.is_empty() {
        return Ok(None);
    }
    let alts: Vec<String> = prefixes.iter().map(|p| regex::escape(p)).collect();
    Ok(Some(Regex::new(&format!(r"(?i)\b(?:{})(?:-[a-z0-9]*\d[a-z0-9]*)+\b", alts.join("|")))?))
}

fn chain(s: &Snapshot, ids: &Ids, iri: &str, rank: Option<i64>) -> anyhow::Result<(Status, String)> {
    let (rank_p, leads) = (s.iri(corr::RANK)?, s.iri(corr::LEADS_TO)?);
    let node = s.iri(iri)?;
    let now = |c: u64| -> Option<i64> { s.object(c, rank_p?).ok()?.and_then(|v| correlate::literal(s, v)?.parse().ok()) };
    if let Some((c, r)) = node.and_then(|c| now(c).map(|r| (c, r))) {
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

#[cfg(test)]
mod tests {
    #[test]
    fn an_incident_id_may_have_several_parts() {
        assert_eq!([super::id_prefix("INC-4120"), super::id_prefix("INC-011F3B"), super::id_prefix("INC-2024-001")], [Some("INC"); 3]);
        assert_eq!(super::id_prefix("SEC-INC-0042"), Some("SEC-INC"));
        assert_eq!([super::id_prefix("INC-related"), super::id_prefix("4120"), super::id_prefix("-12"), super::id_prefix("INC"), super::id_prefix("INC-")], [None; 5]);
        let r = super::incident_regex(&["INC".to_owned()]).unwrap().unwrap();
        let found: Vec<&str> = r.find_iter("INC-2024-002 matches INC-2025-017, inc-011f3b and INC-related").map(|m| m.as_str()).collect();
        assert_eq!(found, ["INC-2024-002", "INC-2025-017", "inc-011f3b"]);
    }
}
