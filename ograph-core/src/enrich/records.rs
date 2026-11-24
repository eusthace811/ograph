//! Record sources: incidents, alerts, deployments, config changes, ownership and news. Each
//! record becomes one event node whose evidence is `file#n`, the n-th record of the file
//! counting from 1 (an array element, or a non-blank JSON line).

use super::{Ctx, Event, EvidenceRef, app, dt, event, json_records, ogrep, parse_time, rel, str_field};
use crate::model::{Batch, EntityRef, Extraction, Object, entity, known, lit, obj, slug};
use crate::vocab::{RDF_TYPE, evt, inc, ops};
use oxrdf::NamedNode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn class(c: &str) -> Object {
    obj(NamedNode::new_unchecked(c))
}

fn rule(name: &str) -> Extraction {
    Extraction::Rule { extractor: name.into() }
}

/// Every incident file of every incident source, as one batch keyed `all`. An incident may
/// appear in several records and files, each stating some of its fields: a field comes from the
/// last record that states it, in file path order and then record order, so a later export of
/// an incident updates it rather than conflicting with it. The fields are merged before the
/// gateway checks the incident, so one harvest sees all of them. A file that cannot be read
/// fails the whole batch, which keeps its last good facts.
pub fn incidents(files: &[(PathBuf, String)], mut degraded: Vec<String>) -> Batch {
    let mut b = Batch::new("incidents", "all", "incidents-v1");
    let mut parsed = Vec::with_capacity(files.len());
    for (path, key) in files {
        match json_records(path) {
            Ok(records) => parsed.push((key, records)),
            Err(e) => degraded.push(format!("{key}: {e:#}")),
        }
    }
    if !degraded.is_empty() {
        b.degraded = degraded;
        return b;
    }
    let records: Vec<(String, IncidentRecord)> = parsed.iter().flat_map(|(key, rs)| rs.iter().enumerate().map(move |(i, r)| (format!("{key}#{}", i + 1), incident_fields(r)))).collect();
    // Keyed by the node's name, so ids that differ only in case or punctuation are one incident.
    let keys: Vec<String> = records.iter().map(|(_, (id, _))| crate::model::slug(id)).collect();
    let mut last: HashMap<(&str, &str), usize> = HashMap::new();
    for (n, (_, (_, values))) in records.iter().enumerate() {
        last.insert((keys[n].as_str(), "id"), n);
        for (p, _) in values {
            last.insert((keys[n].as_str(), *p), n);
        }
    }
    let m = rule("incident-export");
    for (n, (record, (id, values))) in records.iter().enumerate() {
        if id.is_empty() {
            continue;
        }
        let ev = b.record(record.as_str());
        let s = known("inc", id);
        b.add(s.clone(), RDF_TYPE, class(inc::INCIDENT), &ev, &m);
        if last[&(keys[n].as_str(), "id")] == n {
            b.add(s.clone(), inc::ID, lit(id.as_str()), &ev, &m);
        }
        for (p, o) in values {
            if last[&(keys[n].as_str(), *p)] == n {
                b.add(s.clone(), p, o.clone(), &ev, &m);
            }
        }
        b.add(s, evt::SIGNAL, lit("INCIDENT"), &ev, &m);
    }
    b
}

/// The id of an incident record (empty when it has none) and the fields it states.
type IncidentRecord = (String, Vec<(&'static str, Object)>);

fn incident_fields(r: &serde_json::Value) -> IncidentRecord {
    let id = ["id", "number", "key"].iter().find_map(|f| match r.get(*f)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        _ => None,
    });
    let time = |names: &[&str]| names.iter().find_map(|f| r.get(*f).and_then(parse_time)).map(dt);
    let text = |names: &[&str]| str_field(r, names).map(lit);
    let values = [
        (inc::TITLE, text(&["title", "summary", "name"])),
        (evt::ON, str_field(r, &["service", "app", "application"]).map(app)),
        (evt::AT, time(&["opened", "opened_at", "created_at", "at"])),
        (inc::RESOLVED_AT, time(&["resolved", "resolved_at"])),
        (evt::SEVERITY, text(&["severity"])),
        (inc::STATUS, text(&["status"])),
        (inc::MITIGATION, text(&["mitigation"])),
        (inc::FAULT_TYPE, text(&["fault", "fault_type"])),
    ];
    (id.unwrap_or_default(), values.into_iter().filter_map(|(p, o)| Some((p, o?))).collect())
}

/// A monitoring tool's severity label as an `evt:severity` value.
pub(super) fn severity(label: &str) -> &'static str {
    match label.to_lowercase().as_str() {
        "critical" | "page" | "sev1" => "SEV1",
        "error" | "major" | "sev2" => "SEV2",
        "warning" | "minor" | "sev3" => "SEV3",
        _ => "SEV4",
    }
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
        let severity = severity(str_field(labels, &["severity"]).unwrap_or(""));
        event(&mut b, &s, Event { class: evt::ALERT, on: svc, at: t, signal: "ALERT", severity, value: None }, [(evt::SUMMARY, lit(name))], &ev, &m);
    }
    Ok(b)
}

/// A deployment whose changed files a `code` source can name.
pub struct Deploy {
    event: EntityRef,
    evidence: EvidenceRef,
    service: String,
    version: String,
    at: i64,
}

pub fn deployments(path: &Path, key: &str) -> anyhow::Result<(Batch, Vec<Deploy>)> {
    let mut b = Batch::new("deployments", key, "deployments-v1");
    let m = rule("deploy-export");
    let mut deploys = Vec::new();
    for (i, r) in json_records(path)?.iter().enumerate() {
        let ev = b.record(format!("{key}#{}", i + 1));
        let (Some(svc), Some(t)) = (str_field(r, &["service", "app"]), ["at", "time", "finished_at"].iter().find_map(|f| r.get(*f).and_then(parse_time))) else { continue };
        let version = str_field(r, &["version", "tag", "commit"]);
        let s = known("ev", &format!("deploy/{}/{}/{t}", slug(svc), slug(version.unwrap_or("unknown"))));
        event(&mut b, &s, Event { class: evt::DEPLOYMENT, on: svc, at: t, signal: "DEPLOYMENT", severity: "SEV4", value: None }, [(evt::VERSION, lit(version.unwrap_or("unknown")))], &ev, &m);
        if let Some(version) = version {
            deploys.push(Deploy { event: s, evidence: ev, service: svc.to_owned(), version: version.to_owned(), at: t });
        }
    }
    Ok((b, deploys))
}

/// A release can hold hundreds of commits; this many files say which parts of the code it
/// touched.
const MAX_FILES: usize = 200;

/// Adds `evt:changedFile` to each deployment of one file whose version, and the version the
/// same service ran before it, are revisions of a code repository (a tag or a commit id): the
/// files of the commits to the service between the two, from `ogrep --timeline`. Deployments
/// whose versions are not revisions keep no files, and one note says how many.
pub fn deployment_files(b: &mut Batch, ctx: &Ctx<'_>, mut deploys: Vec<Deploy>) {
    deploys.sort_by(|x, y| (&x.service, x.at).cmp(&(&y.service, y.at)));
    let pairs: Vec<(&Deploy, &Deploy)> = deploys.windows(2).map(|w| (&w[0], &w[1])).filter(|(before, d)| before.service == d.service && before.version != d.version).collect();
    // Per pair: the commits between the two versions in the first repository that knows both.
    let shipped = ogrep::each(&pairs, |(before, d)| {
        let mut failure = None;
        for repo in ctx.code {
            match ogrep::timeline(repo, &d.service, &before.version, Some(&d.version)) {
                Ok(commits) => return Ok((repo, commits)),
                Err(e) => failure = failure.or(Some(e.to_string())),
            }
        }
        Err(failure)
    });
    let m = rule("ogrep-timeline");
    let mut failures = Vec::new();
    for ((_, d), shipped) in pairs.iter().zip(shipped) {
        match shipped {
            Ok((repo, commits)) => {
                let mut files: Vec<String> = commits.iter().flat_map(|c| &c.files).map(|f| rel(ctx.root, &repo.join(f))).collect();
                files.sort();
                files.dedup();
                for file in files.into_iter().take(MAX_FILES) {
                    b.add(d.event.clone(), evt::CHANGED_FILE, lit(file), &d.evidence, &m);
                }
            }
            Err(e) => failures.extend(e),
        }
    }
    if let Some(first) = failures.first() {
        // What ogrep 0.2.0 answers when `--timeline` is given `--until`.
        let first = if first.contains("--until and --author") { "this ogrep cannot compare two revisions; it needs a release after 0.2.0" } else { first.as_str() };
        b.degraded.push(format!("{}: {} of {} deployments could not be compared with the version before, so they list no changed files: {first}", b.key, failures.len(), pairs.len()));
    }
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

/// News items: `{"at", "title", "summary", "mentions": [names]}`. Each mention becomes a vendor
/// node `id:vendor/<name>`; `ograph news` reports the items whose vendors an application uses.
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
