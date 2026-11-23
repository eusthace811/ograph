//! Enrichers turn source files into [`Batch`]es of proposed facts. They never write: the
//! gateway does. They run in Netflix's layer order, and each source file is one batch whose key
//! is the file's path, so re-harvesting a file replaces its graph.

pub mod catalog;
pub mod discussions;
pub mod docs;
pub mod drain;
pub mod records;
pub mod signals;

use crate::model::Batch;
use serde::Deserialize;
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

/// Seconds or milliseconds since the epoch (number or numeric string), or RFC 3339 / ISO 8601
/// with any fraction (cut to milliseconds) and any offset (no offset means UTC).
pub fn parse_time(v: &serde_json::Value) -> Option<i64> {
    let from_num = |n: f64| Some(if n.abs() > 1e12 { n as i64 } else { (n * 1000.0).round() as i64 });
    match v {
        serde_json::Value::Number(n) => from_num(n.as_f64()?),
        serde_json::Value::String(s) => s.parse::<f64>().ok().map_or_else(|| parse_iso(s), from_num),
        _ => None,
    }
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

pub fn dt(ms: i64) -> crate::model::Object {
    crate::model::lit(oxrdf::Literal::new_typed_literal(crate::term::format_millis(ms), oxrdf::vocab::xsd::DATE_TIME))
}

/// Reads JSON lines, or one JSON array, into values.
pub fn json_records(path: &Path) -> anyhow::Result<Vec<serde_json::Value>> {
    let text = std::fs::read_to_string(path)?;
    let t = text.trim_start();
    if t.starts_with('[') {
        return Ok(serde_json::from_str(t)?);
    }
    if t.starts_with('{') && !t.contains('\n') {
        return Ok(vec![serde_json::from_str(t)?]);
    }
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| Ok(serde_json::from_str(l)?)).collect()
}

/// The fields `keys` of each JSON line (or of each element of one JSON array), absent where a
/// record lacks one or is not an object.
pub fn json_records_fields<const N: usize>(path: &Path, keys: [&str; N]) -> anyhow::Result<Vec<[Option<serde_json::Value>; N]>> {
    let text = std::fs::read_to_string(path)?;
    let t = text.trim_start();
    if t.starts_with('[') || t.starts_with('{') && !t.contains('\n') {
        let all: Vec<serde_json::Value> = if t.starts_with('[') { serde_json::from_str(t)? } else { vec![serde_json::from_str(t)?] };
        return Ok(all.iter().map(|v| keys.map(|k| v.get(k).cloned())).collect());
    }
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| Ok(json_fields(l, keys)?)).collect()
}

/// The fields `keys` of one JSON value, as `v.get(key)` would give them, without building the
/// value's other fields or a map. Anything the fast path rejects (not an object, escaped keys,
/// bad JSON) is parsed whole, so results and errors match `serde_json::from_str`.
pub fn json_fields<const N: usize>(line: &str, keys: [&str; N]) -> serde_json::Result<[Option<serde_json::Value>; N]> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
    struct Fields<'k, const N: usize>(&'k [&'k str; N]);
    impl<'de, const N: usize> Visitor<'de> for Fields<'_, N> {
        type Value = [Option<serde_json::Value>; N];
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            let mut out = std::array::from_fn(|_| None);
            while let Some(k) = a.next_key::<&'de str>()? {
                match self.0.iter().position(|x| *x == k) {
                    Some(i) => out[i] = Some(a.next_value()?),
                    None => drop(a.next_value::<IgnoredAny>()?),
                }
            }
            Ok(out)
        }
    }
    impl<'de, const N: usize> DeserializeSeed<'de> for Fields<'_, N> {
        type Value = [Option<serde_json::Value>; N];
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    let mut de = serde_json::Deserializer::from_str(line);
    match Fields(&keys).deserialize(&mut de).and_then(|v| de.end().map(|()| v)) {
        Ok(v) => Ok(v),
        Err(_) => {
            let v: serde_json::Value = serde_json::from_str(line)?;
            Ok(keys.map(|k| v.get(k).cloned()))
        }
    }
}

pub fn str_field<'a>(v: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| v.get(*n).and_then(|x| x.as_str()))
}
