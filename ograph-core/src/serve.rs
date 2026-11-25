//! `ograph serve`: keeps the store open and answers line-delimited JSON-RPC over TCP. The address
//! goes to `.ograph/serve.json` so clients find it (ogrep's discovery pattern). Source files are
//! polled and re-harvested when they change. With `--mcp` the same tools speak MCP over stdio.

use crate::correlate::{self, RankMode};
use crate::query;
use crate::sparql;
use crate::workspace::Workspace;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

pub fn tools() -> Value {
    let s = |props: Value, req: &[&str]| json!({"type": "object", "properties": props, "required": req});
    let mut tools = json!([
        {"name": "before", "description": "Events in the window before an incident on its application and the applications it depends on, newest first, with their signal type and strength. `chronic` on an event means its series (same application, signal and metric or log template) fired again and again in the correlation window before the incident: background noise, whatever its strength. Start every investigation here.",
         "inputSchema": s(json!({"incident": {"type": "string", "description": "incident id like INC-5377 or an IRI"}, "window": {"type": "string", "description": "like 10m (the default) or 2h"}}), &["incident"])},
        {"name": "chains", "description": "Correlation chains and ranked root-cause applications for an incident. Each chain is a hypothesis with its evidence, not a conclusion. `scores` says how each link scored (time proximity, severity, known pair, where the dependency comes from) and `weakest` names the link to check first. `precedent` on an application means its root has the application and signal of the accepted cause of a similar past incident. `chronic` on a chain event means its series fired again and again before the incident: background, not a new fault.",
         "inputSchema": s(json!({"incident": {"type": "string"}, "rank": {"type": "string", "enum": ["chain", "propagation"]}}), &["incident"])},
        {"name": "blast", "description": "Applications that depend on an application, by distance, the business processes they support, and open incidents among them.",
         "inputSchema": s(json!({"app": {"type": "string"}}), &["app"])},
        {"name": "similar", "description": "Past incidents that resemble this one, with their accepted causes and mitigations, and `ruled_out`: the roots people rejected on each, with their evidence.",
         "inputSchema": s(json!({"incident": {"type": "string"}}), &["incident"])},
        {"name": "code", "description": "Where an incident points in the code: the lines behind the events before it, the commits before it and the located lines each one changed, the files its description is about, and past write-ups that resemble it. Needs a code source; ogrep failures are listed under degraded.",
         "inputSchema": s(json!({"incident": {"type": "string"}, "top": {"type": "integer", "description": "how many files and write-ups to list (default 5)"}}), &["incident"])},
        {"name": "suspects", "description": "Likely causes from the dependency graph and recent commits, for an incident or for failing applications. Use it when before and chains find little: it needs no telemetry, only the repositories ogrep reads. With a code source, chains adds the same list when it ranks no application.",
         "inputSchema": s(json!({"target": {"type": "string", "description": "an incident, or application names separated by commas"}, "since": {"type": "string", "description": "how far back commits count: 24h, 3d or a date"}}), &["target"])},
        {"name": "why", "description": "Provenance of every fact about a node: harvest graph, source, extractor, model and confidence, and quarantined proposals. For an event, `chronic` says when its series fired again and again within the correlation window of it.",
         "inputSchema": s(json!({"node": {"type": "string"}}), &["node"])},
        {"name": "uses", "description": "What depends on a source file, a directory of them or one record (FILE#n), for when a source turns out to be wrong: its facts and the other sources that also state each one, the events and incidents only it states, the chains that would go, be relinked, move or enter without it (with incident and rank), the current verdicts on chains that would go or be relinked, and its quarantined proposals. Paths are relative to the workspace root.",
         "inputSchema": s(json!({"source": {"type": "string"}}), &["source"])},
        {"name": "news", "description": "News that mentions vendors our applications use, with the affected applications and processes.",
         "inputSchema": s(json!({"since": {"type": "string", "description": "only the last DURATION, like 7d (default: all of it)"}}), &[])},
        {"name": "sparql", "description": "Run a read-only SPARQL query. Prefixes ops: evt: inc: disc: corr: rca: harv: id: g: rdf: rdfs: xsd: are predeclared; the default graph is the union of all harvests. A class or property the ontology does not declare is rejected with the nearest declared name.",
         "inputSchema": s(json!({"query": {"type": "string"}, "limit": {"type": "integer"}}), &["query"])},
        {"name": "resolve", "description": "Application names, aliases or IRIs, at most 5, to catalog applications by the harvest's own matching rules: resolved (exact, or a typo the harvest would accept), ambiguous (close to a catalog name but a sibling of it, or tied with another), or unresolved, each with its rationale and the closest candidates. Use it before you name an application you have not seen in a tool answer.",
         "inputSchema": s(json!({"mentions": {"type": "array", "items": {"type": "string"}, "maxItems": crate::query::MAX_MENTIONS}}), &["mentions"])},
        {"name": "cite_check", "description": "Checks the chain ids, incident ids, application names and IRIs in an answer against the graph: present, stale (a chain no longer produced, or cited at a rank it no longer has) or unknown (not in the graph). Run it on a draft answer before you give it.",
         "inputSchema": s(json!({"text": {"type": "string"}}), &["text"])},
        {"name": "verdict", "description": "Record a human-confirmed verdict on a chain. Only call this when a person has confirmed the cause.",
         "inputSchema": s(json!({"chain": {"type": "string"}, "decision": {"type": "string", "enum": ["accept", "reject"]}, "evidence": {"type": "array", "items": {"type": "string"}}, "by": {"type": "string"}}), &["chain", "decision", "evidence", "by"])}
    ]);
    for t in tools.as_array_mut().into_iter().flatten() {
        let read_only = t["name"] != "verdict";
        t["annotations"] = json!({"readOnlyHint": read_only});
    }
    tools
}

pub fn resources() -> Value {
    json!([
        {"uri": "ograph://ontology", "name": "ontology", "mimeType": "text/turtle",
         "description": "The workspace's ontology, .ograph/ontology.ttl: every class and property a fact or a SPARQL query may use."},
        {"uri": "ograph://incidents", "name": "incidents", "mimeType": "application/json",
         "description": "Every incident in the graph with its id, application, open time, title and status, newest first."}
    ])
}

pub fn resource_templates() -> Value {
    json!([
        {"uriTemplate": "ograph://incident/{id}", "name": "incident", "mimeType": "application/json",
         "description": "One incident's investigation in one read: before, chains and similar for the incident, and blast for its top-ranked application (its own application when none ranks)."}
    ])
}

pub fn read_resource(ws: &Workspace, uri: &str) -> anyhow::Result<Value> {
    let (mime, text) = match uri {
        "ograph://ontology" => ("text/turtle", std::fs::read_to_string(ws.dir.join("ontology.ttl")).unwrap_or_else(|_| crate::ontology::DEFAULT_TTL.to_owned())),
        "ograph://incidents" => ("application/json", serde_json::to_string(&incidents(&ws.store.snapshot()?)?)?),
        _ => {
            let id = uri.strip_prefix("ograph://incident/").filter(|id| !id.is_empty()).ok_or_else(|| anyhow::anyhow!("unknown resource {uri}"))?;
            let at = |m: &str, p: Value| call(ws, m, &p).map_err(|e| e.context(format!("{m} for {id}")));
            let before = at("before", json!({"incident": id}))?;
            let chains = at("chains", json!({"incident": id}))?;
            let top = chains.pointer("/apps/0/app").or_else(|| chains.pointer("/analysis/apps/0/app")).or(before.get("app")).and_then(Value::as_str).unwrap_or_default().to_owned();
            let doc = json!({"before": before, "chains": chains, "similar": at("similar", json!({"incident": id}))?, "blast": at("blast", json!({"app": top}))?});
            ("application/json", doc.to_string())
        }
    };
    Ok(json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}))
}

#[derive(serde::Serialize)]
pub struct IncidentSummary {
    pub id: String,
    pub app: String,
    pub at: String,
    pub title: Option<String>,
    pub status: Option<String>,
}

pub fn incidents(s: &crate::store::Snapshot) -> anyhow::Result<Vec<IncidentSummary>> {
    let Ok(ids) = correlate::Ids::new(s) else { return Ok(vec![]) };
    let Some(class) = ids.incident else { return Ok(vec![]) };
    let value = |i: crate::term::Id, p: &str| -> Option<String> { s.object(i, s.iri(p).ok()??).ok()?.and_then(|v| correlate::literal(s, v)) };
    let mut out: Vec<(i64, IncidentSummary)> = Vec::new();
    for i in s.subjects(ids.ty, class)? {
        let Ok((app, ms)) = correlate::incident_anchor(s, &ids, i) else { continue };
        let id = value(i, crate::vocab::inc::ID).unwrap_or_else(|| crate::vocab::compact(&correlate::literal(s, i).unwrap_or_default()));
        out.push((ms, IncidentSummary { id, app: correlate::app_name(s, &ids, app), at: crate::term::format_millis(ms), title: value(i, crate::vocab::inc::TITLE), status: value(i, crate::vocab::inc::STATUS) }));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    Ok(out.into_iter().map(|x| x.1).collect())
}

/// The investigation reference the skill ships, so the prompt and the skill cannot drift.
const INVESTIGATION: &str = include_str!("../../skills/ograph/references/investigation.md");

pub fn prompts() -> Value {
    json!([{"name": "investigate", "description": "Investigate an incident with the ograph tools: the order to call them in and what to report, from the ograph skill's investigation reference.",
            "arguments": [{"name": "incident", "description": "incident id like INC-5377", "required": true}]}])
}

pub fn prompt(name: &str, args: &Value) -> anyhow::Result<Value> {
    anyhow::ensure!(name == "investigate", "unknown prompt {name}");
    let incident = args.get("incident").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("missing argument incident"))?;
    let text = format!("Investigate incident {incident} with the ograph tools. {}", investigation_guide());
    Ok(json!({"description": format!("Investigate {incident}"), "messages": [{"role": "user", "content": {"type": "text", "text": text}}]}))
}

/// The investigation reference without its example output: each section names the tool its
/// `ograph VERB` command maps to and keeps its rules. A section stops at a command that is not
/// an ograph tool, such as the ogrep hand-over, and drops the sentence that introduces it.
pub fn investigation_guide() -> String {
    let names: Vec<String> = tools().as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    let mut out = String::from("The guide below is written for the command line: `ograph VERB` is the tool VERB, and the tools always answer in JSON. Work through the sections in order and cite the tool behind every claim.\n");
    for section in INVESTIGATION.split("\n## ").skip(1) {
        let (title, body) = section.split_once('\n').unwrap_or((section, ""));
        let mut kept: Vec<&str> = Vec::new();
        let mut tool = None;
        for (k, block) in body.split("```").enumerate() {
            if k % 2 == 0 {
                if !block.trim().is_empty() {
                    kept.push(block.trim());
                }
                continue;
            }
            let verb = block.lines().nth(1).and_then(|l| l.strip_prefix("ograph ")).and_then(|l| l.split_whitespace().next());
            match verb.filter(|v| names.iter().any(|n| n == v)) {
                Some(v) => {
                    tool.get_or_insert(v);
                }
                None if block.starts_with("bash") => {
                    if let Some(last) = kept.pop() {
                        let paragraphs: Vec<&str> = last.split("\n\n").collect();
                        let keep = paragraphs.len() - usize::from(last.ends_with(':'));
                        kept.extend(&paragraphs[..keep]);
                    }
                    break;
                }
                None => {}
            }
        }
        let tool = tool.map(|t| format!(" (tool `{t}`)")).unwrap_or_default();
        out.push_str(&format!("\n## {title}{tool}\n\n{}\n", kept.join("\n\n")));
    }
    out
}

fn arg<'a>(p: &'a Value, k: &str) -> anyhow::Result<&'a str> {
    p.get(k).and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("missing parameter {k}"))
}

pub fn call(ws: &Workspace, method: &str, p: &Value) -> anyhow::Result<Value> {
    let snap = ws.store.snapshot()?;
    Ok(match method {
        "before" => serde_json::to_value(query::before(&snap, arg(p, "incident")?, p.get("window").and_then(Value::as_str).map_or(Ok(600), query::duration_s)?, 4, ws.config.correlation.window_s)?)?,
        "chains" => {
            let ids = correlate::Ids::new(&snap)?;
            let mut st = ws.config.correlation.clone();
            if p.get("rank").and_then(Value::as_str) == Some("propagation") {
                st.rank = RankMode::Propagation;
            }
            let mut a = query::chains(&snap, &ids, query::lookup(&snap, arg(p, "incident")?)?, &st)?;
            let (suspects, _) = ws.fallback_suspects(&snap, &a, arg(p, "incident")?, &st);
            a.apps.truncate(5);
            if suspects.is_empty() { serde_json::to_value(a)? } else { serde_json::to_value(query::Chains { analysis: a, suspects, shift: None })? }
        }
        "suspects" => serde_json::to_value(ws.suspects(&snap, arg(p, "target")?, p.get("since").and_then(Value::as_str), &ws.config.correlation)?.0)?,
        "blast" => serde_json::to_value(query::blast(&snap, arg(p, "app")?, 8, None)?)?,
        "similar" => serde_json::to_value(query::similar(&snap, arg(p, "incident")?, &ws.config.correlation, 5)?)?,
        "code" => serde_json::to_value(ws.code(&snap, arg(p, "incident")?, p.get("top").and_then(Value::as_u64).unwrap_or(5) as usize)?)?,
        "why" => serde_json::to_value(query::why(&snap, arg(p, "node")?, Some(ws.config.correlation.window_s))?)?,
        "uses" => {
            let root = std::env::current_dir()?.join(&ws.root);
            serde_json::to_value(query::uses(&ws.store, &ws.ontology, &ws.config.correlation, &query::source_key(&root, &root, arg(p, "source")?))?)?
        }
        "news" => {
            let since = p.get("since").and_then(Value::as_str).map(|d| query::duration_s(d).map(|d| crate::workspace::now_ms() - d * 1000)).transpose()?;
            serde_json::to_value(query::news(&snap, since)?)?
        }
        "sparql" => {
            let q = sparql::check(arg(p, "query")?, &ws.ontology)?;
            serde_json::to_value(sparql::run(&snap, &q, Some(p.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize))?)?
        }
        "verdict" => {
            drop(snap);
            let ev: Vec<String> = p.get("evidence").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().filter(|s| !s.trim().is_empty()).map(str::to_owned)).collect()).unwrap_or_default();
            anyhow::ensure!(!ev.is_empty(), "a verdict needs evidence");
            let accept = match arg(p, "decision")? {
                "accept" => true,
                "reject" => false,
                other => anyhow::bail!("decision must be accept or reject, not {other}"),
            };
            serde_json::to_value(ws.verdict(arg(p, "chain")?, accept, arg(p, "by")?, &ev)?)?
        }
        "harvest" => {
            drop(snap);
            serde_json::to_value(ws.harvest(None, |_, _| {})?)?
        }
        "resolve" => {
            let m: Vec<String> = p.get("mentions").and_then(Value::as_array).ok_or_else(|| anyhow::anyhow!("missing parameter mentions"))?.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect();
            serde_json::to_value(query::resolve(&snap, &m)?)?
        }
        "cite_check" => serde_json::to_value(crate::cite::check(&snap, arg(p, "text")?)?)?,
        "tools" => tools(),
        other => anyhow::bail!("unknown method {other}"),
    })
}

/// Every file an enricher would read and every `.ograph/` setting, with its modification time
/// and size, so an addition, edit or deletion of an input changes the inventory. Nothing else
/// under `.ograph/` counts, so the server's own writes never trigger a harvest.
pub fn inventory(ws: &Workspace) -> Vec<(PathBuf, Option<SystemTime>, u64)> {
    let settings = ["config.toml", "ontology.ttl", "shapes.ttl"].map(|n| ws.dir.join(n));
    let inputs = ws.config.sources.iter().flat_map(|s| crate::enrich::files(&ws.root, s)).filter(|f| !f.starts_with(&ws.dir));
    let mut files: Vec<PathBuf> = inputs.chain(settings).collect();
    files.sort();
    files.dedup();
    files
        .into_iter()
        .map(|f| {
            let m = std::fs::metadata(&f).ok();
            (f, m.as_ref().and_then(|m| m.modified().ok()), m.map_or(0, |m| m.len()))
        })
        .collect()
}

pub fn run(ws: Workspace, addr: &str, mcp: bool) -> anyhow::Result<()> {
    if mcp {
        return mcp_stdio(&ws);
    }
    let listener = std::net::TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    std::fs::write(ws.dir.join("serve.json"), json!({"addr": local.to_string(), "pid": std::process::id()}).to_string())?;
    eprintln!("ograph serve on {local}");
    let root = ws.root.clone();
    let mut seen = inventory(&ws);
    let current = Arc::new(RwLock::new(Arc::new(ws)));
    let watched = current.clone();
    std::thread::spawn(move || {
        let mut last_error = String::new();
        loop {
            std::thread::sleep(Duration::from_secs(5));
            if inventory(&watched.read().unwrap_or_else(|e| e.into_inner())) == seen {
                continue;
            }
            // Reopened so an edited config.toml or ontology takes effect; the inventory advances
            // only after a harvest succeeds, so a failed one is retried at the next poll.
            let harvested = Workspace::open(&root).and_then(|ws| {
                let now = inventory(&ws);
                let batches = ws.harvest(None, |_, _| {})?.len();
                *watched.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(ws);
                Ok((now, batches))
            });
            match harvested {
                Ok((now, batches)) => {
                    seen = now;
                    last_error.clear();
                    eprintln!("re-harvested {batches} batches");
                }
                Err(e) if format!("{e:#}") != last_error => {
                    last_error = format!("{e:#}");
                    eprintln!("re-harvest failed: {last_error}");
                }
                Err(_) => {}
            }
        }
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let current = current.clone();
        std::thread::spawn(move || {
            let mut out = stream.try_clone().expect("clone socket");
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let reply = match serde_json::from_str::<Value>(&line) {
                    Ok(req) => {
                        let id = req.get("id").cloned().unwrap_or(Value::Null);
                        let ws = current.read().unwrap_or_else(|e| e.into_inner()).clone();
                        match call(&ws, req.get("method").and_then(Value::as_str).unwrap_or(""), req.get("params").unwrap_or(&Value::Null)) {
                            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": format!("{e:#}")}}),
                        }
                    }
                    Err(e) => json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}),
                };
                if writeln!(out, "{reply}").is_err() {
                    break;
                }
            }
        });
    }
    Ok(())
}

/// MCP over stdio: initialize, tools, resources with one template, and one prompt.
fn mcp_stdio(ws: &Workspace) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = req.get("id").cloned() else { continue };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => Ok(json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}, "resources": {}, "prompts": {}}, "serverInfo": {"name": "ograph", "version": env!("CARGO_PKG_VERSION")}})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "resources/list" => Ok(json!({"resources": resources()})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": resource_templates()})),
            "resources/read" => read_resource(ws, req["params"].get("uri").and_then(Value::as_str).unwrap_or("")).map_err(|e| json!({"code": -32002, "message": format!("{e:#}")})),
            "prompts/list" => Ok(json!({"prompts": prompts()})),
            "prompts/get" => prompt(req["params"].get("name").and_then(Value::as_str).unwrap_or(""), req["params"].get("arguments").unwrap_or(&Value::Null)).map_err(|e| json!({"code": -32602, "message": format!("{e:#}")})),
            "tools/call" => {
                let p = &req["params"];
                let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                Ok(match call(ws, name, p.get("arguments").unwrap_or(&Value::Null)) {
                    Ok(v) => json!({"content": [{"type": "text", "text": v.to_string()}]}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
                })
            }
            "ping" => Ok(json!({})),
            _ => Err(json!({"code": -32601, "message": format!("unknown method {method}")})),
        };
        let reply = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verdict_writes() {
        let hints: Vec<(String, bool)> = tools().as_array().unwrap().iter().map(|t| (t["name"].as_str().unwrap().to_owned(), t["annotations"]["readOnlyHint"].as_bool().unwrap())).collect();
        let writers: Vec<&str> = hints.iter().filter(|h| !h.1).map(|h| h.0.as_str()).collect();
        assert_eq!(writers, ["verdict"]);
        assert_eq!(hints.len(), 13);
    }

    #[test]
    fn the_prompt_follows_the_investigation_reference() {
        let text = prompt("investigate", &json!({"incident": "INC-5377"})).unwrap()["messages"][0]["content"]["text"].as_str().unwrap().to_owned();
        assert!(text.starts_with("Investigate incident INC-5377 with the ograph tools."));
        let order: Vec<&str> = text.match_indices("(tool `").map(|(k, _)| text[k + 7..].split('`').next().unwrap()).collect();
        assert_eq!(order, ["before", "chains", "similar", "blast", "code", "suspects", "news"]);
        let report = INVESTIGATION.split("\n## What to report\n").nth(1).unwrap().trim();
        assert!(text.contains(report), "the reporting rules are carried word for word");
        assert!(!text.contains("```") && !text.contains("ogrep --callers"), "no example output and no ogrep hand-over");
        assert!(prompt("investigate", &json!({})).is_err());
        assert!(prompt("triage", &json!({"incident": "INC-1"})).is_err());
    }
}
