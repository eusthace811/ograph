//! `ograph lab`: one self-contained HTML page over the graph. Per incident: ranked causes with
//! their chains, a timeline with one lane per application, the dependency neighbourhood, the
//! provenance of every chain event with the chain's verdict, and similar past incidents. The
//! page is rendered here, SVG included, so it reads without a server, a network or scripts; the
//! small script only opens, filters and copies.

use crate::correlate::{self, Analysis, Chain, Ids, Settings, Signal};
use crate::query;
use crate::store::Snapshot;
use crate::term::Id;
use crate::vocab::{self, evt, rca};
use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt::Write;

pub fn page(s: &Snapshot, st: &Settings) -> anyhow::Result<String> {
    let ids = Ids::new(s)?;
    let mut sections = Vec::new();
    if let Some(c) = ids.incident {
        for i in s.subjects(ids.ty, c)? {
            if let Ok(x) = section(s, &ids, i, st, sections.len()) {
                sections.push(x);
            }
        }
    }
    sections.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let meta = format!(
        "{} incidents, {} distinct triples in {} named graphs. Every cause below is a hypothesis with its evidence.",
        sections.len(),
        s.union_len()?,
        s.graphs()?.len()
    );
    let body: String = sections.into_iter().map(|(_, h)| h).collect();
    Ok(TEMPLATE.replace("__META__", &meta).replace("__BODY__", &body))
}

/// What the page shows of one event: the signal and the record it was read from.
struct Event {
    sig: Signal,
    record: Option<String>,
}

fn section(s: &Snapshot, ids: &Ids, i: Id, st: &Settings, n: usize) -> anyhow::Result<(i64, String)> {
    let iri = correlate::literal(s, i).unwrap_or_default();
    let mut a = query::chains(s, ids, i, st)?;
    a.apps.truncate(5);
    let (app, t, cands) = correlate::candidates(s, ids, i, st)?;
    let src = s.iri(evt::SOURCE_RECORD)?;
    let events: Vec<Event> = cands
        .into_iter()
        .map(|sig| {
            let record = src.and_then(|p| s.object(sig.id, p).ok().flatten()).and_then(|v| correlate::literal(s, v));
            Event { sig, record }
        })
        .collect();
    let similar = query::similar(s, &iri, st, 3).unwrap_or_default();
    let top = a.apps.first().and_then(|r| Some((query::lookup(s, &r.app).ok()?, r)));
    let blast = top.and_then(|(_, r)| query::blast(s, &r.app, 8, Some(t)).ok());
    let chain = a.apps.first().and_then(|r| r.chain.as_ref());

    let mut h = String::new();
    write!(
        h,
        r#"<section class="card" id="i{n}" data-incident="{}"><h2>{} on {}</h2><p class="mute">{} · {} candidate events in the {} minutes before</p>"#,
        esc(&vocab::compact(&iri)),
        esc(iri.rsplit('/').next().unwrap_or("")),
        esc(&a.app),
        crate::term::format_millis(t),
        events.len(),
        st.window_s / 60
    )?;
    h += &ranked(&a, n);
    h += r#"<h3>Timeline</h3><p class="mute">One lane per application, nearest dependency first. Bars are episodes, the line is the incident, arrows are the top chain. Click an arrow or a chip for its evidence.</p>"#;
    h += &timeline(&events, &a.app, t, chain, n);
    h += r#"<h3>Dependency neighbourhood</h3><p class="mute">Callers on the left, dependencies on the right. Filled: applications with events. Outlined in red: the top chain. Click an application to filter the timeline.</p>"#;
    h += &hood(s, ids, (app, t), st.depth, top.map(|(x, _)| x), &events, chain)?;
    if let Some(b) = &blast {
        let procs: Vec<&str> = b.processes.iter().map(|p| p.0.as_str()).collect();
        write!(h, "<p>{} is depended on by {} applications; processes: {}.</p>", esc(&b.app), b.dependents.len(), esc(&if procs.is_empty() { "none recorded".into() } else { procs.join(", ") }))?;
    }
    h += &inspector(s, &a, t, n)?;
    h += "<details><summary>Events before the incident</summary><div class=\"scroll\"><table><tr><th>when</th><th>app</th><th>signal</th><th>strength</th><th>record</th></tr>";
    for e in events.iter().rev() {
        write!(
            h,
            "<tr data-app=\"{}\"><td>-{} min</td><td>{}</td><td>{}</td><td>{:.2}</td><td><code>{}</code></td></tr>",
            esc(&e.sig.app_name),
            minutes(t - e.sig.at_ms),
            esc(&e.sig.app_name),
            esc(&e.sig.kind),
            e.sig.severity,
            esc(e.record.as_deref().unwrap_or(&vocab::compact(&e.sig.iri)))
        )?;
    }
    h += "</table></div></details><details><summary>Similar past incidents</summary><div class=\"scroll\"><table><tr><th>similarity</th><th>incident</th><th>accepted cause</th><th>mitigation</th></tr>";
    for x in &similar {
        write!(h, "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", x.similarity, esc(&x.incident), esc(&x.cause_apps.join(", ")), esc(&x.mitigations.join("; ")))?;
    }
    h += "</table></div></details></section>";
    Ok((a.at_ms, h))
}

fn ranked(a: &Analysis, n: usize) -> String {
    let top = a.apps.first().map_or(1.0, |r| r.score).max(1e-9);
    let mut h = String::from(r#"<div class="scroll"><table><tr><th>#</th><th>candidate cause</th><th></th><th>score</th><th>chain</th></tr>"#);
    for (k, r) in a.apps.iter().enumerate() {
        let chips = r.chain.as_ref().map_or(String::new(), |c| {
            let mut x: Vec<String> = chain_events(c).enumerate().map(|(j, e)| format!(r#"<button class="chip" data-open="c{n}-{k}-{j}">{} {}</button>"#, esc(&e.app_name), esc(&e.kind))).collect();
            x.push(format!(r#"<button class="chip" data-open="c{n}-{k}">incident</button>"#));
            x.join(" → ")
        });
        let _ = write!(
            h,
            r#"<tr><td>{}</td><td><b>{}</b></td><td style="width:20%"><div class="bar" style="width:{:.0}%"></div></td><td>{:.3}</td><td>{chips}</td></tr>"#,
            k + 1,
            esc(&r.app),
            (100.0 * r.score / top).max(4.0),
            r.score
        );
    }
    h + "</table></div>"
}

fn chain_events(c: &Chain) -> impl Iterator<Item = &Signal> {
    std::iter::once(&c.root).chain(&c.links)
}

const W: f64 = 760.0;
const LABEL: f64 = 130.0;
const AXIS: f64 = 20.0;
const PITCH: f64 = 13.0;

/// Minutes before the incident on the x axis, with the incident at the right as the zero line.
/// Overlapping episodes of one application get rows of their own inside its lane, so every event
/// stays visible.
fn timeline(events: &[Event], incident_app: &str, t: i64, chain: Option<&Chain>, n: usize) -> String {
    let mut lanes: Vec<(usize, &str)> = events.iter().map(|e| (e.sig.distance, e.sig.app_name.as_str())).chain([(0, incident_app)]).collect();
    lanes.sort();
    lanes.dedup();
    let before = |ms: i64| (t - ms) as f64 / 60_000.0;
    let hi = nice(events.iter().map(|e| before(e.sig.at_ms)).fold(1.0, f64::max));
    let lo = events.iter().map(|e| before(e.sig.until_ms)).fold(0.0, f64::min);
    let x = |m: f64| LABEL + (hi - m) / (hi - lo) * (W - LABEL - 12.0);
    let hot: FxHashSet<&str> = chain.map(|c| chain_events(c).map(|e| e.iri.as_str()).collect()).unwrap_or_default();
    let mut body = String::new();
    let mut at: FxHashMap<&str, (f64, f64)> = FxHashMap::default();
    let mut lane_mid: FxHashMap<&str, f64> = FxHashMap::default();
    let mut top = AXIS;
    for (d, app) in &lanes {
        let mut mine: Vec<&Event> = events.iter().filter(|e| e.sig.app_name == *app).collect();
        mine.sort_by_key(|e| (e.sig.at_ms, e.sig.until_ms));
        let mut ends: Vec<f64> = Vec::new();
        let mut marks = String::new();
        for e in mine {
            let x1 = x(before(e.sig.at_ms));
            let x2 = x(before(e.sig.until_ms)).max(x1 + 3.0);
            let r = ends.iter().position(|&end| end + 2.0 < x1).unwrap_or(ends.len());
            if r == ends.len() {
                ends.push(x2);
            } else {
                ends[r] = x2;
            }
            let y = top + 5.0 + r as f64 * PITCH;
            at.insert(&e.sig.iri, (x1, y + 5.0));
            let _ = write!(
                marks,
                r#"<rect class="ev{}" data-ev="{}" x="{:.0}" y="{:.0}" width="{:.0}" height="10" opacity="{:.1}"><title>{} {} at -{} min</title></rect>"#,
                if hot.contains(e.sig.iri.as_str()) { " hot" } else { "" },
                esc(&vocab::compact(&e.sig.iri)),
                x1,
                y,
                x2 - x1,
                0.45 + 0.55 * e.sig.severity.min(1.0),
                esc(app),
                esc(&e.sig.kind),
                minutes(t - e.sig.at_ms)
            );
        }
        lane_mid.insert(app, top + 10.0);
        let _ = write!(
            body,
            r#"<g class="lane" data-app="{}"><line class="grid" x1="0" x2="{W}" y1="{top:.0}" y2="{top:.0}"/><text class="label" x="4" y="{:.0}">{} <tspan class="mute">d{d}</tspan></text>{marks}</g>"#,
            esc(app),
            top + 14.0,
            esc(&clip(app, 16))
        );
        top += 10.0 + ends.len().max(1) as f64 * PITCH;
    }
    let height = top + 4.0;
    let mut h = format!(r#"<div class="scroll"><svg class="timeline" width="{W}" height="{height:.0}" viewBox="0 0 {W} {height:.0}" role="img" aria-label="timeline"><defs><marker id="ah{n}" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto"><path class="head" d="M0,0L8,4L0,8z"/></marker></defs>"#);
    let step = [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 240.0].into_iter().find(|s| (hi - lo) / s <= 8.0).unwrap_or(480.0);
    let mut m = step;
    while m <= hi {
        let _ = write!(h, r#"<line class="grid" x1="{0:.0}" x2="{0:.0}" y1="{AXIS}" y2="{height:.0}"/><text class="tick" x="{0:.0}" y="12" text-anchor="middle">-{m}m</text>"#, x(m));
        m += step;
    }
    h += &body;
    let _ = write!(h, r#"<line class="zero" x1="{0:.0}" x2="{0:.0}" y1="{AXIS}" y2="{height:.0}"/><text class="tick" x="{0:.0}" y="12" text-anchor="middle">incident</text>"#, x(0.0));
    if let Some(c) = chain {
        let mut points: Vec<(f64, f64)> = chain_events(c).filter_map(|e| at.get(e.iri.as_str()).copied()).collect();
        points.push((x(0.0), lane_mid[incident_app]));
        for (j, w) in points.windows(2).enumerate() {
            let ((x1, y1), (x2, y2)) = (w[0], w[1]);
            let lift = if (y1 - y2).abs() < 1.0 { 12.0 } else { 0.0 };
            let target = if j + 2 < points.len() { format!("c{n}-0-{}", j + 1) } else { format!("c{n}-0") };
            let _ = write!(
                h,
                r#"<path class="hop" data-hop="{j}" data-open="{target}" d="M{x1:.0},{y1:.0} Q{:.0},{:.0} {x2:.0},{y2:.0}" marker-end="url(#ah{n})"><title>link {}</title></path>"#,
                (x1 + x2) / 2.0,
                (y1 + y2) / 2.0 - lift,
                j + 1
            );
        }
    }
    h + "</svg></div>"
}

/// The dependencies of the incident's application (`anchor`, with its opening time) up to
/// `depth` by distance, plus the callers of the top cause (its blast radius) placed by how far
/// they call from it, through the dependencies that held when it opened. No force layout: a
/// column per distance, names sorted within it.
fn hood(s: &Snapshot, ids: &Ids, (app, at): (Id, i64), depth: usize, top: Option<Id>, events: &[Event], chain: Option<&Chain>) -> anyhow::Result<String> {
    let mut col: FxHashMap<Id, i64> = correlate::downstream(s, ids, app, depth, Some(at))?.into_iter().map(|(x, d)| (x, d as i64)).collect();
    if let Some(top) = top {
        let base = *col.entry(top).or_insert(1);
        for (x, d) in correlate::upstream(s, ids, top, 8, Some(at))? {
            col.entry(x).or_insert(base - d as i64);
        }
    }
    let mut edges = Vec::new();
    for &x in col.keys() {
        for y in ids.callees(s, x, Some(at))? {
            if col.contains_key(&y) && x != y {
                edges.push((x, y));
            }
        }
    }
    let busy: FxHashSet<Id> = events.iter().map(|e| e.sig.app).collect();
    let mut hot_apps: FxHashSet<Id> = FxHashSet::default();
    let mut hot_edges: FxHashSet<(Id, Id)> = FxHashSet::default();
    if let Some(c) = chain {
        let path: Vec<Id> = chain_events(c).map(|e| e.app).chain([app]).collect();
        hot_apps.extend(&path);
        for w in path.windows(2) {
            hot_edges.insert((w[1], w[0]));
        }
    }
    let mut names: Vec<(i64, String, Id)> = col.iter().map(|(&x, &c)| (c, correlate::app_name(s, ids, x), x)).collect();
    names.sort();
    let cols: Vec<i64> = {
        let mut c: Vec<i64> = names.iter().map(|n| n.0).collect();
        c.dedup();
        c
    };
    let (bw, bh, cw, rh) = (136.0, 22.0, 176.0, 30.0);
    let mut pos: FxHashMap<Id, (f64, f64)> = FxHashMap::default();
    let mut rows: FxHashMap<i64, usize> = FxHashMap::default();
    for (c, _, x) in &names {
        let r = rows.entry(*c).or_default();
        let ci = cols.iter().position(|k| k == c).unwrap_or(0);
        pos.insert(*x, (8.0 + ci as f64 * cw, 8.0 + *r as f64 * rh));
        *r += 1;
    }
    let width = 8.0 + cols.len() as f64 * cw;
    let height = 8.0 + rows.values().copied().max().unwrap_or(1) as f64 * rh;
    let mut h = format!(r#"<div class="scroll"><svg class="hood" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-label="dependency neighbourhood">"#);
    for (x, y) in &edges {
        let ((x1, y1), (x2, y2)) = (pos[x], pos[y]);
        let (y1, y2) = (y1 + bh / 2.0, y2 + bh / 2.0);
        let d = if x2 > x1 {
            format!("M{:.0},{y1:.0} L{x2:.0},{y2:.0}", x1 + bw)
        } else if x2 < x1 {
            format!("M{x1:.0},{y1:.0} L{:.0},{y2:.0}", x2 + bw)
        } else {
            format!("M{0:.0},{y1:.0} Q{1:.0},{2:.0} {0:.0},{y2:.0}", x1 + bw, x1 + bw + 24.0, (y1 + y2) / 2.0)
        };
        let _ = write!(h, r#"<path class="dep{}" d="{d}"/>"#, if hot_edges.contains(&(*x, *y)) { " hot" } else { "" });
    }
    for (c, name, x) in &names {
        let (px, py) = pos[x];
        let class = [("has", busy.contains(x)), ("hot", hot_apps.contains(x)), ("inc", *x == app)].iter().filter(|k| k.1).map(|k| k.0).collect::<Vec<_>>().join(" ");
        let count = events.iter().filter(|e| e.sig.app == *x).count();
        let _ = write!(
            h,
            r#"<g class="node {class}" data-app="{name}" data-filter="{name}"><rect x="{px:.1}" y="{py:.1}" width="{bw}" height="{bh}" rx="5"/><text x="{tx:.1}" y="{ty:.1}">{label}</text><title>{name}: distance {c}, {count} events</title></g>"#,
            name = esc(name),
            tx = px + 6.0,
            ty = py + 15.0,
            label = esc(&clip(name, 18))
        );
    }
    Ok(h + "</svg></div>")
}

/// For each ranked chain: its verdict, the commands to record one, and per event `why` (the
/// harvest graphs that assert it, their source, extractor and time, and model provenance).
fn inspector(s: &Snapshot, a: &Analysis, t: i64, n: usize) -> anyhow::Result<String> {
    let mut h = String::from("<h3>Chain evidence</h3>");
    let mut seen: FxHashMap<&str, String> = FxHashMap::default();
    for (k, r) in a.apps.iter().enumerate() {
        let Some(c) = &r.chain else { continue };
        let id = vocab::compact(&c.id);
        let verdict = verdict(s, &c.id)?;
        write!(
            h,
            r#"<details class="chain" id="c{n}-{k}"><summary>#{} {} chain, confidence {:.3} · {}</summary><p class="mute"><code>{}</code></p>"#,
            k + 1,
            esc(&r.app),
            c.confidence,
            esc(&verdict.as_ref().map_or("no verdict yet".into(), |v| v.0.clone())),
            esc(&id)
        )?;
        if let Some(v) = &verdict {
            write!(h, "<p>Verdict: {}</p>", esc(&v.1))?;
        }
        if c.scores.len() > 1 {
            write!(h, "<p>Weakest link: {} to {}, score {:.3}</p>", esc(&c.weakest.from), esc(&c.weakest.to), c.weakest.score)?;
        }
        write!(h, r#"<p class="cmd"><code>ograph verdict {} accept --evidence TICKET --by "$USER"</code> <button data-copy="accept">copy</button> <button data-copy="reject">copy as reject</button></p>"#, esc(&id))?;
        h += "<ol>";
        for (j, e) in chain_events(c).enumerate() {
            write!(h, r#"<li id="c{n}-{k}-{j}"><b>{} {}</b> at -{} min, strength {:.2}"#, esc(&e.app_name), esc(&e.kind), minutes(t - e.at_ms), e.severity)?;
            if let Some(l) = c.scores.get(j) {
                let basis = if l.known_pair { "known pair".to_owned() } else { format!("time {:.2}, severity {:.2}", l.time, l.severity) };
                write!(h, "<br>link to {}: score {:.3} ({basis})", esc(&l.to), l.score)?;
            }
            match seen.get(e.iri.as_str()) {
                Some(first) => write!(h, r#" <button class="chip" data-open="{first}">evidence as above</button>"#)?,
                None => {
                    h += &why(s, &e.iri);
                    seen.insert(&e.iri, format!("c{n}-{k}-{j}"));
                }
            }
            h += "</li>";
        }
        h += "</ol></details>";
    }
    Ok(h)
}

/// The chain's current verdict as (state, sentence), the last in `rca:supersededBy` order.
fn verdict(s: &Snapshot, chain: &str) -> anyhow::Result<Option<(String, String)>> {
    let Some(cid) = s.iri(chain)? else { return Ok(None) };
    let Some(v) = query::verdicts(s, cid)?.pop() else { return Ok(None) };
    let get = |p: &str| -> Vec<String> { s.iri(p).ok().flatten().and_then(|p| s.objects(v, p).ok()).unwrap_or_default().into_iter().filter_map(|o| correlate::literal(s, o)).collect() };
    let state = get(rca::STATE).join(", ");
    let sentence = format!("{state} by {} at {}, evidence: {}", get(rca::BY).join(", "), get(rca::AT).join(", "), get(rca::EVIDENCE).join("; "));
    Ok(Some((state, sentence)))
}

fn why(s: &Snapshot, iri: &str) -> String {
    let Ok(p) = query::why(s, iri) else { return String::new() };
    let mut h = String::from("<div class=\"why\"><p>");
    let mut graphs: Vec<&query::GraphInfo> = Vec::new();
    let mut models = Vec::new();
    for f in &p.facts {
        let _ = write!(h, "<span class=\"mute\">{}</span> <code>{}</code><br>", esc(&f.predicate), esc(&f.object));
        for g in &f.graphs {
            if !graphs.iter().any(|x| x.graph == g.graph) {
                graphs.push(g);
            }
        }
        if let Some(m) = &f.model
            && !models.contains(m)
        {
            models.push(m.clone());
        }
    }
    h += "</p>";
    for g in graphs {
        let o = |x: &Option<String>| x.clone().unwrap_or_else(|| "?".into());
        let _ = write!(h, "<p class=\"mute\">From <code>{}</code>: source {}, extractor {}, harvested {} ({}).</p>", esc(&g.graph), esc(&o(&g.source)), esc(&o(&g.extractor)), esc(&o(&g.harvested_at)), esc(&o(&g.status)));
    }
    for (model, hash, conf) in models {
        let _ = write!(h, "<p class=\"mute\">Proposed by model {}, confidence {}, prompt {}.</p>", esc(&model), esc(&conf), esc(&hash));
    }
    if !p.quarantined.is_empty() {
        let _ = write!(h, "<p class=\"mute\">{} proposals about it were quarantined.</p>", p.quarantined.len());
    }
    h + "</div>"
}

fn minutes(ms: i64) -> f64 {
    (ms as f64 / 6_000.0).round() / 10.0
}

/// The smallest of 5, 10, 15, 30, 60... minutes that holds `m`.
fn nice(m: f64) -> f64 {
    [5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 240.0, 480.0, 1440.0].into_iter().find(|&x| x >= m).unwrap_or(m.ceil())
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const TEMPLATE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>ograph lab</title>
<style>
:root{--bg:#fbfbfa;--fg:#1d1d1b;--mute:#6b6b66;--line:#e3e2dd;--card:#fff;--accent:#2f5fb3;--tint:#dbe5f6;--warn:#b5482b}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ecebe6;--mute:#a09f98;--line:#2e2e2b;--card:#1f1f1d;--accent:#7ea6ec;--tint:#26354f;--warn:#e88a6c}}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 system-ui,sans-serif}
main{max-width:980px;margin:0 auto;padding:24px 16px}h1{font-size:20px;margin:0 0 4px}h2{font-size:16px;margin:0}h3{font-size:14px;margin:16px 0 0}
.mute{color:var(--mute)}.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:16px;margin:16px 0;min-width:0}
.scroll{overflow-x:auto;max-width:100%}table{border-collapse:collapse;width:100%;font-size:13px}td,th{text-align:left;padding:4px 8px;border-bottom:1px solid var(--line);vertical-align:top}
.bar{height:6px;background:var(--accent);border-radius:3px}
.chip{white-space:nowrap;font:inherit;font-size:12px;color:inherit;background:none;padding:0 6px;border:1px solid var(--line);border-radius:6px;margin:1px;cursor:pointer}.chip:hover{border-color:var(--accent)}
details{margin-top:8px}summary{cursor:pointer;color:var(--accent)}code{font-size:12px;overflow-wrap:anywhere}
.focus{outline:2px solid var(--warn);outline-offset:2px;border-radius:4px}.why{font-size:12px}
.cmd button{font:inherit;font-size:12px;cursor:pointer}
svg text{fill:var(--fg);font-size:11px}svg .mute,svg .tick{fill:var(--mute)}.grid{stroke:var(--line)}.zero{stroke:var(--warn);stroke-width:2}
.ev{fill:var(--accent)}.ev.hot{fill:var(--warn)}.hop{stroke:var(--warn);stroke-width:1.6;fill:none;cursor:pointer}.hop:hover{stroke-width:3}.head{fill:var(--warn)}
.node{cursor:pointer}.node rect{fill:var(--card);stroke:var(--line)}.node.has rect{fill:var(--tint)}.node.hot rect{stroke:var(--warn);stroke-width:2}.node.inc text{font-weight:700}
.dep{stroke:var(--mute);stroke-width:1;fill:none}.dep.hot{stroke:var(--warn);stroke-width:2.5}.dim{opacity:.15}@media (max-width:600px){.bar{display:none}}tr.dim{display:none}
</style></head><body><main>
<h1>ograph lab</h1><p class="mute">__META__</p>
__BODY__
<script>
document.addEventListener("click", ev => {
  const el = ev.target.closest("[data-open],[data-filter],[data-copy]");
  if (!el) return;
  if (el.dataset.open) {
    const t = document.getElementById(el.dataset.open);
    if (!t) return;
    for (let d = t; d; d = d.parentElement?.closest("details")) if (d.tagName === "DETAILS") d.open = true;
    document.querySelectorAll(".focus").forEach(x => x.classList.remove("focus"));
    t.classList.add("focus");
    t.scrollIntoView({block: "nearest", behavior: "smooth"});
  } else if (el.dataset.filter) {
    const sec = el.closest("section"), app = sec.dataset.only === el.dataset.filter ? "" : el.dataset.filter;
    sec.dataset.only = app;
    sec.querySelectorAll(".lane[data-app], tr[data-app], .node[data-app]").forEach(x => x.classList.toggle("dim", !!app && x.dataset.app !== app));
  } else {
    const code = el.parentElement.querySelector("code"), text = code.textContent.replace(/ (accept|reject) /, ` ${el.dataset.copy} `);
    code.textContent = text;
    const done = () => { el.textContent = "copied"; };
    const fallback = () => {
      const r = document.createRange(); r.selectNode(code);
      getSelection().removeAllRanges(); getSelection().addRange(r); document.execCommand("copy"); done();
    };
    navigator.clipboard ? navigator.clipboard.writeText(text).then(done, fallback) : fallback();
  }
});
</script></main></body></html>
"#;
