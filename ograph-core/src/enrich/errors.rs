//! Error reports from an error tracker. Each record is one exception event; ogrep finds the
//! frames of its stack trace in the configured code repositories, so the event points at lines.

use super::{Ctx, Event, code_location, event, json_records, ogrep, parse_time, str_field};
use crate::model::{Batch, Extraction, known, lit, slug};
use crate::vocab::evt;
use oxrdf::Literal;
use std::collections::BTreeMap;
use std::path::Path;

/// One ogrep call per distinct trace, so a noisy export cannot hold a harvest for minutes.
const MAX_TRACES: usize = 200;
/// Only frames found in a repository count, so a few cover most traces.
const MAX_FRAMES: usize = 8;

/// `{"service"|"app", "at"|"time"|"timestamp"|"ts", "message"|"error"|"title",
/// "stack"|"stacktrace"|"trace", "level"|"severity"?, "count"?}`.
pub fn harvest(path: &Path, key: &str, ctx: &Ctx<'_>) -> anyhow::Result<Batch> {
    let mut b = Batch::new("errors", key, "errors-v1");
    let m = Extraction::Rule { extractor: "error-export".into() };
    // Per distinct trace text, in file order: the events that carry it.
    let mut traces: Vec<(&str, Vec<_>)> = Vec::new();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let records = json_records(path)?;
    for (i, r) in records.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let (Some(svc), Some(t)) = (str_field(r, &["service", "app"]), ["at", "time", "timestamp", "ts"].iter().find_map(|f| r.get(*f).and_then(parse_time))) else { continue };
        let message = str_field(r, &["message", "error", "title"]).unwrap_or("exception");
        let severity = match str_field(r, &["level", "severity"]) {
            None => "SEV3",
            Some(l) if l.eq_ignore_ascii_case("fatal") => "SEV1",
            Some(l) => super::records::severity(l),
        };
        let s = known("ev", &format!("exception/{}/{}/{t}", slug(svc), &hex::encode(crate::gateway::sha2_hash(message))[..12]));
        let details = [(evt::SUMMARY, lit(message.chars().take(300).collect::<String>()))];
        event(&mut b, &s, Event { class: evt::EXCEPTION, on: svc, at: t, signal: "EXCEPTION", severity, value: None }, details, &ev, &m);
        if let Some(n) = r.get("count").and_then(|c| c.as_i64()) {
            b.add(s.clone(), evt::COUNT, lit(Literal::from(n)), &ev, &m);
        }
        if let Some(stack) = str_field(r, &["stack", "stacktrace", "trace"]).filter(|x| !x.trim().is_empty()) {
            let n = *seen.entry(stack).or_insert_with(|| {
                traces.push((stack, Vec::new()));
                traces.len() - 1
            });
            traces[n].1.push((s, ev));
        }
    }
    if ctx.code.is_empty() {
        return Ok(b);
    }
    if traces.len() > MAX_TRACES {
        b.degraded.push(format!("{key}: {} distinct stack traces, only the first {MAX_TRACES} were looked up in the code", traces.len()));
    }
    let located = Extraction::Rule { extractor: "ogrep-stack-trace".into() };
    'traces: for (stack, events) in traces.iter().take(MAX_TRACES) {
        for repo in ctx.code {
            let frames = match ogrep::stack_trace(repo, stack) {
                Ok(f) => f,
                Err(e @ ogrep::Failure::NotRunnable(_)) => {
                    b.degraded.push(e.to_string());
                    break 'traces;
                }
                Err(e) => {
                    let note = e.to_string();
                    if !b.degraded.contains(&note) {
                        b.degraded.push(note);
                    }
                    continue;
                }
            };
            if frames.is_empty() {
                continue;
            }
            for (s, ev) in events {
                for frame in frames.iter().take(MAX_FRAMES) {
                    b.add(s.clone(), evt::CODE_LOCATION, lit(code_location(ctx.root, repo, frame)), ev, &located);
                }
            }
            break;
        }
    }
    Ok(b)
}
