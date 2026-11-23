//! Record sources: incidents, alerts, deployments, config changes, ownership and news. Each
//! record becomes one event node whose evidence is `file#n`, the n-th record of the file
//! counting from 0 (an array element, or a non-blank JSON line).

use super::{Event, app, dt, event, json_records, parse_time, str_field};
use crate::model::{Batch, EntityRef, Extraction, Object, entity, known, lit, obj, slug};
use crate::vocab::{RDF_TYPE, evt, inc, ops};
use oxrdf::NamedNode;
use std::path::Path;

fn class(c: &str) -> Object {
    obj(NamedNode::new_unchecked(c))
}

fn rule(name: &str) -> Extraction {
    Extraction::Rule { extractor: name.into() }
}

pub fn incidents(path: &Path, key: &str) -> anyhow::Result<Batch> {
    let mut b = Batch::new("incidents", key, "incidents-v1");
    let m = rule("incident-export");
    for (i, r) in json_records(path)?.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let id = str_field(r, &["id", "number", "key"]).ok_or_else(|| anyhow::anyhow!("record {} has no id", i + 1))?;
        let s = known("inc", id);
        b.add(s.clone(), RDF_TYPE, class(inc::INCIDENT), &ev, &m);
        b.add(s.clone(), inc::ID, lit(id), &ev, &m);
        if let Some(t) = str_field(r, &["title", "summary", "name"]) {
            b.add(s.clone(), inc::TITLE, lit(t), &ev, &m);
        }
        if let Some(svc) = str_field(r, &["service", "app", "application"]) {
            b.add(s.clone(), evt::ON, app(svc), &ev, &m);
        }
        if let Some(t) = ["opened", "opened_at", "created_at", "at"].iter().find_map(|f| r.get(*f).and_then(parse_time)) {
            b.add(s.clone(), evt::AT, dt(t), &ev, &m);
        }
        if let Some(t) = ["resolved", "resolved_at"].iter().find_map(|f| r.get(*f).and_then(parse_time)) {
            b.add(s.clone(), inc::RESOLVED_AT, dt(t), &ev, &m);
        }
        if let Some(v) = str_field(r, &["severity"]) {
            b.add(s.clone(), evt::SEVERITY, lit(v), &ev, &m);
        }
        if let Some(v) = str_field(r, &["status"]) {
            b.add(s.clone(), inc::STATUS, lit(v), &ev, &m);
        }
        if let Some(v) = str_field(r, &["mitigation"]) {
            b.add(s.clone(), inc::MITIGATION, lit(v), &ev, &m);
        }
        if let Some(v) = str_field(r, &["fault", "fault_type"]) {
            b.add(s.clone(), inc::FAULT_TYPE, lit(v), &ev, &m);
        }
        b.add(s, evt::SIGNAL, lit("INCIDENT"), &ev, &m);
    }
    Ok(b)
}

/// Alertmanager webhook payloads (`{"alerts": [...]}`) or one alert per line.
pub fn alerts(path: &Path, key: &str) -> anyhow::Result<Batch> {
    let mut b = Batch::new("alerts", key, "alertmanager-v1");
    let m = rule("alertmanager");
    let mut flat = Vec::new();
    for r in json_records(path)? {
        match r.get("alerts").and_then(|a| a.as_array()) {
            Some(list) => flat.extend(list.iter().cloned()),
            None => flat.push(r),
        }
    }
    for (i, a) in flat.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let labels = a.get("labels").unwrap_or(a);
        let Some(svc) = str_field(labels, &["service", "app", "job"]) else { continue };
        let Some(t) = ["startsAt", "at", "time"].iter().find_map(|f| a.get(*f).and_then(parse_time)) else { continue };
        let name = str_field(labels, &["alertname", "name"]).unwrap_or("alert");
        let s = known("ev", &format!("alert/{}/{}/{t}", slug(svc), slug(name)));
        let severity = match str_field(labels, &["severity"]).unwrap_or("").to_lowercase().as_str() {
            "critical" | "page" | "sev1" => "SEV1",
            "error" | "major" | "sev2" => "SEV2",
            "warning" | "minor" | "sev3" => "SEV3",
            _ => "SEV4",
        };
        event(&mut b, &s, Event { class: evt::ALERT, on: svc, at: t, signal: "ALERT", severity, value: None }, [(evt::SUMMARY, lit(name))], &ev, &m);
    }
    Ok(b)
}

pub fn deployments(path: &Path, key: &str) -> anyhow::Result<Batch> {
    let mut b = Batch::new("deployments", key, "deployments-v1");
    let m = rule("deploy-export");
    for (i, r) in json_records(path)?.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let (Some(svc), Some(t)) = (str_field(r, &["service", "app"]), ["at", "time", "finished_at"].iter().find_map(|f| r.get(*f).and_then(parse_time))) else { continue };
        let version = str_field(r, &["version", "tag", "commit"]).unwrap_or("unknown");
        let s = known("ev", &format!("deploy/{}/{}/{t}", slug(svc), slug(version)));
        event(&mut b, &s, Event { class: evt::DEPLOYMENT, on: svc, at: t, signal: "DEPLOYMENT", severity: "SEV4", value: None }, [(evt::VERSION, lit(version))], &ev, &m);
    }
    Ok(b)
}

pub fn config_changes(path: &Path, key: &str) -> anyhow::Result<Batch> {
    let mut b = Batch::new("config", key, "config-v1");
    let m = rule("config-export");
    for (i, r) in json_records(path)?.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let (Some(svc), Some(t)) = (str_field(r, &["service", "app"]), ["at", "time"].iter().find_map(|f| r.get(*f).and_then(parse_time))) else { continue };
        let what = str_field(r, &["key", "setting", "summary"]).unwrap_or("config");
        let s = known("ev", &format!("config/{}/{}/{t}", slug(svc), slug(what)));
        event(&mut b, &s, Event { class: evt::CONFIG_CHANGE, on: svc, at: t, signal: "CONFIG_CHANGE", severity: "SEV4", value: None }, [(evt::SUMMARY, lit(what))], &ev, &m);
    }
    Ok(b)
}

/// `{"app", "process"}` (business processes), `{"app", "vendor"}` (external vendors) and
/// `{"app", "alias"}` (another name telemetry uses for the app).
pub fn ownership(path: &Path, key: &str) -> anyhow::Result<Batch> {
    let mut b = Batch::new("ownership", key, "ownership-v1");
    let m = rule("ownership");
    for (i, r) in json_records(path)?.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let Some(a) = str_field(r, &["app", "service"]) else { continue };
        let subject = EntityRef::Provisional { class: ops::APPLICATION, name: a.to_owned() };
        if let Some(p) = str_field(r, &["process"]) {
            let pn = entity("process", p);
            b.add(EntityRef::Known(pn.clone()), RDF_TYPE, class(ops::BUSINESS_PROCESS), &ev, &m);
            b.add(EntityRef::Known(pn.clone()), ops::NAME, lit(p), &ev, &m);
            b.add(subject.clone(), ops::SUPPORTS, obj(pn), &ev, &m);
        }
        if let Some(alias) = str_field(r, &["alias"]) {
            b.add(subject.clone(), ops::ALIAS, lit(alias), &ev, &m);
        }
        if let Some(v) = str_field(r, &["vendor"]) {
            let vn = entity("vendor", v);
            b.add(EntityRef::Known(vn.clone()), RDF_TYPE, class(ops::VENDOR), &ev, &m);
            b.add(EntityRef::Known(vn.clone()), ops::NAME, lit(v), &ev, &m);
            b.add(subject, ops::USES_VENDOR, obj(vn), &ev, &m);
        }
    }
    Ok(b)
}

/// News items: `{"at", "title", "summary", "mentions": [names]}`. A mention resolves to a
/// catalog application or a known vendor; anything else stays out of the graph.
pub fn news(path: &Path, key: &str) -> anyhow::Result<Batch> {
    let mut b = Batch::new("news", key, "news-v1");
    let m = rule("news-feed");
    for (i, r) in json_records(path)?.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let Some(t) = ["at", "published", "time"].iter().find_map(|f| r.get(*f).and_then(parse_time)) else { continue };
        let title = str_field(r, &["title"]).unwrap_or("news");
        let s = known("ev", &format!("news/{}/{t}", slug(title)));
        b.add(s.clone(), RDF_TYPE, class(evt::NEWS_ITEM), &ev, &m);
        b.add(s.clone(), evt::AT, dt(t), &ev, &m);
        b.add(s.clone(), evt::SUMMARY, lit(str_field(r, &["summary"]).unwrap_or(title)), &ev, &m);
        b.add(s.clone(), evt::SIGNAL, lit("NEWS"), &ev, &m);
        for name in r.get("mentions").and_then(|x| x.as_array()).into_iter().flatten().filter_map(|x| x.as_str()) {
            b.add(s.clone(), evt::MENTIONS, obj(entity("vendor", name)), &ev, &m);
        }
    }
    Ok(b)
}
