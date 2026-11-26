//! `ograph`: an ontology-driven observability knowledge graph on the command line.
//! Exit codes follow ogrep: 0 found something, 1 found nothing, 2 error.

use anyhow::Context;
use clap::{Parser, Subcommand};
use ograph_core::correlate::{self, RankMode};
use ograph_core::query;
use ograph_core::sparql::{self, Answer};
use ograph_core::workspace::{self, Workspace};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "ograph", version, about = "Ontology-driven observability knowledge graph")]
struct Cli {
    /// Directory holding .ograph/ (default: search upward from the current directory)
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// JSON output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create .ograph/ with config, ontology and shapes
    Init,
    /// Run the enrichers in layer order; each source file is one named graph
    Harvest {
        /// Only this source kind (catalog, logs, metrics, ..., or correlation)
        #[arg(long)]
        only: Option<String>,
    },
    /// Events before an incident on its application and its dependencies (Q1)
    Before {
        incident: String,
        #[arg(long, default_value = "10m")]
        window: String,
        #[arg(long, default_value_t = 4)]
        depth: usize,
        /// Answer from what the store held at this time: a dateTime with a zone, or how long
        /// ago (90m, 2d)
        #[arg(long)]
        known_at: Option<String>,
    },
    /// Applications and business processes that depend on an application (Q2)
    Blast {
        app: String,
        #[arg(long, default_value_t = 8)]
        depth: usize,
        /// Follow only dependencies that held at this time: an incident (its opening time), a
        /// dateTime with a zone, or how long ago (90m, 2d)
        #[arg(long)]
        at: Option<String>,
        /// Answer from what the store held at this time: a dateTime with a zone, or how long
        /// ago (90m, 2d)
        #[arg(long)]
        known_at: Option<String>,
    },
    /// Correlation chains and ranked root-cause candidates for an incident
    Chains {
        incident: String,
        #[arg(long)]
        rank: Option<String>,
        #[arg(long)]
        window: Option<String>,
        #[arg(long, default_value_t = 5)]
        top: usize,
        /// Override a correlation setting, e.g. --set prop_weights=[0.5,0.3,0.1,0.1]
        #[arg(long = "set")]
        set: Vec<String>,
        /// Answer from what the store held at this time: a dateTime with a zone, or how long
        /// ago (90m, 2d)
        #[arg(long)]
        known_at: Option<String>,
        /// Also rank as the store was at this time, and name the changed source files that
        /// move the ranking when each is tested alone
        #[arg(long, conflicts_with = "known_at")]
        since: Option<String>,
    },
    /// Past incidents that resemble this one, with accepted causes and mitigations (Q3)
    Similar {
        incident: String,
        #[arg(long, default_value_t = 5)]
        top: usize,
    },
    /// Where an incident points in the code: lines, commits, files and past write-ups
    Code {
        incident: String,
        /// How many files and write-ups to list
        #[arg(long, default_value_t = 5)]
        top: usize,
    },
    /// Likely causes from the dependency graph and recent commits, when telemetry says little
    Suspects {
        /// An incident, or failing application names separated by commas
        target: String,
        /// How far back commits count, as ogrep reads it: 24h, 3d or a date (default: the day
        /// before the incident, or 24h for application names)
        #[arg(long)]
        since: Option<String>,
    },
    /// News that mentions vendors our applications use (Q4)
    News {
        /// Only news since then: a dateTime with a zone, or how long ago (90m, 7d)
        #[arg(long)]
        since: Option<String>,
    },
    /// Provenance of every fact about a node; for a chain, also every verdict on it
    Why { node: String },
    /// What depends on a source file, directory or record (`FILE#n`): its facts, the events
    /// and incidents only it states, the chains that would change without it, their verdicts,
    /// and its quarantined proposals
    Uses { source: String },
    /// Correlate every judged incident again and list the verdicts the result would contradict;
    /// exits 1 when any would flip, so it can gate a config.toml change
    Replay {
        /// Override a correlation setting, e.g. --set window_s=600 --set rank=propagation
        #[arg(long = "set")]
        set: Vec<String>,
    },
    /// Application names, aliases or IRIs (at most 5) to catalog applications, by the harvest's
    /// matching rules
    Resolve {
        #[arg(required = true)]
        mentions: Vec<String>,
    },
    /// Check the chain ids, incident ids, application names and IRIs an answer cites
    #[command(name = "cite-check")]
    CiteCheck {
        /// The answer's file, or - for standard input
        file: PathBuf,
    },
    /// Source files changed since the last full harvest, and incidents with chains but no
    /// verdict; prints nothing when nothing is due
    Status,
    /// Accept or reject a chain, with evidence
    Verdict {
        chain: String,
        #[arg(value_parser = ["accept", "reject"])]
        decision: String,
        #[arg(long)]
        evidence: Vec<String>,
        #[arg(long, default_value = "cli")]
        by: String,
    },
    /// Run a SPARQL query (default graph is the union of all harvests)
    Sparql {
        query: String,
        #[arg(long)]
        limit: Option<usize>,
        /// Answer from what the store held at this time: a dateTime with a zone, or how long
        /// ago (90m, 2d)
        #[arg(long)]
        known_at: Option<String>,
    },
    /// What the sources state differently now than at a time: facts added, removed and changed
    Changes {
        /// A dateTime with a zone, or how long ago (90m, 2d)
        #[arg(long)]
        since: String,
    },
    /// Full SHACL audit of the store
    Validate,
    /// Write every graph as TriG (the per-harvest archive)
    Export {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Ontology as code: check drift against the code, diff two versions, or report which parts
    /// of a file of questions (one per line) the ontology cannot represent
    Ontology {
        #[arg(value_parser = ["check", "diff", "coverage"])]
        action: String,
        other: Option<PathBuf>,
        /// coverage: where to write the proposed Turtle (default .ograph/proposed.ttl)
        #[arg(long)]
        out: Option<PathBuf>,
        /// diff: the newer shapes.ttl (default the workspace's)
        #[arg(long)]
        shapes: Option<PathBuf>,
    },
    /// Write a self-contained HTML page over the graph (incidents, causes, timelines)
    Lab {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Store size per table and graph count
    Stats,
    /// Drop history older than history_days, then rewrite the store file without free pages
    Compact,
    /// Keep the store open and answer JSON-RPC over TCP (also MCP over stdio with --mcp)
    Serve {
        #[arg(long, default_value = "127.0.0.1:0")]
        addr: String,
        #[arg(long)]
        mcp: bool,
    },
}

fn print<T: serde::Serialize>(json: bool, v: &T, human: impl FnOnce(&T)) {
    if json {
        println!("{}", serde_json::to_string(v).unwrap());
    } else {
        human(v);
    }
}

fn print_shift(s: &query::RankShift) {
    let list = |r: &[query::RankedApp]| if r.is_empty() { "nothing ranked".to_owned() } else { r.iter().enumerate().map(|(k, a)| format!("{}. {} {:.3}", k + 1, a.app, a.score)).collect::<Vec<_>>().join("  ") };
    let moved = s.then.iter().map(|a| &a.app).ne(s.now.iter().map(|a| &a.app));
    println!("since {}: {}; source files changed: {}", s.since, if moved { "the ranking moved" } else { "the ranking is the same" }, s.changed_graphs);
    if !moved {
        return;
    }
    println!("  then  {}\n  now   {}", list(&s.then), list(&s.now));
    if s.causes.is_empty() {
        println!("  no changed file moves it when tested alone; the changes move it together");
    }
    for c in &s.causes {
        let why = match (c.needed, c.enough) {
            (true, true) => "taking it back alone changes the ranking now, and it alone changes the ranking then",
            (true, false) => "taking it back alone changes the ranking now",
            _ => "it alone changes the ranking then",
        };
        println!("  {} [{}]: {why}", c.graph, c.source.as_deref().unwrap_or("?"));
    }
}

/// Why ogrep gave less than it could, on standard error so the answer stays what it was.
fn warn(degraded: &[String]) {
    for d in degraded {
        eprintln!("ograph: DEGRADED: {d}");
    }
}

/// The store now, or as it was at `known_at`, with what that view covers on standard error so
/// the answer on standard output is the one the command gave at the time.
fn view(ws: &Workspace, known_at: Option<&str>) -> anyhow::Result<ograph_core::store::Snapshot> {
    let Some(when) = known_at else { return Ok(ws.store.snapshot()?) };
    let (snap, note) = ws.known_at(when)?;
    eprintln!("ograph: {note}");
    Ok(snap)
}

/// Why a link scored what it did, for `ograph chains`.
fn link_basis(l: &correlate::LinkScore) -> String {
    let edge = match l.edge {
        correlate::Edge::SameApp => "same application",
        correlate::Edge::Catalog => "catalog dependency",
        correlate::Edge::Trace => "dependency seen only in traces",
        correlate::Edge::Other => "dependency from another source",
        correlate::Edge::Indirect => "no direct dependency",
    };
    if l.known_pair {
        format!("known pair, {edge}")
    } else {
        format!("time {:.2}, severity {:.2}, {edge}", l.time, l.severity)
    }
}

fn print_withheld(withheld: &[correlate::Withheld]) {
    for w in withheld {
        println!("  not followed: {} -> {}, added to the repository at {}, after the incident opened", w.caller, w.callee, w.valid_from);
    }
}

fn print_suspects(suspects: &[query::Suspect]) {
    for s in suspects {
        println!("  {}. {:<22} score {:.2}  distance {}  explains {}", s.rank, s.app, s.score, s.distance, s.explains.join(", "));
        for c in &s.changes {
            println!("     {:.12}  {}  {}  ({})", c.revision, c.at, c.summary, c.files.join(", "));
        }
    }
}

/// `st` with each `--set key=value` applied; a value that is not JSON is a string.
fn with_sets(st: &correlate::Settings, set: &[String]) -> anyhow::Result<correlate::Settings> {
    let mut v = serde_json::to_value(st)?;
    for kv in set {
        let (k, val) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--set wants key=value"))?;
        anyhow::ensure!(v.get(k).is_some(), "--set: no setting {k}");
        v[k] = serde_json::from_str(val).unwrap_or_else(|_| serde_json::Value::String(val.to_owned()));
    }
    let st: correlate::Settings = serde_json::from_value(v)?;
    st.check()?;
    Ok(st)
}

fn main() -> ExitCode {
    // `ograph ... | head`: a reader that stops early is no error, but println! panics on the
    // closed pipe. End quietly instead. Harvest writes its progress with writeln and goes on.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let p = info.payload();
        if p.downcast_ref::<String>().map(String::as_str).or_else(|| p.downcast_ref::<&str>().copied()).is_some_and(|m| m.starts_with("failed printing to stdout") && m.contains("Broken pipe")) {
            std::process::exit(0);
        }
        report(info)
    }));
    let cli = Cli::parse();
    match run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("ograph: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<bool> {
    let cwd = std::env::current_dir()?;
    if let Cmd::Init = cli.cmd {
        let root = cli.root.unwrap_or(cwd);
        let dir = Workspace::init(&root)?;
        print(cli.json, &dir, |d| println!("initialized {}", d.display()));
        return Ok(true);
    }
    let root = match cli.root {
        Some(r) => r,
        None => workspace::find(&cwd).ok_or_else(|| anyhow::anyhow!("no .ograph/ here or above; run `ograph init`"))?,
    };
    // Only these write the store; every other command opens it without marking the file.
    let writes = matches!(cli.cmd, Cmd::Harvest { .. } | Cmd::Verdict { .. } | Cmd::Compact | Cmd::Serve { .. });
    let ws = if writes { Workspace::open(&root)? } else { Workspace::open_reading(&root)? };
    let json = cli.json;
    match cli.cmd {
        Cmd::Init => unreachable!(),
        Cmd::Harvest { only } => {
            let t = std::time::Instant::now();
            let outcomes = ws.harvest(only.as_deref(), |o, secs| {
                if !json {
                    let g = o.graph.trim_start_matches(ograph_core::vocab::GRAPH);
                    // A closed pipe (`| head`) must not stop the harvest before it prunes and flushes.
                    let _ = writeln!(std::io::stdout(), "{g}: {} proposed, {} accepted, {} quarantined, +{} -{} quads{}{} ({secs:.2} s)", o.proposed, o.accepted, o.quarantined.len(), o.added, o.removed,
                        if o.fuzzy_matches > 0 { format!(", {} fuzzy catalog matches", o.fuzzy_matches) } else { String::new() },
                        if o.degraded.is_empty() { String::new() } else { format!(", DEGRADED: {}", o.degraded.join("; ")) });
                }
            })?;
            if json {
                println!("{}", serde_json::to_string(&outcomes)?);
            } else {
                let q: usize = outcomes.iter().map(|o| o.quarantined.len()).sum();
                let _ = writeln!(std::io::stdout(), "harvested {} batches in {:.1} s; {q} facts quarantined (see `ograph sparql` on graph g:quarantine/...)", outcomes.len(), t.elapsed().as_secs_f64());
            }
            Ok(true)
        }
        Cmd::Before { incident, window, depth, known_at } => {
            let snap = view(&ws, known_at.as_deref())?;
            let b = query::before(&snap, &incident, query::duration_s(&window)?, depth, ws.config.correlation.window_s)?;
            let found = !b.events.is_empty();
            print(json, &b, |b| {
                println!("{} on {} at {}; scope: {}", b.incident, b.app, b.at, b.scope.iter().map(|(a, d)| format!("{a}({d})")).collect::<Vec<_>>().join(" "));
                for e in &b.events {
                    println!("  -{:>5.1} min  {:<16} {:<15} sev {:.2}  {}", e.minutes_before, e.app, e.signal, e.severity, e.event);
                    if let Some(c) = &e.chronic {
                        println!("                 chronic: {c}");
                    }
                }
                print_withheld(&b.withheld);
            });
            Ok(found)
        }
        Cmd::Blast { app, depth, at, known_at } => {
            let snap = view(&ws, known_at.as_deref())?;
            let at = at.map(|w| query::time_of(&snap, &w)).transpose()?;
            let b = query::blast(&snap, &app, depth, at)?;
            let found = !b.dependents.is_empty() || !b.processes.is_empty();
            print(json, &b, |b| {
                let when = b.at.as_ref().map_or(String::new(), |t| format!(" at {t}"));
                println!("{} is depended on by {} applications{when}", b.app, b.dependents.len());
                for (a, d) in &b.dependents {
                    println!("  {d}  {a}");
                }
                for (p, apps) in &b.processes {
                    println!("  process {p}: via {}", apps.join(", "));
                }
                if !b.open_incidents.is_empty() {
                    println!("  open incidents: {}", b.open_incidents.join(", "));
                }
            });
            Ok(found)
        }
        Cmd::Chains { incident, rank, window, top, set, known_at, since } => {
            let snap = view(&ws, known_at.as_deref())?;
            let ids = correlate::Ids::new(&snap)?;
            let mut st = ws.config.correlation.clone();
            if let Some(r) = rank {
                st.rank = match r.as_str() {
                    "chain" => RankMode::Chain,
                    "propagation" => RankMode::Propagation,
                    _ => anyhow::bail!("--rank is chain or propagation"),
                };
            }
            if let Some(w) = window {
                st.window_s = query::duration_s(&w)?;
            }
            let st = with_sets(&st, &set)?;
            let i = query::lookup(&snap, &incident)?;
            let mut a = query::chains(&snap, &ids, i, &st, &mut Default::default())?;
            let (suspects, degraded) = ws.fallback_suspects(&snap, &a, &incident, &st);
            a.apps.truncate(top);
            let shift = match &since {
                Some(when) => Some(query::rank_shift(&snap, ws.history_time(&snap, when)?.0, &incident, &st, top)?),
                None => None,
            };
            let found = !a.apps.is_empty() || !suspects.is_empty();
            let human = |a: &correlate::Analysis| {
                println!("{} on {} at {}: {} candidate events", ograph_core::vocab::compact(&a.incident), a.app, ograph_core::term::format_millis(a.at_ms), a.candidates);
                for (k, r) in a.apps.iter().enumerate() {
                    println!("  {}. {:<22} score {:.3}", k + 1, r.app, r.score);
                    if !r.explains.is_empty() {
                        println!("     explains {}: its anomaly came first and was still under way when theirs began, so it ranks above them", r.explains.join(", "));
                    }
                    if let Some(p) = &r.precedent {
                        let gain = if st.precedent_weight == 0.0 { String::new() } else { format!(", score {:+.3}", st.precedent_weight * p.similarity) };
                        println!("     {}:{} was accepted as the cause of {} (similarity {:.3}{gain}): {}", p.app, p.signal, p.incident, p.similarity, p.evidence.join("; "));
                    }
                    if let Some(c) = &r.chain {
                        let path: Vec<String> = c.events().map(|s| format!("{}:{}", s.app_name, s.kind)).collect();
                        println!("     {} {:.2}: {} -> incident", ograph_core::vocab::compact(&c.id), c.confidence, path.join(" -> "));
                        for l in &c.scores {
                            println!("       {:.2}  {} -> {}  ({})", l.score, l.from, l.to, link_basis(l));
                        }
                        for sig in c.events() {
                            if let Some(ch) = &sig.chronic {
                                println!("       chronic {}:{}: {ch}", sig.app_name, sig.kind);
                            }
                        }
                        if c.scores.len() > 1 {
                            println!("       weakest link: {} -> {} at {:.2}", c.weakest.from, c.weakest.to, c.weakest.score);
                        }
                    }
                }
                print_withheld(&a.withheld);
            };
            warn(&degraded);
            print(json, &query::Chains { analysis: a, suspects, shift }, |c| {
                human(&c.analysis);
                if !c.suspects.is_empty() {
                    println!("no application ranked from telemetry; these come from the dependency graph and recent commits, not from telemetry:");
                    print_suspects(&c.suspects);
                }
                if let Some(s) = &c.shift {
                    print_shift(s);
                }
            });
            Ok(found)
        }
        Cmd::Suspects { target, since } => {
            let snap = ws.store.snapshot()?;
            let (suspects, degraded) = ws.suspects(&snap, &target, since.as_deref(), &ws.config.correlation)?;
            warn(&degraded);
            print(json, &suspects, |s| print_suspects(s));
            Ok(!suspects.is_empty())
        }
        Cmd::Similar { incident, top } => {
            let snap = ws.store.snapshot()?;
            let v = query::similar(&snap, &incident, &ws.config.correlation, top, &mut Default::default())?;
            let found = !v.is_empty();
            print(json, &v, |v| {
                for s in v {
                    println!("  {:.3}  {} on {} at {}  causes: [{}]  mitigations: [{}]", s.similarity, s.incident, s.app, s.at, s.cause_apps.join(", "), s.mitigations.join("; "));
                    for r in &s.ruled_out {
                        println!("         ruled out: {}:{} ({})", r.app, r.signal, r.evidence.join("; "));
                    }
                }
            });
            Ok(found)
        }
        Cmd::Code { incident, top } => {
            let snap = ws.store.snapshot()?;
            let c = ws.code(&snap, &incident, top)?;
            let found = !(c.locations.is_empty() && c.commits.is_empty() && c.deployments.is_empty() && c.files.is_empty() && c.writeups.is_empty());
            print(json, &c, |c| {
                println!("{} on {} at {}", c.incident, c.app, c.at);
                if !c.locations.is_empty() {
                    println!("lines behind the events before it:");
                }
                for l in &c.locations {
                    println!("  {:<16} {:<15} {}  {}", l.app, l.signal, l.location, l.summary.as_deref().unwrap_or(""));
                    if let Some(i) = &l.introduced {
                        println!("  {:<16} {:<15} first added {:.1} days before by {:.12}  {}", "", "", i.days_before, i.revision, i.summary);
                    }
                }
                if !c.commits.is_empty() {
                    println!("commits before it:");
                }
                for k in &c.commits {
                    let touches = if k.touches.is_empty() { String::new() } else { format!("  changed {}", k.touches.join(", ")) };
                    println!("  -{:>5.1} min  {:<16} {:.12}  {}{touches}", k.minutes_before, k.app, k.revision.as_deref().unwrap_or("?"), k.summary.as_deref().unwrap_or(""));
                }
                if !c.deployments.is_empty() {
                    println!("deployments before it:");
                }
                for d in &c.deployments {
                    let touches = if d.touches.is_empty() { String::new() } else { format!("  changed {}", d.touches.join(", ")) };
                    println!("  -{:>5.1} min  {:<16} {}  {} files since the version before{touches}", d.minutes_before, d.app, d.version.as_deref().unwrap_or("?"), d.files.len());
                }
                if !c.files.is_empty() {
                    println!("files its description is about ({}):", c.files_confidence.as_deref().unwrap_or("unranked"));
                }
                for f in &c.files {
                    println!("  {:>6.2}  {}{}", f.score, f.path, f.line.map(|n| format!(":{n}")).unwrap_or_default());
                }
                if !c.writeups.is_empty() {
                    println!("past write-ups:");
                }
                for w in &c.writeups {
                    println!("  {:>6.2}  {}  ({})", w.score, w.path, w.terms.join(", "));
                }
                for d in &c.degraded {
                    println!("DEGRADED: {d}");
                }
            });
            Ok(found)
        }
        Cmd::News { since } => {
            let snap = ws.store.snapshot()?;
            let since = since.as_deref().map(workspace::when_ms).transpose()?;
            let v = query::news(&snap, since)?;
            let found = !v.is_empty();
            print(json, &v, |v| {
                for n in v {
                    println!("  {}  {}  vendor {} -> apps {} -> processes {}", n.at, n.summary, n.vendor, n.apps.join(","), n.processes.join(","));
                }
            });
            Ok(found)
        }
        Cmd::Why { node } => {
            let snap = ws.store.snapshot()?;
            let p = query::why(&snap, &node, Some(ws.config.correlation.window_s))?;
            print(json, &p, |p| {
                println!("{}", p.node);
                for f in &p.facts {
                    let g: Vec<String> = f.graphs.iter().map(|g| format!("{} [{} via {} at {}]", g.graph, g.source.as_deref().unwrap_or("?"), g.extractor.as_deref().unwrap_or("?"), g.harvested_at.as_deref().unwrap_or("?"))).collect();
                    println!("  {} {}\n      from {}", f.predicate, f.object, g.join(", "));
                    if let Some((m, h, c)) = &f.model {
                        println!("      proposed by model {m} (prompt {h}, confidence {c})");
                    }
                }
                for (fact, reason) in &p.quarantined {
                    println!("  QUARANTINED {fact}: {reason}");
                }
                if let Some(c) = &p.chronic {
                    println!("  chronic: {c}");
                }
                if !p.verdicts.is_empty() {
                    println!("  verdicts, oldest first:");
                }
                for v in &p.verdicts {
                    let rank = v.rank.as_deref().map(|r| format!(" at rank {r}")).unwrap_or_default();
                    let now = if v.superseded_by.is_some() { "superseded" } else { "current" };
                    println!("    {} {:<8} by {} at {}{rank} ({now}): {}", v.verdict, v.state, v.by, v.at, v.evidence.join("; "));
                }
            });
            Ok(!p.facts.is_empty())
        }
        Cmd::Uses { source } => {
            let u = query::uses(&ws.store.snapshot()?, &ws.ontology, &ws.config.correlation, &query::source_key(&ws.root, &cwd, &source))?;
            for w in &u.warnings {
                eprintln!("warning: {w}");
            }
            print(json, &u, |u| {
                let only = u.facts.iter().filter(|f| f.also_in.is_empty()).count();
                println!("{}: {} facts ({only} stated nowhere else), {} events and incidents, {} chains change, {} verdicts, {} quarantined", u.source, u.facts.len(), u.events.len(), u.chains.len(), u.verdicts.len(), u.quarantined.len());
                if let Some(n) = &u.naming {
                    println!("note: this is a {n} source; every other file's application names are checked against it, so without it facts elsewhere can change too");
                }
                for g in &u.graphs {
                    println!("  in {g}: {} facts", u.facts.iter().filter(|f| &f.graph == g).count());
                }
                if !u.shared.is_empty() {
                    println!("  also stated by other files, so they stay: {}", u.shared.join(", "));
                }
                for e in &u.events {
                    println!("  event {}  {:<16} {:<15} {}  ({})", e.at.as_deref().unwrap_or("?"), e.app.as_deref().unwrap_or("?"), e.signal, e.node, e.record.as_deref().unwrap_or("?"));
                }
                for c in &u.chains {
                    let rank = |r: Option<usize>| r.map_or("-".to_owned(), |r| r.to_string());
                    let uses = if c.uses.is_empty() { String::new() } else { format!("  uses {}", c.uses.join(", ")) };
                    println!("  chain {:<8} {:<10} rank {} -> {}  {}{uses}", c.change, c.incident, rank(c.rank), rank(c.rank_after), c.chain);
                }
                for v in &u.verdicts {
                    println!("  verdict {} by {} at {} on {} ({})", v.state, v.by, v.at, v.chain, v.incident);
                }
                for q in &u.quarantined {
                    println!("  QUARANTINED {}: {}", q.fact, q.reason);
                }
            });
            Ok(!u.facts.is_empty() || u.naming.is_some())
        }
        Cmd::Replay { set } => {
            let snap = ws.store.snapshot()?;
            let st = with_sets(&ws.config.correlation, &set)?;
            let r = query::replay(&snap, &st, &ws.config.correlation)?;
            print(json, &r, |r| {
                let incidents: std::collections::BTreeSet<&str> = r.verdicts.iter().map(|v| v.incident.as_str()).collect();
                println!("replayed {} verdicts on {} incidents, settings {} (config.toml) against {}: {} would flip, {} would be fixed; {} contradicted by the replayed settings", r.verdicts.len(), incidents.len(), r.config_settings, r.settings, r.flips, r.fixed, r.contradicted);
                if r.stale_chains > 0 {
                    println!("note: {} stored chains were ranked under other settings than config.toml's; `ograph harvest` re-ranks them", r.stale_chains);
                }
                let rank = |k: Option<usize>| k.map_or("-".to_owned(), |k| k.to_string());
                for v in &r.verdicts {
                    let mark = match (v.flip, v.after.contradicts) {
                        (Some(f), _) => format!("FLIP {f}"),
                        (None, Some(c)) => format!("still {}", c.name()),
                        (None, None) if v.before.contradicts.is_some() => "fixed".to_owned(),
                        _ => continue,
                    };
                    println!("  {mark:<20} {:<8} {:<10} {:<22} app rank {} -> {}, chain rank {} -> {}  {}", v.state, v.incident, v.app, rank(v.before.app_rank), rank(v.after.app_rank), rank(v.before.chain_rank), rank(v.after.chain_rank), v.chain);
                }
            });
            Ok(r.flips == 0)
        }
        Cmd::Resolve { mentions } => {
            let snap = ws.store.snapshot()?;
            let r = query::resolve(&snap, &mentions)?;
            print(json, &r, |r| {
                for m in &r.results {
                    let target = m.app.as_deref().map(|a| format!(" -> {a}")).unwrap_or_default();
                    println!("{:<10} {}{target}  ({})", format!("{:?}", m.status).to_lowercase(), m.mention, m.rationale);
                    if m.app.is_none() {
                        for c in &m.candidates {
                            println!("{:<10}   {:.3}  {}", "", c.score, c.app);
                        }
                    }
                }
                if !r.skipped.is_empty() {
                    println!("skipped (at most {} per call): {}", query::MAX_MENTIONS, r.skipped.join(", "));
                }
            });
            Ok(r.results.iter().any(|m| m.status == query::MentionStatus::Resolved))
        }
        Cmd::CiteCheck { file } => {
            let text = if file.as_os_str() == "-" { std::io::read_to_string(std::io::stdin())? } else { std::fs::read_to_string(&file).with_context(|| file.display().to_string())? };
            let snap = ws.store.snapshot()?;
            let c = ograph_core::cite::check(&snap, &text)?;
            print(json, &c, |c| {
                for x in c {
                    println!("{:<8} {:<9} {}  (line {}: {})", format!("{:?}", x.status).to_lowercase(), format!("{:?}", x.kind).to_lowercase(), x.cited, x.line, x.detail);
                }
            });
            Ok(c.iter().all(|x| x.status == ograph_core::cite::Status::Present))
        }
        Cmd::Status => {
            let s = ograph_core::status::status(&ws)?;
            if json {
                println!("{}", serde_json::to_string(&s)?);
            } else if let Some(n) = s.note() {
                println!("{n}");
            }
            Ok(s.due())
        }
        Cmd::Verdict { chain, decision, evidence, by } => {
            let o = ws.verdict(&chain, decision == "accept", &by, &evidence)?;
            print(json, &o, |o| println!("verdict recorded in {} ({} facts, {} quarantined)", o.graph, o.accepted, o.quarantined.len()));
            Ok(o.quarantined.is_empty())
        }
        Cmd::Changes { since } => {
            let (then, note) = ws.known_at(&since)?;
            eprintln!("ograph: {note}");
            let v = query::changes(&then, &ws.store.snapshot()?)?;
            print(json, &v, |v| {
                for c in v {
                    let mark = match c.change {
                        "added" => '+',
                        "removed" => '-',
                        _ => '~',
                    };
                    let was = c.was.as_ref().map(|w| format!(" (was {w})")).unwrap_or_default();
                    println!("{mark} {} {} {}{was}\n      {} {} [{} via {}]", c.subject, c.predicate, c.object, c.at, c.graph, c.source.as_deref().unwrap_or("?"), c.extractor.as_deref().unwrap_or("?"));
                }
            });
            Ok(!v.is_empty())
        }
        Cmd::Sparql { query, limit, known_at } => {
            let query = sparql::check(&query, &ws.ontology)?;
            let snap = view(&ws, known_at.as_deref())?;
            let a = sparql::run(&snap, &query, limit)?;
            let found = match &a {
                Answer::Solutions { rows, .. } => !rows.is_empty(),
                Answer::Boolean { value } => *value,
                Answer::Graph { triples } => !triples.is_empty(),
            };
            print(json, &a, |a| match a {
                Answer::Solutions { variables, rows } => {
                    println!("{}", variables.join("\t"));
                    for r in rows {
                        println!("{}", r.iter().map(|c| c.clone().unwrap_or_default()).collect::<Vec<_>>().join("\t"));
                    }
                }
                Answer::Boolean { value } => println!("{value}"),
                Answer::Graph { triples } => {
                    for t in triples {
                        println!("{} {} {} .", t[0], t[1], t[2]);
                    }
                }
            });
            Ok(found)
        }
        Cmd::Validate => {
            let snap = ws.store.snapshot()?;
            let t = std::time::Instant::now();
            let r = ograph_core::gateway::audit(&snap, &ws.ontology);
            let shapes = ograph_core::gateway::shape_focus(&snap, &ws.ontology);
            let unmatched: Vec<&str> = shapes.iter().filter(|(_, n)| *n == 0).map(|(s, _)| s.as_str()).collect();
            let secs = t.elapsed().as_secs_f64();
            print(json, &serde_json::json!({"violations": r, "shapes": shapes, "unmatched": unmatched}), |_| {
                for v in r.iter().take(50) {
                    println!("{} {} {:?}: {}", v.component, v.focus, v.path, v.message);
                }
                println!("shapes.ttl focus nodes: {}", shapes.iter().map(|(s, n)| format!("{s} {n}")).collect::<Vec<_>>().join(", "));
                if !unmatched.is_empty() {
                    println!("warning: {} shapes matched nothing: {}", unmatched.len(), unmatched.join(", "));
                }
                println!("{} violations ({secs:.1} s)", r.len());
            });
            Ok(r.is_empty())
        }
        Cmd::Export { out } => {
            let snap = ws.store.snapshot()?;
            let path = out.unwrap_or_else(|| ws.dir.join("export.trig"));
            let n = ograph_core::export::trig(&snap, &path).with_context(|| path.display().to_string())?;
            print(json, &serde_json::json!({"quads": n, "path": path}), |_| println!("wrote {n} quads to {}", path.display()));
            Ok(n > 0)
        }
        Cmd::Ontology { action, other, out, shapes } => {
            if action == "coverage" {
                let file = other.ok_or_else(|| anyhow::anyhow!("ontology coverage needs a file of questions, one per line"))?;
                let llm = ws.llm().ok_or_else(|| anyhow::anyhow!("ontology coverage needs a model: add [llm.extract] to .ograph/config.toml"))?;
                let questions: Vec<String> = std::fs::read_to_string(&file).with_context(|| file.display().to_string())?.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect();
                let catalog = ograph_core::gateway::Catalog::from_snapshot(&ws.store.snapshot()?, None)?;
                let is_entity = |t: &str| matches!(catalog.resolve(ograph_core::vocab::ops::APPLICATION, t), ograph_core::gateway::Resolution::Exact(_));
                let mut report = ograph_core::coverage::report(&ws.ontology, &llm, &questions, is_entity);
                let declined = ws.dir.join("declined.ttl");
                if declined.exists() {
                    let d = ograph_core::coverage::declines(&std::fs::read_to_string(&declined).with_context(|| declined.display().to_string())?).map_err(|e| e.context(declined.display().to_string()))?;
                    ograph_core::coverage::decline(&mut report, &d);
                }
                let path = out.unwrap_or_else(|| ws.dir.join("proposed.ttl"));
                std::fs::write(&path, ograph_core::coverage::proposals(&report)).with_context(|| path.display().to_string())?;
                let gaps = report.iter().filter(|c| !c.gaps.is_empty() || c.failed.is_some()).count();
                print(json, &report, |r| {
                    for c in r {
                        let names: Vec<&str> = c.covered.iter().filter_map(|n| n.name.as_deref()).collect();
                        let missing: Vec<&str> = c.gaps.iter().map(|n| n.text.as_str()).collect();
                        let state = if c.failed.is_some() { "FAILED  " } else if !missing.is_empty() { "GAP     " } else if !c.declined.is_empty() { "DECLINED" } else { "covered " };
                        println!("{state} {}\n    uses {}{}", c.question, names.join(" "), if missing.is_empty() { String::new() } else { format!("\n    missing: {}", missing.join("; ")) });
                        for d in &c.declined {
                            println!("    declined by {}: \"{}\" ({}: {})", d.by, d.need.text, d.term, d.reason);
                        }
                        for n in c.notes.iter().chain(&c.failed) {
                            println!("    note: {n}");
                        }
                    }
                    let judged = r.iter().filter(|c| !c.declined.is_empty()).count();
                    println!("{} of {} questions need something the ontology lacks; {judged} have declined gaps; proposals in {}", gaps, r.len(), path.display());
                });
                return Ok(gaps == 0);
            }
            if action == "check" {
                let p = ws.ontology.check();
                let snap = ws.store.snapshot()?;
                let usage = ograph_core::ontology::Usage::from_snapshot(&snap)?;
                let w = ws.ontology.lints(Some(&usage));
                print(json, &serde_json::json!({"problems": p, "warnings": w}), |_| {
                    for x in &p {
                        println!("{x}");
                    }
                    for x in &w {
                        println!("warning: {x}");
                    }
                    println!("ontology v{}: {} classes, {} properties, {} closed vocabularies, {} shapes; {} problems, {} warnings", ws.ontology.version, ws.ontology.classes.len(), ws.ontology.properties.len(), ws.ontology.vocabularies.len(), ws.ontology.shapes.shapes.len(), p.len(), w.len());
                });
                Ok(p.is_empty())
            } else {
                anyhow::ensure!(other.is_some() || shapes.is_some(), "ontology diff needs the newer ontology.ttl, --shapes FILE, or both");
                let read = |p: Option<PathBuf>, name: &str, default: &str| -> anyhow::Result<String> {
                    match p {
                        Some(p) => std::fs::read_to_string(&p).with_context(|| p.display().to_string()),
                        None => workspace::read_setting(&ws.dir, name, default),
                    }
                };
                let newer = ograph_core::ontology::Ontology::load(&read(other, "ontology.ttl", ograph_core::ontology::DEFAULT_TTL)?, &read(shapes, "shapes.ttl", ograph_core::ontology::DEFAULT_SHAPES)?)?;
                let snap = ws.store.snapshot()?;
                let changes = ws.ontology.diff(&newer, &ograph_core::ontology::Usage::from_snapshot(&snap)?);
                let ograph_core::gateway::Impacts { quarantine, verdicts } = ograph_core::gateway::impact(&snap, &ws.ontology, &newer)?;
                let breaks = changes.iter().any(|x| x.starts_with("BREAKING")) || !quarantine.is_empty() || !verdicts.is_empty();
                print(json, &serde_json::json!({"changes": changes, "quarantine": quarantine, "verdicts": verdicts}), |_| {
                    for x in &changes {
                        println!("{x}");
                    }
                    let reasons = |q: &ograph_core::gateway::Impact| {
                        for (r, n) in q.reasons.iter().take(3) {
                            println!("    {n}  {r}");
                        }
                    };
                    for q in &quarantine {
                        println!("{} facts in {} would be quarantined", q.facts, q.graph);
                        reasons(q);
                    }
                    for q in &verdicts {
                        println!("{} verdict facts in {} no longer valid; new verdicts on this chain would be refused", q.facts, q.graph);
                        reasons(q);
                    }
                    let facts: usize = quarantine.iter().map(|q| q.facts).sum();
                    println!("{} changes, {} breaking; a harvest under the newer ontology would quarantine {facts} stored facts in {} graphs", changes.len(), changes.iter().filter(|x| x.starts_with("BREAKING")).count(), quarantine.len());
                    if !verdicts.is_empty() {
                        let facts: usize = verdicts.iter().map(|q| q.facts).sum();
                        println!("{facts} verdict facts on {} chains no longer valid; new verdicts on these chains would be refused", verdicts.len());
                    }
                });
                Ok(!breaks)
            }
        }
        Cmd::Lab { out } => {
            let snap = ws.store.snapshot()?;
            let path = out.unwrap_or_else(|| ws.dir.join("lab.html"));
            std::fs::write(&path, ograph_core::lab::page(&snap, &ws.config.correlation)?).with_context(|| path.display().to_string())?;
            print(json, &path, |p| println!("wrote {}", p.display()));
            Ok(true)
        }
        Cmd::Stats => {
            let snap = ws.store.snapshot()?;
            let size = std::fs::metadata(ws.store.path())?.len();
            let stats = snap.stats()?;
            let graphs = snap.graphs()?;
            print(json, &(size, &stats, graphs.len()), |_| {
                println!("store {} ({:.1} MiB), {} named graphs, {} distinct triples", ws.store.path().display(), size as f64 / 1048576.0, graphs.len(), snap.union_len().unwrap_or(0));
                for (n, len, stored, over) in &stats {
                    println!("  {n:<9} {len:>10} entries {:>8.1} MiB (+{:.1})", *stored as f64 / 1048576.0, *over as f64 / 1048576.0);
                }
            });
            Ok(true)
        }
        Cmd::Compact => {
            let before = std::fs::metadata(ws.store.path())?.len();
            let path = ws.store.path().to_owned();
            let days = ws.history_days();
            let _write = ws.store.write_lock()?;
            let pruned = ws.store.prune_history(workspace::now_ms() - i64::from(days) * 86_400_000)?;
            let mut store = ws.store;
            store.compact()?;
            let after = std::fs::metadata(path)?.len();
            print(json, &serde_json::json!({"bytes_before": before, "bytes_after": after, "history_rows_pruned": pruned, "history_days": days}), |_| {
                println!("{:.1} MiB -> {:.1} MiB; dropped {pruned} history rows older than {days} days", before as f64 / 1048576.0, after as f64 / 1048576.0)
            });
            Ok(true)
        }
        Cmd::Serve { addr, mcp } => {
            ograph_core::serve::run(ws, &addr, mcp)?;
            Ok(true)
        }
    }
}
