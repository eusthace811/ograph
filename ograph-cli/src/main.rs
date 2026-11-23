//! `ograph`: an ontology-driven observability knowledge graph on the command line.
//! Exit codes follow ogrep: 0 found something, 1 found nothing, 2 error.

use clap::{Parser, Subcommand};
use ograph_core::correlate::{self, RankMode};
use ograph_core::query;
use ograph_core::sparql::{self, Answer};
use ograph_core::workspace::{self, Workspace};
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
    },
    /// Applications and business processes that depend on an application (Q2)
    Blast {
        app: String,
        #[arg(long, default_value_t = 8)]
        depth: usize,
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
    },
    /// Past incidents that resemble this one, with accepted causes and mitigations (Q3)
    Similar {
        incident: String,
        #[arg(long, default_value_t = 5)]
        top: usize,
    },
    /// News that mentions vendors our applications use (Q4)
    News {
        #[arg(long)]
        since: Option<String>,
    },
    /// Provenance of every fact about a node
    Why { node: String },
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
    },
    /// Write a self-contained HTML page over the graph (incidents, causes, timelines)
    Lab {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Store size per table and graph count
    Stats,
    /// Rewrite the store file without free pages
    Compact,
    /// Keep the store open and answer JSON-RPC over TCP (also MCP over stdio with --mcp)
    Serve {
        #[arg(long, default_value = "127.0.0.1:0")]
        addr: String,
        #[arg(long)]
        mcp: bool,
    },
}

fn duration_s(s: &str) -> anyhow::Result<i64> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num.parse()?;
    Ok(match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => anyhow::bail!("unknown unit in {s}"),
    })
}

fn print<T: serde::Serialize>(json: bool, v: &T, human: impl FnOnce(&T)) {
    if json {
        println!("{}", serde_json::to_string(v).unwrap());
    } else {
        human(v);
    }
}

fn main() -> ExitCode {
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
        println!("initialized {}", dir.display());
        return Ok(true);
    }
    let root = match cli.root {
        Some(r) => r,
        None => workspace::find(&cwd).ok_or_else(|| anyhow::anyhow!("no .ograph/ here or above; run `ograph init`"))?,
    };
    let ws = Workspace::open(&root)?;
    let json = cli.json;
    match cli.cmd {
        Cmd::Init => unreachable!(),
        Cmd::Harvest { only } => {
            let t = std::time::Instant::now();
            let outcomes = ws.harvest(only.as_deref(), |o, secs| {
                if !json {
                    let g = o.graph.trim_start_matches(ograph_core::vocab::GRAPH);
                    println!("{g}: {} proposed, {} accepted, {} quarantined, +{} -{} quads{}{} ({secs:.2} s)", o.proposed, o.accepted, o.quarantined.len(), o.added, o.removed,
                        if o.fuzzy_matches > 0 { format!(", {} fuzzy catalog matches", o.fuzzy_matches) } else { String::new() },
                        if o.degraded.is_empty() { String::new() } else { format!(", DEGRADED: {}", o.degraded.join("; ")) });
                }
            })?;
            if json {
                println!("{}", serde_json::to_string(&outcomes)?);
            } else {
                let q: usize = outcomes.iter().map(|o| o.quarantined.len()).sum();
                println!("harvested {} batches in {:.1} s; {q} facts quarantined (see `ograph sparql` on graph g:quarantine/...)", outcomes.len(), t.elapsed().as_secs_f64());
            }
            Ok(true)
        }
        Cmd::Before { incident, window, depth } => {
            let snap = ws.store.snapshot()?;
            let b = query::before(&snap, &incident, duration_s(&window)?, depth)?;
            let found = !b.events.is_empty();
            print(json, &b, |b| {
                println!("{} on {} at {}; scope: {}", b.incident, b.app, b.at, b.scope.iter().map(|(a, d)| format!("{a}({d})")).collect::<Vec<_>>().join(" "));
                for e in &b.events {
                    println!("  -{:>5.1} min  {:<16} {:<15} sev {:.2}  {}", e.minutes_before, e.app, e.signal, e.severity, e.event);
                }
            });
            Ok(found)
        }
        Cmd::Blast { app, depth } => {
            let snap = ws.store.snapshot()?;
            let b = query::blast(&snap, &app, depth)?;
            let found = !b.dependents.is_empty();
            print(json, &b, |b| {
                println!("{} is depended on by {} applications", b.app, b.dependents.len());
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
        Cmd::Chains { incident, rank, window, top, set } => {
            let snap = ws.store.snapshot()?;
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
                st.window_s = duration_s(&w)?;
            }
            if !set.is_empty() {
                let mut v = serde_json::to_value(&st)?;
                for kv in &set {
                    let (k, val) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--set wants key=value"))?;
                    v[k] = serde_json::from_str(val).unwrap_or_else(|_| serde_json::Value::String(val.to_owned()));
                }
                st = serde_json::from_value(v)?;
            }
            let i = query::lookup(&snap, &incident)?;
            let mut a = correlate::analyze(&snap, &ids, i, &st)?;
            a.apps.truncate(top);
            let found = !a.apps.is_empty();
            print(json, &a, |a| {
                println!("{} on {} at {}: {} candidate events", ograph_core::vocab::compact(&a.incident), a.app, ograph_core::term::format_millis(a.at_ms), a.candidates);
                for (k, r) in a.apps.iter().enumerate() {
                    println!("  {}. {:<22} score {:.3}", k + 1, r.app, r.score);
                    if let Some(c) = &r.chain {
                        let path: Vec<String> = std::iter::once(&c.root).chain(&c.links).map(|s| format!("{}:{}", s.app_name, s.kind)).collect();
                        println!("     chain {:.2}: {} -> incident", c.confidence, path.join(" -> "));
                    }
                }
            });
            Ok(found)
        }
        Cmd::Similar { incident, top } => {
            let snap = ws.store.snapshot()?;
            let v = query::similar(&snap, &incident, &ws.config.correlation, top)?;
            let found = !v.is_empty();
            print(json, &v, |v| {
                for s in v {
                    println!("  {:.3}  {} on {} at {}  causes: [{}]  mitigations: [{}]", s.similarity, s.incident, s.app, s.at, s.cause_apps.join(", "), s.mitigations.join("; "));
                }
            });
            Ok(found)
        }
        Cmd::News { since } => {
            let snap = ws.store.snapshot()?;
            let since = since.map(|s| duration_s(&s).map(|d| workspace::now_ms() - d * 1000)).transpose()?;
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
            let p = query::why(&snap, &node)?;
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
            });
            Ok(!p.facts.is_empty())
        }
        Cmd::Verdict { chain, decision, evidence, by } => {
            let o = ws.verdict(&chain, decision == "accept", &by, &evidence)?;
            print(json, &o, |o| println!("verdict recorded in {} ({} facts, {} quarantined)", o.graph, o.accepted, o.quarantined.len()));
            Ok(o.quarantined.is_empty())
        }
        Cmd::Sparql { query, limit } => {
            sparql::check_names(&query, &ws.ontology)?;
            let snap = ws.store.snapshot()?;
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
            print(json, &r, |r| {
                for v in r.iter().take(50) {
                    println!("{} {} {:?}: {}", v.component, v.focus, v.path, v.message);
                }
                println!("{} violations ({:.1} s)", r.len(), t.elapsed().as_secs_f64());
            });
            Ok(r.is_empty())
        }
        Cmd::Export { out } => {
            let snap = ws.store.snapshot()?;
            let path = out.unwrap_or_else(|| ws.dir.join("export.trig"));
            let n = ograph_core::export::trig(&snap, &path)?;
            println!("wrote {n} quads to {}", path.display());
            Ok(n > 0)
        }
        Cmd::Ontology { action, other, out } => {
            if action == "coverage" {
                let file = other.ok_or_else(|| anyhow::anyhow!("ontology coverage needs a file of questions, one per line"))?;
                let llm = ws.llm().ok_or_else(|| anyhow::anyhow!("ontology coverage needs a model: add [llm.extract] to .ograph/config.toml"))?;
                let questions: Vec<String> = std::fs::read_to_string(file)?.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect();
                let catalog = ograph_core::gateway::Catalog::from_snapshot(&ws.store.snapshot()?)?;
                let is_entity = |t: &str| matches!(catalog.resolve(ograph_core::vocab::ops::APPLICATION, t), ograph_core::gateway::Resolution::Exact(_));
                let report = ograph_core::coverage::report(&ws.ontology, &llm, &questions, is_entity);
                let path = out.unwrap_or_else(|| ws.dir.join("proposed.ttl"));
                std::fs::write(&path, ograph_core::coverage::proposals(&report))?;
                let gaps = report.iter().filter(|c| !c.gaps.is_empty()).count();
                print(json, &report, |r| {
                    for c in r {
                        let names: Vec<&str> = c.covered.iter().filter_map(|n| n.name.as_deref()).collect();
                        let missing: Vec<&str> = c.gaps.iter().map(|n| n.text.as_str()).collect();
                        println!("{} {}\n    uses {}{}", if missing.is_empty() { "covered" } else { "GAP    " }, c.question, names.join(" "), if missing.is_empty() { String::new() } else { format!("\n    missing: {}", missing.join("; ")) });
                        for n in &c.notes {
                            println!("    note: {n}");
                        }
                    }
                    println!("{} of {} questions need something the ontology lacks; proposals in {}", gaps, r.len(), path.display());
                });
                return Ok(gaps == 0);
            }
            if action == "check" {
                let p = ws.ontology.check();
                for x in &p {
                    println!("{x}");
                }
                println!("ontology v{}: {} classes, {} properties, {} closed vocabularies, {} shapes; {} problems", ws.ontology.version, ws.ontology.classes.len(), ws.ontology.properties.len(), ws.ontology.vocabularies.len(), ws.ontology.shapes.shapes.len(), p.len());
                Ok(p.is_empty())
            } else {
                let other = other.ok_or_else(|| anyhow::anyhow!("ontology diff needs the other ontology.ttl"))?;
                let newer = ograph_core::ontology::Ontology::load(&std::fs::read_to_string(other)?, ograph_core::ontology::DEFAULT_SHAPES)?;
                let d = ws.ontology.diff(&newer);
                for x in &d {
                    println!("{x}");
                }
                Ok(!d.iter().any(|x| x.starts_with("BREAKING")))
            }
        }
        Cmd::Lab { out } => {
            let snap = ws.store.snapshot()?;
            let path = out.unwrap_or_else(|| ws.dir.join("lab.html"));
            std::fs::write(&path, ograph_core::lab::page(&snap, &ws.config.correlation)?)?;
            println!("wrote {}", path.display());
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
            let mut store = ws.store;
            store.compact()?;
            println!("{:.1} MiB -> {:.1} MiB", before as f64 / 1048576.0, std::fs::metadata(path)?.len() as f64 / 1048576.0);
            Ok(true)
        }
        Cmd::Serve { addr, mcp } => {
            ograph_core::serve::run(ws, &addr, mcp)?;
            Ok(true)
        }
    }
}
