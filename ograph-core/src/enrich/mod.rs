//! Enrichers turn source files into [`Batch`]es of proposed facts. They never write: the
//! gateway does. They run in Netflix's layer order, and each source file is one batch whose key
//! is the file's path, so re-harvesting a file replaces its graph.

pub mod catalog;
pub mod discussions;
pub mod docs;
pub mod drain;
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Runs `ogrep --entities . --json` in a repository: Backstage, Compose, Kubernetes.
    Catalog { path: PathBuf },
    /// JSON lines `{"app", "process"}` or `{"app", "vendor"}`.
    Ownership { path: PathBuf },
    /// Runbooks and other prose: other names for an application where the text states them.
    Docs { path: PathBuf },
    Deployments { path: PathBuf },
    ConfigChanges { path: PathBuf },
    Alerts { path: PathBuf },
    Incidents { path: PathBuf },
    Logs { path: PathBuf },
    Metrics { path: PathBuf },
    Traces { path: PathBuf },
    Discussions { path: PathBuf },
    News { path: PathBuf },
}

impl Source {
    pub fn name(&self) -> &'static str {
        match self {
            Source::Catalog { .. } => "catalog",
            Source::Ownership { .. } => "ownership",
            Source::Docs { .. } => "docs",
            Source::Deployments { .. } => "deployments",
            Source::ConfigChanges { .. } => "config",
            Source::Alerts { .. } => "alerts",
            Source::Incidents { .. } => "incidents",
            Source::Logs { .. } => "logs",
            Source::Metrics { .. } => "metrics",
            Source::Traces { .. } => "traces",
            Source::Discussions { .. } => "discussions",
            Source::News { .. } => "news",
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Source::Catalog { path }
            | Source::Ownership { path }
            | Source::Docs { path }
            | Source::Deployments { path }
            | Source::ConfigChanges { path }
            | Source::Alerts { path }
            | Source::Incidents { path }
            | Source::Logs { path }
            | Source::Metrics { path }
            | Source::Traces { path }
            | Source::Discussions { path }
            | Source::News { path } => path,
        }
    }
}

/// Settings for the deterministic detectors. Defaults are documented in docs/REFERENCE.md.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Detect {
    pub metric_z: f64,
    pub metric_baseline_s: i64,
    pub metric_min_run: usize,
    /// Seconds of history a metric baseline needs before the detector judges a point.
    pub metric_min_history_s: i64,
    pub log_burst_factor: f64,
    pub log_min_count: usize,
    pub trace_error_rate: f64,
    pub trace_slow_factor: f64,
    /// Metric names (substrings) where a drop is as anomalous as a rise, such as throughput.
    pub two_sided: Vec<String>,
}

impl Default for Detect {
    fn default() -> Self {
        Self { metric_z: 5.0, metric_baseline_s: 300, metric_min_run: 3, metric_min_history_s: 150, log_burst_factor: 3.0, log_min_count: 3, trace_error_rate: 0.05, trace_slow_factor: 2.0, two_sided: vec!["workload".into(), "throughput".into(), "requests".into()] }
    }
}

pub struct Ctx<'a> {
    pub root: &'a Path,
    pub detect: &'a Detect,
    pub llm: Option<&'a crate::llm::Llm>,
    pub ontology: &'a crate::ontology::Ontology,
}

/// Files under a source path: the path itself, or every regular file below it, sorted.
pub fn files(root: &Path, path: &Path) -> Vec<PathBuf> {
    let p = if path.is_absolute() { path.to_owned() } else { root.join(path) };
    if p.is_file() {
        return vec![p];
    }
    let mut out = Vec::new();
    let mut stack = vec![p];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
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
    out
}

pub fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Harvests one source into batches, one per file. A file that fails to parse becomes a
/// degraded, empty batch instead of stopping the harvest.
pub fn harvest(source: &Source, ctx: &Ctx<'_>) -> Vec<Batch> {
    if let Source::Catalog { path } = source {
        return vec![catalog::harvest(ctx.root, path)];
    }
    let mut out = Vec::new();
    for file in files(ctx.root, source.path()) {
        let key = rel(ctx.root, &file);
        let result = match source {
            Source::Catalog { .. } => unreachable!(),
            Source::Ownership { .. } => records::ownership(&file, &key),
            Source::Docs { .. } => docs::harvest(&file, &key),
            Source::Deployments { .. } => records::deployments(&file, &key),
            Source::ConfigChanges { .. } => records::config_changes(&file, &key),
            Source::Alerts { .. } => records::alerts(&file, &key),
            Source::Incidents { .. } => records::incidents(&file, &key),
            Source::News { .. } => records::news(&file, &key),
            Source::Logs { .. } => signals::logs(&file, &key, ctx.detect),
            Source::Metrics { .. } => signals::metrics(&file, &key, ctx.detect),
            Source::Traces { .. } => signals::traces(&file, &key, ctx.detect),
            Source::Discussions { .. } => discussions::harvest(&file, &key, ctx),
        };
        out.push(result.unwrap_or_else(|e| {
            let mut b = Batch::new(source.name(), &key, "failed");
            b.degraded.push(format!("{key}: {e:#}"));
            b
        }));
    }
    out
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

fn epoch_ms(n: f64) -> Option<i64> {
    let scale = match n.abs() {
        x if x > 1e17 => 1e-6,
        x if x > 1e14 => 1e-3,
        x if x > 1e12 => 1.0,
        _ => 1000.0,
    };
    Some((n * scale).round() as i64)
}

pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim().replacen(' ', "T", 1);
    let (main, zone) = match s.find(['Z', '+']).or_else(|| s.get(10..).and_then(|t| t.find('-')).map(|i| i + 10)) {
        Some(i) => s.split_at(i),
        None => (s.as_str(), "Z"),
    };
    let main = match main.split_once('.') {
        Some((a, f)) => format!("{a}.{:0<3}", &f[..f.len().min(3)]),
        None => main.to_owned(),
    };
    let dt: oxsdatatypes::DateTime = format!("{main}{zone}").parse().ok()?;
    crate::term::millis_of(dt.adjust(Some(oxsdatatypes::TimezoneOffset::UTC))?)
}

pub fn app(name: &str) -> Object {
    Object::Entity(EntityRef::Provisional { class: ops::APPLICATION, name: name.to_owned() })
}

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
}

pub fn dt(ms: i64) -> crate::model::Object {
    crate::model::lit(oxrdf::Literal::new_typed_literal(crate::term::format_millis(ms), oxrdf::vocab::xsd::DATE_TIME))
}

/// A source file as text, without the byte-order mark Windows exports start with.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_owned(),
        None => text,
    })
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
