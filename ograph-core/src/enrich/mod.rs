//! Enrichers turn source files into [`Batch`]es of proposed facts. They never write: the
//! gateway does. They run in Netflix's layer order, and each source file is one batch whose key
//! is the file's path, so re-harvesting a file replaces its graph. Incidents are the exception:
//! all of them are one batch, since one incident's fields can come from several files.

pub mod catalog;
pub mod code;
pub mod discussions;
pub mod docs;
pub mod drain;
pub mod errors;
pub mod ogrep;
pub mod records;
pub mod signals;

use crate::model::{Batch, EntityRef, EvidenceRef, Extraction, Object, lit, obj};
use crate::vocab::{RDF_TYPE, evt, ops};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// One configured input. The order of this enum is the harvest order.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// Runs `ogrep --entities . --json` in a repository: Backstage, Compose, Kubernetes.
    /// `inferred` also takes the calls ogrep infers from configuration (an environment
    /// variable that holds another service's address) as dependencies.
    Catalog {
        path: PathBuf,
        #[serde(default = "yes")]
        inferred: bool,
    },
    /// JSON lines `{"app", "process"}` or `{"app", "vendor"}`.
    Ownership { path: PathBuf },
    /// Runbooks and other prose: other names for an application where the text states them.
    Docs { path: PathBuf },
    Deployments { path: PathBuf },
    ConfigChanges { path: PathBuf },
    /// A git repository holding the services' code and their operations files. Commits newer
    /// than `since` (passed to `ogrep --since` as written) become events, and log lines and
    /// stack traces are located in it.
    Code {
        path: PathBuf,
        #[serde(default = "default_since")]
        since: String,
    },
    Alerts { path: PathBuf },
    /// `classify` asks the model for the fault class of each incident title these files state.
    Incidents {
        path: PathBuf,
        #[serde(default)]
        classify: bool,
    },
    Logs { path: PathBuf },
    Metrics { path: PathBuf },
    Traces { path: PathBuf },
    /// JSON or JSON lines from an error tracker: one exception per record, with its stack trace.
    Errors { path: PathBuf },
    /// `actions` also asks the model for what people did (`evt:OperatorAction`). On by
    /// default since its gates passed (docs/EVIDENCE.md, Phase 13 lane V); `actions = false`
    /// asks with the prompt from before actions existed.
    Discussions {
        path: PathBuf,
        #[serde(default = "yes")]
        actions: bool,
        /// Sends each thread, with `actions = false`, with the catalog applications it may name
        /// by another name. Off by default: the gate has not shown it earns its place.
        #[serde(default)]
        known_apps: bool,
    },
    News { path: PathBuf },
}

fn yes() -> bool {
    true
}

fn default_since() -> String {
    "30d".into()
}

impl Source {
    pub fn name(&self) -> &'static str {
        match self {
            Source::Catalog { .. } => "catalog",
            Source::Ownership { .. } => "ownership",
            Source::Docs { .. } => "docs",
            Source::Deployments { .. } => "deployments",
            Source::ConfigChanges { .. } => "config",
            Source::Code { .. } => "code",
            Source::Alerts { .. } => "alerts",
            Source::Incidents { .. } => "incidents",
            Source::Logs { .. } => "logs",
            Source::Metrics { .. } => "metrics",
            Source::Traces { .. } => "traces",
            Source::Errors { .. } => "errors",
            Source::Discussions { .. } => "discussions",
            Source::News { .. } => "news",
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Source::Catalog { path, .. }
            | Source::Ownership { path }
            | Source::Docs { path }
            | Source::Deployments { path }
            | Source::ConfigChanges { path }
            | Source::Code { path, .. }
            | Source::Alerts { path }
            | Source::Incidents { path, .. }
            | Source::Logs { path }
            | Source::Metrics { path }
            | Source::Traces { path }
            | Source::Errors { path }
            | Source::Discussions { path, .. }
            | Source::News { path } => path,
        }
    }
}

/// Settings for the deterministic detectors. Defaults are documented in docs/REFERENCE.md.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Detect {
    pub metric_z: f64,
    pub metric_baseline_s: i64,
    pub metric_min_run: usize,
    /// Seconds of history a metric baseline needs before the detector judges a point.
    pub metric_min_history_s: i64,
    pub log_burst_factor: f64,
    pub log_min_count: usize,
    /// Seconds of a service's log a message must be absent from before its first error line
    /// counts as a new message. 0 turns the detector off.
    pub log_new_after_s: i64,
    pub trace_error_rate: f64,
    pub trace_slow_factor: f64,
    /// Metric names (substrings) where a drop is as anomalous as a rise, such as throughput.
    pub two_sided: Vec<String>,
}

impl Default for Detect {
    fn default() -> Self {
        Self { metric_z: 5.0, metric_baseline_s: 300, metric_min_run: 3, metric_min_history_s: 150, log_burst_factor: 3.0, log_min_count: 3, log_new_after_s: 600, trace_error_rate: 0.05, trace_slow_factor: 2.0, two_sided: vec!["workload".into(), "throughput".into(), "requests".into()] }
    }
}

pub struct Ctx<'a> {
    pub root: &'a Path,
    pub detect: &'a Detect,
    pub llm: Option<&'a crate::llm::Llm>,
    pub ontology: &'a crate::ontology::Ontology,
    /// Every configured `code` repository, as an absolute path.
    pub code: &'a [PathBuf],
    pub merge: &'a records::Merge,
    /// The catalog's applications as the naming sources of this harvest left them.
    pub apps: &'a [crate::gateway::KnownApp],
}

/// The files a source's enricher reads: for a catalog, the operations files ogrep reads; for
/// a code repository, git's log of where HEAD has been, which changes with every commit, pull
/// and checkout (a whole code tree is too much to watch); for any other source, the path
/// itself or every regular file below it, sorted.
pub fn files(root: &Path, source: &Source) -> Vec<PathBuf> {
    match source {
        Source::Catalog { path, .. } => catalog::files(root, path),
        Source::Code { path, .. } => Some(root.join(path).join(".git/logs/HEAD")).filter(|f| f.is_file()).into_iter().collect(),
        _ => walk(root, source.path()).0,
    }
}

/// A line of code in `repo` as `path:line`, with the path relative to the workspace root.
pub fn code_location(root: &Path, repo: &Path, at: &ogrep::CodeRef) -> String {
    format!("{}:{}", rel(root, &repo.join(&at.path)), at.line)
}

/// [`files`], and the directories below the source path that could not be listed.
fn walk(root: &Path, path: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let p = if path.is_absolute() { path.to_owned() } else { root.join(path) };
    if p.is_file() {
        return (vec![p], vec![]);
    }
    let (mut out, mut unreadable) = (Vec::new(), Vec::new());
    let mut stack = vec![p];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            unreadable.push(d);
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    unreadable.sort();
    (out, unreadable)
}

/// A path below the root as `/`-joined components, so a `\` inside a Unix file name stays
/// part of that name.
pub fn rel(root: &Path, p: &Path) -> String {
    let p = p.strip_prefix(root).unwrap_or(p);
    let parts: Vec<Cow<str>> = p.components().map(|c| if c == std::path::Component::RootDir { Cow::Borrowed("") } else { c.as_os_str().to_string_lossy() }).collect();
    parts.join("/")
}

/// Harvests one source into batches, one per file. A file that fails to parse, or a directory
/// that cannot be listed, becomes a degraded, empty batch instead of stopping the harvest; the
/// gateway keeps the last good facts of a failed batch's key and of every key below it.
pub fn harvest(source: &Source, ctx: &Ctx<'_>) -> Vec<Batch> {
    match source {
        Source::Catalog { path, inferred } => return vec![catalog::harvest(ctx.root, path, *inferred)],
        Source::Code { path, since } => return vec![code::harvest(ctx.root, path, since)],
        Source::Incidents { path, classify } => return vec![incidents(&[(path, *classify)], &[], ctx)],
        _ => {}
    }
    let (files, unreadable) = walk(ctx.root, source.path());
    let mut out: Vec<Batch> = unreadable
        .iter()
        .map(|d| {
            let mut b = Batch::new(source.name(), &rel(ctx.root, d), "failed");
            b.degraded.push(format!("cannot read directory {}", rel(ctx.root, d)));
            b
        })
        .collect();
    let mut bursts = Vec::new();
    for file in files {
        let key = rel(ctx.root, &file);
        let result = match source {
            Source::Catalog { .. } | Source::Code { .. } | Source::Incidents { .. } => unreachable!("harvested as one batch"),
            Source::Errors { .. } => errors::harvest(&file, &key, ctx),
            Source::Ownership { .. } => records::ownership(&file, &key),
            Source::Docs { .. } => docs::harvest(&file, &key),
            Source::Deployments { .. } => records::deployments(&file, &key).map(|(mut b, deploys)| {
                if !ctx.code.is_empty() {
                    records::deployment_files(&mut b, ctx, deploys);
                }
                b
            }),
            Source::ConfigChanges { .. } => records::config_changes(&file, &key),
            Source::Alerts { .. } => records::alerts(&file, &key),
            Source::News { .. } => records::news(&file, &key),
            Source::Logs { .. } => signals::logs(&file, &key, ctx).map(|(b, fired)| {
                bursts.push((out.len(), fired));
                b
            }),
            Source::Metrics { .. } => signals::metrics(&file, &key, ctx.detect),
            Source::Traces { .. } => signals::traces(&file, &key, ctx.detect),
            Source::Discussions { actions, known_apps, .. } => discussions::harvest(&file, &key, ctx, *actions, *known_apps),
        };
        out.push(result.unwrap_or_else(|e| {
            let mut b = Batch::new(source.name(), &key, "failed");
            b.degraded.push(format!("{key}: {e:#}"));
            b
        }));
    }
    signals::locate_bursts(&mut out, ctx, &bursts);
    out
}

/// The files of every incident source as one batch (see [`records::incidents`]). A source path
/// or a directory below it that cannot be listed fails the batch.
/// `held` are the files the last incidents harvest read. Each path says whether its titles
/// are classified.
pub fn incidents(paths: &[(&Path, bool)], held: &[String], ctx: &Ctx<'_>) -> Batch {
    let (mut files, mut degraded) = (Vec::new(), Vec::new());
    for &(path, classify) in paths {
        let (found, unreadable) = walk(ctx.root, path);
        // A directory that held incident files and now lists none is more likely an unmounted
        // share than an empty export, and every incident shares one graph, so it fails the batch.
        let dir = rel(ctx.root, &ctx.root.join(path));
        if found.is_empty() && unreadable.is_empty() && ctx.root.join(path).is_dir() && held.iter().any(|f| f.starts_with(&format!("{dir}/"))) {
            degraded.push(format!("{dir} lists no files; keeping the incidents it held"));
        }
        files.extend(found.into_iter().map(|f| (rel(ctx.root, &f), f, classify)));
        degraded.extend(unreadable.iter().map(|d| format!("cannot read directory {}", rel(ctx.root, d))));
    }
    // A file under two sources is classified when either asks.
    files.sort_by(|x, y| (&x.0, y.2).cmp(&(&y.0, x.2)));
    files.dedup_by(|x, y| x.0 == y.0);
    let mut b = records::incidents(&files.iter().map(|(key, f, c)| (f.clone(), key.clone(), *c)).collect::<Vec<_>>(), degraded, ctx);
    b.files = files.into_iter().map(|(key, ..)| key).collect();
    b
}

/// Seconds, milliseconds, microseconds or nanoseconds since the epoch (number or numeric string), or RFC 3339 / ISO 8601
/// with any fraction (cut to milliseconds) and any offset (no offset means UTC).
pub fn parse_time(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => epoch_ms(n.as_f64()?),
        serde_json::Value::String(s) => parse_time_str(s),
        _ => None,
    }
}

/// [`parse_time`] of a JSON string.
pub fn parse_time_str(s: &str) -> Option<i64> {
    s.parse::<f64>().ok().map_or_else(|| parse_iso(s), epoch_ms)
}

/// Unix seconds below 1e10, milliseconds below 1e13, microseconds below 1e16, nanoseconds
/// above. The cast saturates, so NaN and infinities land outside [`plausible`].
fn epoch_ms(n: f64) -> Option<i64> {
    let scale = match n.abs() {
        x if x < 1e10 => 1000.0,
        x if x < 1e13 => 1.0,
        x if x < 1e16 => 1e-3,
        _ => 1e-6,
    };
    plausible((n * scale).round() as i64)
}

/// Times from 1971 to 2200. Anything else is a year, a compact date or an overflow, not a time.
fn plausible(ms: i64) -> Option<i64> {
    (31_536_000_000..7_258_118_400_000).contains(&ms).then_some(ms)
}

pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let s = match s.strip_suffix(" UTC").or_else(|| s.strip_suffix(" GMT")) {
        Some(bare) => format!("{bare}Z"),
        None => s.to_owned(),
    };
    let s = s.replacen(' ', "T", 1);
    let (main, zone) = match s.find(['Z', '+']).or_else(|| s.get(10..).and_then(|t| t.find('-')).map(|i| i + 10)) {
        Some(i) => s.split_at(i),
        None => (s.as_str(), "Z"),
    };
    let zone = match (zone.len(), zone.is_ascii() && !zone.contains(':')) {
        (5, true) => format!("{}:{}", &zone[..3], &zone[3..]),
        (3, true) => format!("{zone}:00"),
        _ => zone.to_owned(),
    };
    let main = match main.split_once('.') {
        Some((a, f)) => format!("{a}.{:0<3}", f.chars().take(3).collect::<String>()),
        None => main.to_owned(),
    };
    let dt: oxsdatatypes::DateTime = format!("{main}{zone}").parse().ok()?;
    plausible(crate::term::millis_of(dt.adjust(Some(oxsdatatypes::TimezoneOffset::UTC))?)?)
}

pub fn app(name: &str) -> Object {
    Object::Entity(EntityRef::Provisional { class: ops::APPLICATION, name: name.to_owned() })
}

/// Every event class and signal value an enricher writes. `ontology check` reports the event
/// classes and signal values outside it as declared terms nothing writes.
pub const WRITES: &[(&str, &str)] = &[
    (evt::DEPLOYMENT, "DEPLOYMENT"),
    (evt::CONFIG_CHANGE, "CONFIG_CHANGE"),
    (evt::ALERT, "ALERT"),
    (crate::vocab::inc::INCIDENT, "INCIDENT"),
    (evt::LOG_ERROR, "LOG_ERROR"),
    (evt::LOG_ERROR, "LOG_NEW"),
    (evt::METRIC_ANOMALY, "METRIC_ANOMALY"),
    (evt::TRACE_FAILED, "TRACE_FAILED"),
    (evt::TRACE_SLOW, "TRACE_SLOW"),
    (evt::NEWS_ITEM, "NEWS"),
    (evt::COMMIT, "COMMIT"),
    (evt::EXCEPTION, "EXCEPTION"),
    (evt::OPERATOR_ACTION, crate::correlate::ACTION_SIGNAL),
];

/// The facts every event carries. `details` go between its time and its value.
pub struct Event<'a> {
    pub class: &'a str,
    pub on: &'a str,
    pub at: i64,
    pub signal: &'a str,
    pub severity: &'a str,
    pub value: Option<f64>,
}

pub fn event<const N: usize>(b: &mut Batch, s: &EntityRef, e: Event<'_>, details: [(&str, Object); N], ev: &EvidenceRef, m: &Extraction) {
    debug_assert!(WRITES.contains(&(e.class, e.signal)), "{} {} is missing from enrich::WRITES", e.class, e.signal);
    b.add(s.clone(), RDF_TYPE, obj(oxrdf::NamedNode::new_unchecked(e.class)), ev, m);
    b.add(s.clone(), evt::ON, app(e.on), ev, m);
    b.add(s.clone(), evt::AT, dt(e.at), ev, m);
    for (p, o) in details {
        b.add(s.clone(), p, o, ev, m);
    }
    if let Some(v) = e.value {
        b.add(s.clone(), evt::VALUE, lit(crate::term::decimal(v)), ev, m);
    }
    b.add(s.clone(), evt::SIGNAL, lit(e.signal), ev, m);
    b.add(s.clone(), evt::SEVERITY, lit(e.severity), ev, m);
    b.add(s.clone(), evt::SOURCE_RECORD, lit(ev.0.as_str()), ev, m);
}

pub fn dt(ms: i64) -> crate::model::Object {
    crate::model::lit(oxrdf::Literal::new_typed_literal(crate::term::format_millis(ms), oxrdf::vocab::xsd::DATE_TIME))
}

/// A source file as text, without the byte-order mark Windows exports start with. A stray
/// invalid byte becomes U+FFFD rather than failing the file. An empty file is an error, so an
/// exporter that truncates before it writes keeps the facts its file had.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if text.trim().is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "empty file, keeping its previous facts"));
    }
    Ok(text.to_owned())
}

/// Reads JSON lines, one JSON array or one JSON object (pretty-printed or not) into values.
pub fn json_records(path: &Path) -> anyhow::Result<Vec<serde_json::Value>> {
    let text = read_text(path)?;
    json_record_texts(&text)?.into_iter().map(|t| Ok(serde_json::from_str(t)?)).collect()
}

/// The records of a file of JSON lines, one JSON array or one JSON object, as raw JSON text.
pub fn json_record_texts(text: &str) -> serde_json::Result<Vec<&str>> {
    let t = text.trim_start();
    if t.starts_with('[') {
        let all: Vec<&RawValue> = serde_json::from_str(t)?;
        return Ok(all.into_iter().map(|r| r.get()).collect());
    }
    if t.starts_with('{')
        && let Ok(one) = serde_json::from_str::<&RawValue>(t)
    {
        return Ok(vec![one.get()]);
    }
    Ok(text.lines().filter(|l| !l.trim().is_empty()).collect())
}

/// The fields `keys` of one JSON value as raw JSON text, without building the value: absent
/// where the value lacks one or is not an object, the last one where a key repeats. Errors
/// match `serde_json::from_str`, except that a listed field's number is not range-checked.
pub fn json_fields<'a, const N: usize>(text: &'a str, keys: [&str; N]) -> serde_json::Result<[Option<&'a RawValue>; N]> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
    // A bare `Cow` key always deserializes owned; `borrow` keeps unescaped keys in the text.
    #[derive(serde::Deserialize)]
    struct Key<'a>(#[serde(borrow)] Cow<'a, str>);
    struct Fields<'k, const N: usize>(&'k [&'k str; N]);
    impl<'de, const N: usize> Visitor<'de> for Fields<'_, N> {
        type Value = [Option<&'de RawValue>; N];
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            let mut out = [None; N];
            while let Some(Key(k)) = a.next_key()? {
                match self.0.iter().position(|x| *x == k) {
                    Some(i) => out[i] = Some(a.next_value()?),
                    None => drop(a.next_value::<IgnoredAny>()?),
                }
            }
            Ok(out)
        }
    }
    impl<'de, const N: usize> DeserializeSeed<'de> for Fields<'_, N> {
        type Value = [Option<&'de RawValue>; N];
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    let mut de = serde_json::Deserializer::from_str(text);
    match Fields(&keys).deserialize(&mut de).and_then(|v| de.end().map(|()| v)) {
        Ok(v) => Ok(v),
        // Valid JSON that is not an object has no fields; anything else is the parse error.
        Err(_) => serde_json::from_str::<IgnoredAny>(text).map(|_| [None; N]),
    }
}

/// A raw JSON field as a string, or nothing if it is not one.
pub fn raw_str(v: &RawValue) -> Option<Cow<'_, str>> {
    let s = v.get();
    if !s.starts_with('"') {
        return None;
    }
    serde_json::from_str(s).map(Cow::Borrowed).or_else(|_| serde_json::from_str(s).map(Cow::Owned)).ok()
}

/// A raw JSON field as a number, or nothing if it is not one.
pub fn raw_f64(v: &RawValue) -> Option<f64> {
    serde_json::from_str(v.get()).ok()
}

/// [`parse_time`] of a raw JSON field.
pub fn raw_time(v: &RawValue) -> Option<i64> {
    match raw_str(v) {
        Some(s) => parse_time_str(&s),
        None => epoch_ms(raw_f64(v)?),
    }
}

pub fn str_field<'a>(v: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| v.get(*n).and_then(|x| x.as_str()))
}

#[cfg(test)]
mod tests {
    use super::{parse_iso, parse_time_str};

    #[test]
    fn an_epoch_number_is_read_by_its_magnitude() {
        let y2000 = Some(946_684_800_000);
        for s in ["946684800", "946684800000", "946684800000000", "946684800000000000"] {
            assert_eq!(parse_time_str(s), y2000, "{s}");
        }
    }

    #[test]
    fn a_number_that_is_not_a_plausible_time_is_rejected() {
        for s in ["nan", "inf", "-inf", "2026", "20261122", "1e30", "-1700000000", "9999999999999"] {
            assert_eq!(parse_time_str(s), None, "{s}");
        }
        assert_eq!(parse_time_str("2300-01-01T00:00:00Z"), None);
    }

    #[test]
    fn every_offset_spelling_is_the_same_instant() {
        let z = parse_iso("2024-03-01T12:00:00Z");
        assert_eq!(z, Some(1_709_294_400_000));
        for s in ["2024-03-01T12:00:00+0000", "2024-03-01 12:00:00 UTC", "2024-03-01T12:00:00+00", "2024-03-01T17:30:00+0530", "2024-03-01T06:30:00-0530"] {
            assert_eq!(parse_iso(s), z, "{s}");
        }
        assert_eq!(parse_iso("2024-03-01T12:00:00.12éé"), None);
    }
}
