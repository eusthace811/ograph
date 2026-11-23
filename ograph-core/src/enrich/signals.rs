//! Signal sources: logs, metrics and traces. Raw lines, points and spans stay in their files;
//! the graph gets only the episodes a detector finds (one event per episode) with a pointer
//! back to the first record. That is what bounds the graph's size.
//!
//! Detectors are online: each point is judged against a trailing baseline, never against a
//! label or a known incident time.

use super::drain::Drain;
use super::{Detect, Event, app, dt, event, json_fields, json_record_texts, parse_time_str, raw_f64, raw_str, raw_time};
use crate::model::{Batch, EntityRef, Extraction, known, lit, slug};
use crate::vocab::{evt, ops};
use oxrdf::{Literal, NamedNode};
use rustc_hash::FxHashMap;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

fn severity(score: f64) -> &'static str {
    if score >= 0.8 {
        "SEV1"
    } else if score >= 0.5 {
        "SEV2"
    } else if score >= 0.25 {
        "SEV3"
    } else {
        "SEV4"
    }
}

/// Normalized strength of an anomaly in [0, 1]: log-scaled so z=5 is 0 and z=200 is 1.
pub fn strength(z: f64) -> f64 {
    (((z.abs().max(1.0)).ln() - 5f64.ln()).max(0.0) / (200f64.ln() - 5f64.ln())).min(1.0)
}

struct Episode {
    start_ms: i64,
    end_ms: i64,
    peak: f64,
    first_row: usize,
    len: usize,
}

const MIN_BASELINE_POINTS: i64 = 12;

fn median(v: &mut [f64]) -> f64 {
    let n = v.len();
    let (below, mid, _) = v.select_nth_unstable_by(n / 2, f64::total_cmp);
    let mid = *mid;
    if n % 2 == 1 { mid } else { (below.iter().copied().max_by(f64::total_cmp).unwrap() + mid) / 2.0 }
}

/// Robust z-score episodes over one series of `(ms, value, row)`. One-sided (increases) unless
/// `two_sided`.
fn episodes(series: &[(i64, f64, usize)], d: &Detect, two_sided: bool) -> Vec<Episode> {
    let mut out = Vec::new();
    let mut base: Option<(f64, f64)> = None;
    let mut last_base_at: Option<i64> = None;
    let mut cur: Option<Episode> = None;
    let mut misses = 0;
    let mut lo = 0;
    let gap_ms = 10_000;
    // A coarsely sampled series widens its window until the baseline holds enough points.
    let mut steps: Vec<f64> = series.windows(2).take(256).map(|w| (w[1].0 - w[0].0) as f64).filter(|s| *s > 0.0).collect();
    let step = if steps.is_empty() { 0 } else { median(&mut steps) as i64 };
    let window_ms = (d.metric_baseline_s * 1000).max(MIN_BASELINE_POINTS * step + gap_ms);
    for (i, &(t, v, row)) in series.iter().enumerate() {
        if !v.is_finite() {
            continue;
        }
        if cur.is_none() && last_base_at.is_none_or(|b| t - b >= 10_000) {
            while lo < i && series[lo].0 < t - window_ms {
                lo += 1;
            }
            let mut hist: Vec<f64> = series[lo..i].iter().filter(|x| x.0 < t - gap_ms && x.1.is_finite()).map(|x| x.1).collect();
            if hist.len() >= MIN_BASELINE_POINTS as usize && t - series[lo].0 >= d.metric_min_history_s * 1000 {
                let m = median(&mut hist);
                let mut dev: Vec<f64> = hist.iter().map(|x| (x - m).abs()).collect();
                let mad = median(&mut dev);
                let (lo_v, hi_v) = hist.iter().fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(*x), b.max(*x)));
                // Sparse series (mostly zero with spikes) have a MAD of 0; the baseline's own
                // range keeps one routine spike from scoring like an outage.
                base = Some((m, (1.4826 * mad).max(0.01 * m.abs()).max((hi_v - lo_v) / 6.0).max(1e-6)));
                last_base_at = Some(t);
            }
        }
        let Some((m, scale)) = base else { continue };
        let z = (v - m) / scale;
        let z = if two_sided { z.abs() } else { z };
        if z >= d.metric_z {
            misses = 0;
            match &mut cur {
                Some(e) => {
                    e.len += 1;
                    e.end_ms = t;
                    if z > e.peak {
                        e.peak = z;
                    }
                }
                None => cur = Some(Episode { start_ms: t, end_ms: t, peak: z, first_row: row, len: 1 }),
            }
        } else if let Some(e) = &cur {
            misses += 1;
            if misses > 2 {
                if e.len >= d.metric_min_run {
                    out.push(cur.take().unwrap());
                } else {
                    cur = None;
                }
                last_base_at = None;
            }
        }
    }
    if let Some(e) = cur.filter(|e| e.len >= d.metric_min_run) {
        out.push(e);
    }
    out
}

/// CSV with a `time` column and one `<service>_<metric>` column per series.
pub fn metrics(path: &Path, key: &str, d: &Detect) -> anyhow::Result<Batch> {
    let mut b = Batch::new("metrics", key, "robust-z-v1");
    let text = super::read_text(path)?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().ok_or_else(|| anyhow::anyhow!("empty CSV"))?.split(',').collect();
    let tcol = header.iter().position(|h| matches!(h.trim(), "time" | "timestamp" | "ts")).ok_or_else(|| anyhow::anyhow!("no time column"))?;
    let mut cols: Vec<Vec<(i64, f64, usize)>> = vec![Vec::new(); header.len()];
    for (row, line) in lines.enumerate() {
        let cells: Vec<&str> = line.split(',').collect();
        let Some(t) = cells.get(tcol).and_then(|c| parse_time_str(c.trim())) else { continue };
        for (c, cell) in cells.iter().enumerate() {
            if c != tcol
                && let Some(col) = cols.get_mut(c)
                && let Ok(v) = cell.trim().parse::<f64>()
            {
                col.push((t, v, row + 2));
            }
        }
    }
    let m = Extraction::Rule { extractor: "robust-z".into() };
    for (c, series) in cols.iter_mut().enumerate() {
        if c == tcol || series.len() < 40 {
            continue;
        }
        series.sort_by_key(|x| x.0);
        let name = header[c].trim();
        let Some((svc, metric)) = name.split_once('_') else { continue };
        // Faults raise resource use, latency and errors; a drop there is a symptom of less
        // traffic. Only throughput-like series count in both directions.
        let two_sided = d.two_sided.iter().any(|k| metric.contains(k.as_str()));
        for e in episodes(series, d, two_sided) {
            let ev = b.record(format!("{key}:{}", e.first_row));
            let s = known("ev", &format!("metric/{}/{}/{}", slug(svc), slug(metric), e.start_ms));
            let st = strength(e.peak);
            let details = [(evt::UNTIL, dt(e.end_ms)), (evt::METRIC, lit(metric))];
            event(&mut b, &s, Event { class: evt::METRIC_ANOMALY, on: svc, at: e.start_ms, signal: "METRIC_ANOMALY", severity: severity(st), value: Some(e.peak) }, details, &ev, &m);
        }
    }
    Ok(b)
}

fn error_like() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)\b(error|exception|fail(ed|ure|s)?|timeout|timed out|refused|unavailable|deadline|panic|critical|fatal|unreachable|reset)\b").unwrap())
}

/// JSON lines `{"ts", "service", "message", "level"?}` or plain text with leading timestamps
/// (the service is then the file stem).
pub fn logs(path: &Path, key: &str, d: &Detect) -> anyhow::Result<Batch> {
    let mut b = Batch::new("logs", key, "drain-burst-v1");
    let text = super::read_text(path)?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // Per service: one Drain, and per line its (time ms, template index, error-like).
    type Lines = Vec<(i64, usize, bool)>;
    let mut per: BTreeMap<String, (Drain, Lines)> = BTreeMap::new();
    for line in text.lines() {
        let (svc, t, msg, level) = if line.trim_start().starts_with('{') {
            let Ok(f) = json_fields(line, ["ts", "timestamp", "time", "service", "container_name", "app", "message", "msg", "log", "level", "severity"]) else { continue };
            let Some(t) = f[0..3].iter().flatten().find_map(|v| raw_time(v)) else { continue };
            let svc = first_str(&f[3..6]).map_or_else(|| stem.clone(), Cow::into_owned);
            (svc, t, first_str(&f[6..9]).map(Cow::into_owned).unwrap_or_default(), first_str(&f[9..11]).map(|l| l.to_lowercase()))
        } else {
            let (Some(t), rest) = super::drain::leading_timestamp(line) else { continue };
            (stem.clone(), t * 1000, rest.to_owned(), None)
        };
        let errorish = level.as_deref().is_some_and(|l| matches!(l, "error" | "err" | "fatal" | "critical" | "warn" | "warning")) || error_like().is_match(&msg);
        let entry = per.entry(svc).or_default();
        let Some(idx) = entry.0.add(&msg) else { continue };
        entry.1.push((t, idx, errorish));
    }
    let m = Extraction::Rule { extractor: "drain-burst".into() };
    for (svc, (drain, lines)) in per {
        let Some(t0) = lines.iter().map(|l| l.0).min() else { continue };
        let mut buckets: BTreeMap<usize, BTreeMap<i64, (usize, i64)>> = BTreeMap::new();
        for &(t, idx, errorish) in &lines {
            if errorish {
                let e = buckets.entry(idx).or_default().entry(t.div_euclid(60_000)).or_insert((0, t));
                e.0 += 1;
                e.1 = e.1.min(t);
            }
        }
        for (idx, minutes) in buckets {
            let text = drain.templates.get(idx).map(|t| t.text()).unwrap_or_default();
            let mut fired_until = i64::MIN;
            for (&minute, &(count, first)) in &minutes {
                let prior: f64 = (1..=5).map(|k| minutes.get(&(minute - k)).map_or(0, |x| x.0) as f64).sum::<f64>() / 5.0;
                let warm = first - t0 >= 120_000;
                let burst = count >= d.log_min_count && (count as f64) >= d.log_burst_factor * prior.max(1.0 / d.log_burst_factor);
                if !(warm && burst) || minute <= fired_until {
                    continue;
                }
                fired_until = minute + 5;
                let ratio = count as f64 / prior.max(0.2);
                let ev = b.record(format!("{key}@{}", first));
                let s = known("ev", &format!("log/{}/{}/{first}", slug(&svc), &hex::encode(crate::gateway::sha2_hash(&text))[..12]));
                let e = Event { class: evt::LOG_ERROR, on: &svc, at: first, signal: "LOG_ERROR", severity: severity(strength(ratio * 5.0)), value: Some(ratio * 5.0) };
                let details = [(evt::TEMPLATE, lit(text.chars().take(300).collect::<String>())), (evt::COUNT, lit(Literal::from(count as i64)))];
                event(&mut b, &s, e, details, &ev, &m);
            }
        }
    }
    Ok(b)
}

/// The first of `fields` that is a string.
fn first_str<'a>(fields: &[Option<&'a RawValue>]) -> Option<Cow<'a, str>> {
    fields.iter().flatten().find_map(|v| raw_str(v))
}

struct Span {
    service: u32,
    start: i64,
    dur_us: f64,
    error: bool,
    row: usize,
}

/// JSON lines of spans: `trace_id`/`traceID`, `span_id`/`spanID`, `parent_span_id`/`parentSpanID`,
/// `service`/`serviceName`, `start_ms`/`startTimeMillis`, `duration_us`/`duration`, `status`/`statusCode`.
/// A parent is looked up in the span's own trace; spans without a trace id form one trace.
pub fn traces(path: &Path, key: &str, d: &Detect) -> anyhow::Result<Batch> {
    let mut b = Batch::new("traces", key, "span-stats-v1");
    let text = super::read_text(path)?;
    let records = json_record_texts(&text)?;
    let mut services: Vec<Cow<str>> = Vec::new();
    let mut service_ids: FxHashMap<Cow<str>, u32> = FxHashMap::default();
    let mut spans = Vec::with_capacity(records.len());
    let mut by_id: FxHashMap<(Option<Cow<str>>, Cow<str>), usize> = FxHashMap::with_capacity_and_hasher(records.len(), Default::default());
    let mut parents = Vec::with_capacity(records.len());
    let keys = ["service", "serviceName", "start_ms", "startTimeMillis", "duration_us", "duration", "status", "statusCode", "span_id", "spanID", "parent_span_id", "parentSpanID", "trace_id", "traceID"];
    for (i, record) in records.into_iter().enumerate() {
        let f = json_fields(record, keys)?;
        let (Some(svc), Some(start)) = (first_str(&f[0..2]), f[2..4].iter().flatten().find_map(|v| raw_time(v))) else { continue };
        let dur = f[4..6].iter().flatten().find_map(|v| raw_f64(v).or_else(|| raw_str(v)?.parse().ok())).unwrap_or(0.0);
        let error = f[6].or(f[7]).is_some_and(|v| match raw_str(v) {
            Some(s) => matches!(s.to_lowercase().as_str(), "error" | "2" | "status_code_error"),
            None => serde_json::from_str::<i64>(v.get()).is_ok_and(|n| n == 2 || n >= 400),
        });
        let trace = first_str(&f[12..14]);
        if let Some(id) = first_str(&f[8..10]) {
            by_id.insert((trace.clone(), id), spans.len());
        }
        parents.push(first_str(&f[10..12]).map(|p| (trace, p)));
        let service = *service_ids.entry(svc).or_insert_with_key(|s| {
            services.push(s.clone());
            services.len() as u32 - 1
        });
        spans.push(Span { service, start, dur_us: dur, error, row: i + 1 });
    }
    let rule = Extraction::Rule { extractor: "span-stats".into() };
    let mut edges: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (i, p) in parents.iter().enumerate() {
        if let Some(&j) = p.as_ref().and_then(|p| by_id.get(p))
            && spans[j].service != spans[i].service {
                edges.entry((&services[spans[j].service as usize], &services[spans[i].service as usize])).or_insert(spans[i].row);
            }
    }
    let inferred = Extraction::Rule { extractor: "span-parent:inferred".into() };
    for ((from, to), row) in edges {
        let ev = b.record(format!("{key}#{row}"));
        b.facts.push(crate::model::ProposedFact {
            subject: EntityRef::Provisional { class: ops::APPLICATION, name: from.to_owned() },
            predicate: NamedNode::new_unchecked(ops::DEPENDS_ON),
            object: app(to),
            evidence: vec![ev],
            method: inferred.clone(),
        });
    }
    let mut per: BTreeMap<&str, BTreeMap<i64, Vec<usize>>> = BTreeMap::new();
    for (i, s) in spans.iter().enumerate() {
        per.entry(&services[s.service as usize]).or_default().entry(s.start.div_euclid(60_000)).or_default().push(i);
    }
    for (svc, minutes) in per {
        let stats: BTreeMap<i64, (usize, usize, f64, usize)> = minutes
            .iter()
            .map(|(&m, idx)| {
                let mut durs: Vec<f64> = idx.iter().map(|&i| spans[i].dur_us).collect();
                durs.sort_by(f64::total_cmp);
                let p95 = durs[((durs.len() - 1) as f64 * 0.95) as usize];
                let first = idx.iter().map(|&i| spans[i].row).min().unwrap_or(0);
                (m, (idx.len(), idx.iter().filter(|&&i| spans[i].error).count(), p95, first))
            })
            .collect();
        let mut fired = [i64::MIN, i64::MIN];
        for (&m, &(n, errs, p95, row)) in &stats {
            let prior: Vec<&(usize, usize, f64, usize)> = (1..=5).filter_map(|k| stats.get(&(m - k))).collect();
            if prior.len() < 3 {
                continue;
            }
            let base_err = prior.iter().map(|x| x.1 as f64 / x.0.max(1) as f64).sum::<f64>() / prior.len() as f64;
            let mut base_p95: Vec<f64> = prior.iter().map(|x| x.2).collect();
            let base_p95 = median(&mut base_p95).max(1.0);
            let rate = errs as f64 / n.max(1) as f64;
            let t = minutes[&m].iter().map(|&i| spans[i].start).min().unwrap_or(m * 60_000);
            let mut emit = |b: &mut Batch, kind: &str, class: &str, signal: &str, score: f64, k: usize| {
                if m <= fired[k] {
                    return;
                }
                fired[k] = m + 5;
                let ev = b.record(format!("{key}#{row}"));
                let s = known("ev", &format!("trace/{}/{kind}/{t}", slug(svc)));
                event(b, &s, Event { class, on: svc, at: t, signal, severity: severity(strength(score)), value: Some(score) }, [], &ev, &rule);
            };
            if errs >= 3 && rate >= d.trace_error_rate.max(3.0 * base_err) {
                emit(&mut b, "failed", evt::TRACE_FAILED, "TRACE_FAILED", 5.0 * rate / base_err.max(0.01), 0);
            }
            if n >= 5 && p95 >= d.trace_slow_factor * base_p95 {
                emit(&mut b, "slow", evt::TRACE_SLOW, "TRACE_SLOW", 5.0 * p95 / base_p95, 1);
            }
        }
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spike_in_minute_samples_is_an_episode() {
        let series: Vec<(i64, f64, usize)> =
            (0..60).map(|i| (i * 60_000, if (40..45).contains(&i) { 100.0 } else { 10.0 + (i % 3) as f64 }, i as usize)).collect();
        let found = episodes(&series, &Detect::default(), false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start_ms, 40 * 60_000);
    }

    #[test]
    fn a_parent_span_is_found_only_in_its_own_trace() {
        let spans = [
            r#"{"trace_id": "t1", "span_id": "p", "service": "checkout", "start_ms": 1795386000000}"#,
            r#"{"trace_id": "t1", "span_id": "c", "parent_span_id": "p", "service": "ledger-db", "start_ms": 1795386000000}"#,
            r#"{"trace_id": "t2", "span_id": "p", "service": "search-api", "start_ms": 1795386000000}"#,
            r#"{"span_id": "q", "service": "web", "start_ms": 1795386000000}"#,
            r#"{"span_id": "r", "parent_span_id": "q", "service": "pay", "start_ms": 1795386000000}"#,
        ];
        let path = std::env::temp_dir().join(format!("ograph-trace-test-{}.json", std::process::id()));
        std::fs::write(&path, spans.join("\n")).unwrap();
        let b = traces(&path, "t.json", &Detect::default()).unwrap();
        let name = |e: &EntityRef| match e {
            EntityRef::Provisional { name, .. } => name.clone(),
            EntityRef::Known(n) => n.to_string(),
        };
        let edges: Vec<(String, String)> = b
            .facts
            .iter()
            .filter(|f| f.predicate.as_str() == ops::DEPENDS_ON)
            .map(|f| (name(&f.subject), match &f.object { crate::model::Object::Entity(e) => name(e), o => format!("{o:?}") }))
            .collect();
        assert_eq!(edges, [("checkout".to_owned(), "ledger-db".to_owned()), ("web".to_owned(), "pay".to_owned())]);
    }
}
