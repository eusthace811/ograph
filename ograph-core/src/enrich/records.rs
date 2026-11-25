//! Record sources: incidents, alerts, deployments, config changes, ownership and news. Each
//! record becomes one event node whose evidence is `file#n`, the n-th record of the file
//! counting from 1 (an array element, or a non-blank JSON line).

use super::{Ctx, Event, EvidenceRef, app, dt, event, json_records, ogrep, parse_time, rel, str_field};
use crate::model::{Batch, Confidence, EntityRef, Extraction, Object, entity, known, lit, obj, slug};
use crate::vocab::{RDF_TYPE, evt, inc, ops};
use oxrdf::NamedNode;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

fn class(c: &str) -> Object {
    obj(NamedNode::new_unchecked(c))
}

fn rule(name: &str) -> Extraction {
    Extraction::Rule { extractor: name.into() }
}

/// How one incident field is chosen when several records state it.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    /// The last record that states it, in file path order and then record order.
    #[default]
    Last,
    First,
    /// The earliest or latest time (`at`, `resolved`).
    Earliest,
    Latest,
    /// The most or least severe (`severity`).
    Max,
    Min,
}

impl Pick {
    fn name(self) -> &'static str {
        match self {
            Pick::Last => "last",
            Pick::First => "first",
            Pick::Earliest => "earliest",
            Pick::Latest => "latest",
            Pick::Max => "max",
            Pick::Min => "min",
        }
    }
}

/// `[incidents] merge`: a [`Pick`] per functional field, and `default` for the rest.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Merge {
    pub default: Pick,
    pub at: Option<Pick>,
    pub resolved: Option<Pick>,
    pub severity: Option<Pick>,
    pub service: Option<Pick>,
}

/// The fields of an incident that hold one value, by config name. Records that disagree on one
/// of them are a conflict, and the values not kept are quarantined. The status is not one: it
/// moves from open to resolved, so a later export that states another status updates it.
const FUNCTIONAL: [(&str, &str); 4] = [("at", evt::AT), ("resolved", inc::RESOLVED_AT), ("severity", evt::SEVERITY), ("service", evt::ON)];

impl Merge {
    fn pick(&self, field: &str) -> Pick {
        let set = match field {
            "at" => self.at,
            "resolved" => self.resolved,
            "severity" => self.severity,
            "service" => self.service,
            _ => None,
        };
        set.unwrap_or(self.default)
    }

    /// A time pick on a field that is not a time, or a severity pick on one that is not the
    /// severity, is refused rather than read as `last`.
    pub fn check(&self) -> anyhow::Result<()> {
        for (field, _) in FUNCTIONAL {
            let p = self.pick(field);
            let fits = match p {
                Pick::Last | Pick::First => true,
                Pick::Earliest | Pick::Latest => matches!(field, "at" | "resolved"),
                Pick::Max | Pick::Min => field == "severity",
            };
            anyhow::ensure!(fits, "[incidents] merge: {field} = \"{}\" does not apply to {field}", p.name());
        }
        Ok(())
    }
}

/// Every incident file of every incident source, as one batch keyed `all`. An incident may
/// appear in several records and files, each stating some of its fields. By default a field
/// comes from the last record that states it, in file path order and then record order, so a
/// later export of an incident updates it; `[incidents] merge` can choose another record per
/// field. When records disagree on a functional field, each value not kept goes to the
/// quarantine with the record that stated it, so `ograph why` shows the disagreement. The
/// fields are merged before the gateway checks the incident, so one harvest sees all of them. A
/// file that cannot be read fails the whole batch, which keeps its last good facts. Titles from
/// a file marked to be classified get a fault class from the model.
pub fn incidents(files: &[(PathBuf, String, bool)], mut degraded: Vec<String>, ctx: &Ctx<'_>) -> Batch {
    let mut b = Batch::new("incidents", "all", "incidents-v1");
    let mut parsed = Vec::with_capacity(files.len());
    for (path, key, classify) in files {
        match json_records(path) {
            Ok(records) => parsed.push((key, *classify, records)),
            Err(e) => degraded.push(format!("{key}: {e:#}")),
        }
    }
    if !degraded.is_empty() {
        b.degraded = degraded;
        return b;
    }
    let records: Vec<(String, bool, IncidentRecord)> = parsed.iter().flat_map(|(key, classify, rs)| rs.iter().enumerate().map(move |(i, r)| (format!("{key}#{}", i + 1), *classify, incident_fields(r)))).collect();
    // Keyed by the node's name, so ids that differ only in case or punctuation are one incident.
    let keys: Vec<String> = records.iter().map(|(_, _, (id, _))| crate::model::slug(id)).collect();
    // Per incident and field, the records that state it, in order.
    let mut stated: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    for (n, (_, _, (_, values))) in records.iter().enumerate() {
        stated.entry((keys[n].as_str(), "id")).or_default().push(n);
        for v in values {
            stated.entry((keys[n].as_str(), v.predicate)).or_default().push(n);
        }
    }
    let value = |n: usize, p: &str| records[n].2.1.iter().find(|v| v.predicate == p);
    let m = rule("incident-export");
    let mut titles = Vec::new();
    let mut kept: HashMap<(&str, &str), usize> = HashMap::new();
    // Values outside a closed vocabulary take no part in the merge; each goes to the gateway,
    // which quarantines it for that reason.
    let mut invalid: Vec<(usize, &str)> = Vec::new();
    let valid = |n: usize, p: &str| match (ctx.ontology.vocabulary(p), &value(n, p).unwrap().object) {
        (Some(v), Object::Literal(l)) => v.iter().any(|x| x.value == l.value()),
        _ => true,
    };
    for (&(key, p), all) in &stated {
        let Some((field, _)) = FUNCTIONAL.iter().find(|(_, q)| *q == p) else {
            kept.insert((key, p), *all.last().unwrap());
            continue;
        };
        let (ns, bad): (Vec<usize>, Vec<usize>) = all.iter().partition(|&&n| valid(n, p));
        invalid.extend(bad.into_iter().map(|n| (n, p)));
        let Some(&last) = ns.last() else {
            kept.insert((key, p), *all.last().unwrap());
            continue;
        };
        let pick = ctx.merge.pick(field);
        let rank = |n: &usize| value(*n, p).and_then(|v| v.rank);
        let chosen = match pick {
            Pick::Last => ns.last(),
            Pick::First => ns.first(),
            // Among equal values the last record wins, as with `last`.
            Pick::Earliest | Pick::Max => ns.iter().rev().filter(|n| rank(n).is_some()).min_by_key(|n| rank(n)),
            Pick::Latest | Pick::Min => ns.iter().filter(|n| rank(n).is_some()).max_by_key(|n| rank(n)),
        }
        .copied()
        .unwrap_or(last);
        kept.insert((key, p), chosen);
        let winner = &value(chosen, p).unwrap().object;
        for &n in &ns {
            let loser = value(n, p).unwrap();
            if !same(&loser.object, winner) && !records[n].2.0.is_empty() {
                let ev = b.record(records[n].0.as_str());
                b.withhold(known("inc", &records[n].2.0), p, loser.object.clone(), &ev, &m, format!("{} disagrees with {} (kept by policy {})", records[n].0, records[chosen].0, pick.name()));
            }
        }
    }
    for (n, (record, classify, (id, values))) in records.iter().enumerate() {
        if id.is_empty() {
            continue;
        }
        let ev = b.record(record.as_str());
        let s = known("inc", id);
        b.add(s.clone(), RDF_TYPE, class(inc::INCIDENT), &ev, &m);
        if kept[&(keys[n].as_str(), "id")] == n {
            b.add(s.clone(), inc::ID, lit(id.as_str()), &ev, &m);
        }
        for v in values {
            if kept[&(keys[n].as_str(), v.predicate)] == n || invalid.contains(&(n, v.predicate)) {
                if let (true, inc::TITLE, Object::Literal(l)) = (classify, v.predicate, &v.object) {
                    titles.push((s.clone(), ev.clone(), l.value().to_owned()));
                }
                b.add(s.clone(), v.predicate, v.object.clone(), &ev, &m);
            }
        }
        b.add(s.clone(), evt::SOURCE_RECORD, lit(record.as_str()), &ev, &m);
        b.add(s, evt::SIGNAL, lit("INCIDENT"), &ev, &m);
    }
    fault_classes(&mut b, ctx, titles);
    b
}

/// Whether two stated values agree: application names as the catalog matches them, the rest
/// exactly.
fn same(a: &Object, b: &Object) -> bool {
    match (a, b) {
        (Object::Entity(EntityRef::Provisional { class: c, name: x }), Object::Entity(EntityRef::Provisional { class: d, name: y })) => c == d && crate::gateway::norm(x) == crate::gateway::norm(y),
        _ => a == b,
    }
}

fn fault_prompt(ont: &crate::ontology::Ontology) -> String {
    let mut p = String::from(
        "You classify the fault an incident title states. Reply with one JSON object and nothing else:\n\
         {\"fault_class\": <one value>, \"quote\": <the words of the title that state the fault, copied exactly, or null>, \"confidence\": <0 to 1>}\n\nAllowed fault_class values:\n",
    );
    for t in ont.vocabulary(inc::FAULT_CLASS).unwrap_or_default() {
        match &t.definition {
            Some(d) => p.push_str(&format!("- {}: {d}\n", t.value)),
            None => p.push_str(&format!("- {}\n", t.value)),
        }
    }
    p.push_str("\nUse only these values. When the title names only a symptom (errors, slow, down) and not the kind of fault, answer UNKNOWN with quote null; do not guess. The title:\n");
    p
}

/// `inc:faultClass` for each title, from the model. A class the model cannot quote from the
/// title is not stated there, so it becomes UNKNOWN; a value outside the vocabulary still goes
/// to the gateway, which quarantines it.
fn fault_classes(b: &mut Batch, ctx: &Ctx<'_>, titles: Vec<(EntityRef, EvidenceRef, String)>) {
    if titles.is_empty() {
        return;
    }
    let Some(llm) = ctx.llm else {
        b.degraded.push(format!("classify = true needs an [llm.extract] model; {} incident titles have no fault class", titles.len()));
        return;
    };
    let p = fault_prompt(ctx.ontology);
    let prompt_hash = crate::llm::hash(&p);
    let texts: Vec<String> = titles.iter().map(|t| t.2.clone()).collect();
    let valid = ctx.ontology.vocabulary(inc::FAULT_CLASS).unwrap_or_default();
    for ((node, ev, title), answer) in titles.into_iter().zip(llm.ask_all(&p, &texts)) {
        let v = match answer.map(|a| crate::llm::first_json(&a).ok_or(a)) {
            Ok(Ok(v)) => v,
            Ok(Err(a)) => {
                b.degraded.push(format!("model answer was not JSON: {}", a.chars().take(80).collect::<String>()));
                continue;
            }
            Err(e) => {
                b.degraded.push(format!("model call failed for incident title {title:?}: {e:#}"));
                continue;
            }
        };
        let Some(c) = str_field(&v, &["fault_class"]) else { continue };
        let confidence = v.get("confidence").and_then(|c| c.as_f64()).and_then(Confidence::new).unwrap_or(Confidence::new(0.0).unwrap());
        let m = Extraction::Llm { model: llm.config.model.clone(), prompt_hash: prompt_hash.clone(), confidence };
        let stated = str_field(&v, &["quote"]).is_some_and(|q| crate::llm::quoted(&title, q));
        let known = valid.iter().any(|t| t.value == c);
        b.add(node, inc::FAULT_CLASS, lit(if stated || !known { c } else { "UNKNOWN" }), &ev, &m);
    }
}

/// One field an incident record states. `rank` orders the values a time or severity pick
/// compares: milliseconds for a time, and 1 (SEV1) to 4 for a severity.
struct Stated {
    predicate: &'static str,
    object: Object,
    rank: Option<i64>,
}

/// The id of an incident record (empty when it has none) and the fields it states.
type IncidentRecord = (String, Vec<Stated>);

fn incident_fields(r: &serde_json::Value) -> IncidentRecord {
    let id = ["id", "number", "key"].iter().find_map(|f| match r.get(*f)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        _ => None,
    });
    let time = |names: &[&str]| names.iter().find_map(|f| r.get(*f).and_then(parse_time)).map(|t| (dt(t), Some(t)));
    let text = |names: &[&str]| str_field(r, names).map(|v| (lit(v), None));
    let values = [
        (inc::TITLE, text(&["title", "summary", "name"])),
        (evt::ON, str_field(r, &["service", "app", "application"]).map(|v| (app(v), None))),
        (evt::AT, time(&["opened", "opened_at", "created_at", "at"])),
        (inc::RESOLVED_AT, time(&["resolved", "resolved_at"])),
        (evt::SEVERITY, str_field(r, &["severity"]).map(|v| (lit(v), severity(v)[3..].parse().ok()))),
        (inc::STATUS, text(&["status"])),
        (inc::MITIGATION, text(&["mitigation"])),
        (inc::FAULT_TYPE, text(&["fault", "fault_type"])),
    ];
    (id.unwrap_or_default(), values.into_iter().filter_map(|(predicate, v)| v.map(|(object, rank)| Stated { predicate, object, rank })).collect())
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
        let first = if first.contains("--until and --author") { "this ogrep cannot compare two revisions; it needs ogrep 0.3 or newer" } else { first.as_str() };
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
