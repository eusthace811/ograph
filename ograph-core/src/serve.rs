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
    json!([
        {"name": "before", "description": "Events in the window before an incident on its application and the applications it depends on, newest first, with their signal type and strength. Start every investigation here.",
         "inputSchema": s(json!({"incident": {"type": "string", "description": "incident id like INC-5377 or an IRI"}, "window": {"type": "string", "description": "like 10m (the default) or 2h"}}), &["incident"])},
        {"name": "chains", "description": "Correlation chains and ranked root-cause applications for an incident. Each chain is a hypothesis with its evidence, not a conclusion.",
         "inputSchema": s(json!({"incident": {"type": "string"}, "rank": {"type": "string", "enum": ["chain", "propagation"]}}), &["incident"])},
        {"name": "blast", "description": "Applications that depend on an application, by distance, and the business processes they support.",
         "inputSchema": s(json!({"app": {"type": "string"}}), &["app"])},
        {"name": "similar", "description": "Past incidents that resemble this one, with their accepted causes and mitigations.",
         "inputSchema": s(json!({"incident": {"type": "string"}}), &["incident"])},
        {"name": "why", "description": "Provenance of every fact about a node: harvest graph, source, extractor, model and confidence, and quarantined proposals.",
         "inputSchema": s(json!({"node": {"type": "string"}}), &["node"])},
        {"name": "news", "description": "News that mentions vendors our applications use, with the affected applications and processes.",
         "inputSchema": s(json!({}), &[])},
        {"name": "sparql", "description": "Run a read-only SPARQL query. Prefixes ops: evt: inc: disc: corr: rca: harv: id: are predeclared; the default graph is the union of all harvests. A class or property the ontology does not declare is rejected with the nearest declared name.",
         "inputSchema": s(json!({"query": {"type": "string"}, "limit": {"type": "integer"}}), &["query"])},
        {"name": "verdict", "description": "Record a human-confirmed verdict on a chain. Only call this when a person has confirmed the cause.",
         "inputSchema": s(json!({"chain": {"type": "string"}, "decision": {"type": "string", "enum": ["accept", "reject"]}, "evidence": {"type": "array", "items": {"type": "string"}}, "by": {"type": "string"}}), &["chain", "decision", "evidence", "by"])}
    ])
}

fn arg<'a>(p: &'a Value, k: &str) -> anyhow::Result<&'a str> {
    p.get(k).and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("missing parameter {k}"))
}

pub fn call(ws: &Workspace, method: &str, p: &Value) -> anyhow::Result<Value> {
    let snap = ws.store.snapshot()?;
    Ok(match method {
        "before" => serde_json::to_value(query::before(&snap, arg(p, "incident")?, p.get("window").and_then(Value::as_str).map_or(Ok(600), query::duration_s)?, 4)?)?,
        "chains" => {
            let ids = correlate::Ids::new(&snap)?;
            let mut st = ws.config.correlation.clone();
            if p.get("rank").and_then(Value::as_str) == Some("propagation") {
                st.rank = RankMode::Propagation;
            }
            let mut a = correlate::analyze(&snap, &ids, query::lookup(&snap, arg(p, "incident")?)?, &st)?;
            a.apps.truncate(5);
            serde_json::to_value(a)?
        }
        "blast" => serde_json::to_value(query::blast(&snap, arg(p, "app")?, 8)?)?,
        "similar" => serde_json::to_value(query::similar(&snap, arg(p, "incident")?, &ws.config.correlation, 5)?)?,
        "why" => serde_json::to_value(query::why(&snap, arg(p, "node")?)?)?,
        "news" => serde_json::to_value(query::news(&snap, None)?)?,
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

/// MCP over stdio: initialize, tools/list, tools/call.
fn mcp_stdio(ws: &Workspace) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = req.get("id").cloned() else { continue };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => Ok(json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "ograph", "version": env!("CARGO_PKG_VERSION")}})),
            "tools/list" => Ok(json!({"tools": tools()})),
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
