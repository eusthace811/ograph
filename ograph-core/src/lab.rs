//! `ograph lab`: one self-contained HTML page over the graph (incidents, ranked causes with their
//! chains, the timeline before each incident, blast radius and similar incidents). The data is
//! embedded as JSON, so the page needs no server and can be attached to a ticket.

use crate::correlate::{self, Ids, Settings};
use crate::query;
use crate::store::Snapshot;
use serde_json::json;

pub fn page(s: &Snapshot, st: &Settings) -> anyhow::Result<String> {
    let ids = Ids::new(s)?;
    let mut incidents = Vec::new();
    if let Some(c) = ids.incident {
        for i in s.subjects(ids.ty, c)? {
            let iri = correlate::literal(s, i).unwrap_or_default();
            let Ok(mut a) = correlate::analyze(s, &ids, i, st) else { continue };
            a.apps.truncate(5);
            let before = query::before(s, &iri, st.window_s, st.depth).ok();
            let similar = query::similar(s, &iri, st, 3).unwrap_or_default();
            let blast = a.apps.first().and_then(|top| query::blast(s, &top.app, 8).ok());
            incidents.push(json!({"analysis": a, "before": before, "similar": similar, "blast": blast}));
        }
    }
    incidents.sort_by_key(|v| std::cmp::Reverse(v["analysis"]["at_ms"].as_i64().unwrap_or(0)));
    let data = serde_json::to_string(&json!({"incidents": incidents, "triples": s.union_len()?, "graphs": s.graphs()?.len()}))?.replace('<', "\\u003c");
    Ok(TEMPLATE.replace("__DATA__", &data))
}

const TEMPLATE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>ograph lab</title>
<style>
:root{--bg:#fbfbfa;--fg:#1d1d1b;--mute:#6b6b66;--line:#e3e2dd;--card:#fff;--accent:#2f5fb3;--warn:#b5482b}
@media (prefers-color-scheme:dark){:root{--bg:#161615;--fg:#ecebe6;--mute:#a09f98;--line:#2e2e2b;--card:#1f1f1d;--accent:#7ea6ec;--warn:#e88a6c}}
body{margin:0;background:var(--bg);color:var(--fg);font:14px/1.5 system-ui,sans-serif}
main{max-width:980px;margin:0 auto;padding:24px 16px}h1{font-size:20px;margin:0 0 4px}h2{font-size:16px;margin:0}
.mute{color:var(--mute)}.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:16px;margin:16px 0}
table{border-collapse:collapse;width:100%;font-size:13px}td,th{text-align:left;padding:4px 8px;border-bottom:1px solid var(--line);vertical-align:top}
.bar{height:6px;background:var(--accent);border-radius:3px}.chip{display:inline-block;padding:0 6px;border:1px solid var(--line);border-radius:6px;margin:1px;font-size:12px}
details{margin-top:8px}summary{cursor:pointer;color:var(--accent)}code{font-size:12px;overflow-wrap:anywhere}
</style></head><body><main>
<h1>ograph lab</h1><p class="mute" id="meta"></p><div id="list"></div>
<script>
const D = __DATA__;
const esc = s => String(s ?? "").replace(/[&<>"]/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;"}[c]));
document.getElementById("meta").textContent = `${D.incidents.length} incidents, ${D.triples} distinct triples in ${D.graphs} named graphs. Every cause below is a hypothesis with its evidence.`;
document.getElementById("list").innerHTML = D.incidents.map(x => {
  const a = x.analysis, top = (a.apps[0] || {}).score || 1;
  const ranks = a.apps.map((r, i) => `<tr><td>${i + 1}</td><td><b>${esc(r.app)}</b></td><td style="width:30%"><div class="bar" style="width:${Math.max(4, 100 * r.score / top)}%"></div></td><td>${r.score.toFixed(3)}</td><td>${r.chain ? [r.chain.root, ...r.chain.links].map(s => `<span class="chip">${esc(s.app_name)} ${esc(s.kind)}</span>`).join(" → ") + " → incident" : ""}</td></tr>`).join("");
  const before = (x.before?.events || []).slice(0, 25).map(e => `<tr><td>-${e.minutes_before} min</td><td>${esc(e.app)}</td><td>${esc(e.signal)}</td><td>${esc(e.severity)}</td><td><code>${esc(e.source_record || e.event)}</code></td></tr>`).join("");
  const sim = x.similar.map(s => `<tr><td>${s.similarity}</td><td>${esc(s.incident)}</td><td>${esc(s.cause_apps.join(", "))}</td><td>${esc(s.mitigations.join("; "))}</td></tr>`).join("");
  const blast = x.blast ? `${esc(x.blast.app)} is depended on by ${x.blast.dependents.length} applications; processes: ${x.blast.processes.map(p => esc(p[0])).join(", ") || "none recorded"}` : "";
  return `<section class="card"><h2>${esc(a.incident.split("/").pop())} on ${esc(a.app)}</h2><p class="mute">${new Date(a.at_ms).toISOString()} · ${a.candidates} candidate events</p>
  <table><tr><th>#</th><th>candidate cause</th><th></th><th>score</th><th>chain</th></tr>${ranks}</table>
  <p>${blast}</p>
  <details><summary>Timeline before the incident</summary><table><tr><th>when</th><th>app</th><th>signal</th><th>strength</th><th>record</th></tr>${before}</table></details>
  <details><summary>Similar past incidents</summary><table><tr><th>similarity</th><th>incident</th><th>accepted cause</th><th>mitigation</th></tr>${sim}</table></details></section>`;
}).join("");
</script></main></body></html>
"#;
