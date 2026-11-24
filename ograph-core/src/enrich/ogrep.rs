//! Every call to ogrep, the code specialist: one runner and one typed reader per record type.
//! ogrep stays an external process called with `--json`; ograph keeps no copy of its logic,
//! and callers get typed values, never raw JSON.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Why a call gave no records.
#[derive(Debug)]
pub enum Failure {
    NotRunnable(std::io::Error),
    Failed { flag: &'static str, stderr: String },
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NotRunnable(e) => write!(f, "ogrep not runnable: {e}"),
            Failure::Failed { flag, stderr } => write!(f, "ogrep {flag} failed: {stderr}"),
        }
    }
}

fn run(dir: &Path, args: &[&str], stdin: Option<&str>) -> std::io::Result<Output> {
    let mut cmd = Command::new(std::env::var("OGRAPH_OGREP").unwrap_or_else(|_| "ogrep".into()));
    cmd.args(args).current_dir(dir);
    let Some(input) = stdin else { return cmd.output() };
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut pipe = child.stdin.take().expect("stdin is piped");
    // Written beside the read, so a trace larger than the pipe cannot block both ends. A
    // child that exits without reading it closes the pipe, which is not a failure.
    std::thread::scope(|s| {
        s.spawn(move || drop(pipe.write_all(input.as_bytes())));
        child.wait_with_output()
    })
}

/// `f` of every item, in order, on up to eight threads: each call is one short-lived process,
/// and a repository with fifty services would otherwise wait for fifty of them in turn.
pub fn each<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut out: Vec<(usize, R)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..items.len().min(8))
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(item) = items.get(i) else { return done };
                        done.push((i, f(item)));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("an ogrep call does not panic")).collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

/// Standard output of a call. ogrep exits 1 when it found nothing, which is empty output
/// unless `strict`.
fn stdout(dir: &Path, flag: &'static str, args: &[&str], stdin: Option<&str>, strict: bool) -> Result<String, Failure> {
    let out = run(dir, args, stdin).map_err(Failure::NotRunnable)?;
    if out.status.success() || (!strict && out.status.code() == Some(1)) {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(Failure::Failed { flag, stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned() })
}

/// The `data` of every line that is a record of type `kind`. Lines that are something else
/// are skipped: ogrep mixes record types and may add fields.
fn records<T: DeserializeOwned>(text: &str, kind: Option<&str>) -> Vec<T> {
    #[derive(Deserialize)]
    struct Line {
        #[serde(rename = "type")]
        kind: Option<String>,
        data: serde_json::Value,
    }
    text.lines()
        .filter_map(|l| serde_json::from_str::<Line>(l).ok())
        .filter(|l| kind.is_none() || l.kind.as_deref() == kind)
        .filter_map(|l| serde_json::from_value(l.data).ok())
        .collect()
}

/// Where the operations files state something.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Location {
    pub file: Option<String>,
    pub line: Option<u64>,
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:", self.file.as_deref().unwrap_or("?"))?;
        match self.line {
            Some(n) => write!(f, "{n}"),
            None => f.write_str("null"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub relation: String,
    pub provenance: String,
    pub evidence: Location,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub location: Location,
    pub edges: Vec<Edge>,
}

/// `ogrep --entities . --json`: what the operations files of a repository declare. A
/// repository that declares nothing is a failure, as it is for the catalog.
pub fn entities(dir: &Path) -> Result<Vec<Entity>, Failure> {
    #[derive(Deserialize)]
    struct Data {
        entity: Head,
        #[serde(default)]
        edges: Vec<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Head {
        id: String,
        kind: String,
        #[serde(default)]
        locations: Vec<Location>,
    }
    #[derive(Deserialize)]
    struct RawEdge {
        from: String,
        to: String,
        #[serde(default)]
        relation: String,
        provenance: Option<String>,
        #[serde(default)]
        evidence: Location,
    }
    let text = stdout(dir, "--entities", &["--entities", ".", "--json"], None, true)?;
    Ok(records::<Data>(&text, None)
        .into_iter()
        .map(|d| Entity {
            id: d.entity.id,
            kind: d.entity.kind,
            location: d.entity.locations.into_iter().next().unwrap_or_default(),
            edges: d
                .edges
                .into_iter()
                .filter_map(|e| serde_json::from_value::<RawEdge>(e).ok())
                .map(|e| Edge { from: e.from, to: e.to, relation: e.relation, provenance: e.provenance.unwrap_or_else(|| "declared".into()), evidence: e.evidence })
                .collect(),
        })
        .collect())
}

/// The ids of the services among [`entities`].
pub fn services(dir: &Path) -> Result<Vec<String>, Failure> {
    Ok(entities(dir)?.into_iter().filter(|e| e.kind == "service").map(|e| e.id).collect())
}

/// `ogrep --files .`: the files ogrep searches, relative to `dir`, honouring its ignore rules.
/// Nothing when ogrep cannot run.
pub fn files(dir: &Path) -> Vec<String> {
    let Ok(out) = run(dir, &["--files", "."], None) else { return vec![] };
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.strip_prefix("./").unwrap_or(l).to_owned()).collect()
}

/// A commit, with paths relative to its repository and its time in Unix seconds.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Commit {
    pub sha: String,
    #[serde(default)]
    pub subject: String,
    pub time: i64,
    #[serde(default)]
    pub files: Vec<String>,
}

/// `ogrep --timeline SERVICE --since SINCE . --json`: commits to one service, newest first.
/// ogrep also lists commits to the service's direct dependencies; those are left out.
pub fn timeline(dir: &Path, service: &str, since: &str) -> Result<Vec<Commit>, Failure> {
    #[derive(Deserialize)]
    struct Data {
        service: String,
        commit: Commit,
    }
    let text = stdout(dir, "--timeline", &["--timeline", service, "--since", since, ".", "--json"], None, false)?;
    Ok(records::<Data>(&text, Some("event")).into_iter().filter(|d| d.service == service).map(|d| d.commit).collect())
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Suspect {
    pub service: String,
    pub score: f64,
    pub distance: f64,
    #[serde(default)]
    pub explains: Vec<String>,
    #[serde(default)]
    pub changes: Vec<Commit>,
}

/// `ogrep --suspects A,B --since SINCE . --json`: likely causes for failing services from the
/// dependency graph and recent commits, best first.
pub fn suspects(dir: &Path, failing: &[String], since: &str) -> Result<Vec<Suspect>, Failure> {
    #[derive(Deserialize)]
    struct Data {
        suspect: Suspect,
    }
    let text = stdout(dir, "--suspects", &["--suspects", &failing.join(","), "--since", since, ".", "--json"], None, false)?;
    Ok(records::<Data>(&text, Some("suspect")).into_iter().map(|d| d.suspect).collect())
}

/// A line of code, with its path relative to the repository.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeRef {
    pub path: String,
    pub line: u64,
}

/// `ogrep --log-templates --template-to-code . LOGFILE --json`: for each group of lines of
/// `logfile` with one template that ogrep located, the 1-based number of the group's first
/// line and the statement that writes it.
pub fn template_to_code(dir: &Path, logfile: &Path) -> Result<Vec<(usize, CodeRef)>, Failure> {
    #[derive(Deserialize)]
    struct Data {
        code: Option<(String, u64, serde_json::Value)>,
        example: Example,
    }
    #[derive(Deserialize)]
    struct Example {
        line_number: usize,
    }
    let file = logfile.to_string_lossy();
    let text = stdout(dir, "--template-to-code", &["--log-templates", "--template-to-code", ".", &file, "--json"], None, false)?;
    Ok(records::<Data>(&text, Some("template")).into_iter().filter_map(|d| d.code.map(|(path, line, _)| (d.example.line_number, CodeRef { path, line }))).collect())
}

/// `ogrep --stack-trace - . --json`: the frames of `trace` found in the repository, in the
/// order the trace lists them.
pub fn stack_trace(dir: &Path, trace: &str) -> Result<Vec<CodeRef>, Failure> {
    #[derive(Deserialize)]
    struct Data {
        path: String,
        line_number: u64,
        #[serde(default)]
        index: u64,
    }
    let text = stdout(dir, "--stack-trace", &["--stack-trace", "-", ".", "--json"], Some(trace), false)?;
    let mut frames = records::<Data>(&text, Some("frame"));
    frames.sort_by_key(|f| f.index);
    Ok(frames.into_iter().map(|f| CodeRef { path: f.path, line: f.line_number }).collect())
}

/// A past incident write-up that shares words with a description.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Writeup {
    pub path: String,
    pub score: f64,
    #[serde(default)]
    pub terms: Vec<String>,
}

/// `ogrep --similar-incidents TEXT . --json`: write-ups under postmortem-like paths, best first.
pub fn similar_incidents(dir: &Path, text: &str) -> Result<Vec<Writeup>, Failure> {
    let out = stdout(dir, "--similar-incidents", &["--similar-incidents", text, ".", "--json"], None, false)?;
    Ok(records(&out, Some("incident")))
}

#[derive(Clone, Debug, PartialEq)]
pub struct RankedFile {
    pub path: String,
    pub score: f64,
    /// The first matching line ogrep printed for the file.
    pub line: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct About {
    pub files: Vec<RankedFile>,
    /// `confident` or `unsure`: whether the first file stands clear of the rest.
    pub confidence: Option<String>,
}

/// `ogrep --about TEXT --budget N --per-file 1 . --json`: the files a description is about,
/// best first.
pub fn about(dir: &Path, text: &str, budget: usize) -> Result<About, Failure> {
    #[derive(Deserialize)]
    struct Text {
        text: String,
    }
    #[derive(Deserialize)]
    struct Level {
        level: String,
    }
    #[derive(Deserialize)]
    struct Rank {
        path: Text,
        score: f64,
        confidence: Option<Level>,
    }
    #[derive(Deserialize)]
    struct Match {
        path: Text,
        line_number: u64,
    }
    let clean = |p: String| p.strip_prefix("./").map(str::to_owned).unwrap_or(p);
    let out = stdout(dir, "--about", &["--about", text, "--budget", &budget.to_string(), "--per-file", "1", ".", "--json"], None, false)?;
    let mut about = About::default();
    for (n, r) in records::<Rank>(&out, Some("rank")).into_iter().enumerate() {
        if n == 0 {
            about.confidence = r.confidence.map(|c| c.level);
        }
        about.files.push(RankedFile { path: clean(r.path.text), score: r.score, line: None });
    }
    for m in records::<Match>(&out, Some("match")) {
        let path = clean(m.path.text);
        if let Some(f) = about.files.iter_mut().find(|f| f.path == path && f.line.is_none()) {
            f.line = Some(m.line_number);
        }
    }
    Ok(about)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_of_other_types_and_broken_lines_are_skipped() {
        let out = "not json\n{\"type\":\"begin\",\"data\":{\"path\":\"x\",\"score\":9}}\n{\"type\":\"incident\",\"data\":{\"path\":\"docs/postmortems/a.md\",\"score\":2.5,\"terms\":[\"pool\"],\"new_field\":1}}\n";
        assert_eq!(records::<Writeup>(out, Some("incident")), [Writeup { path: "docs/postmortems/a.md".into(), score: 2.5, terms: vec!["pool".into()] }]);
    }

    #[test]
    fn a_location_prints_as_the_catalog_evidence_always_did() {
        assert_eq!(Location { file: Some("c.yaml".into()), line: Some(4) }.to_string(), "c.yaml:4");
        assert_eq!(Location::default().to_string(), "?:null");
    }
}
