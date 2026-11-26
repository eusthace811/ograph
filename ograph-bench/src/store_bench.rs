//! Store gate: correctness against Oxigraph in memory, speed of W1-W3 through SPARQL and the
//! native paths, load rate, size, and harvest-replace cost.
//!
//! Usage: store_bench [--scale N] [--dir DIR] [--no-oxigraph] [--history-days D]

use ograph_core::store::{Snapshot, Store};
use ograph_core::term::{self, Id};
use ograph_core::vocab::{RDF_TYPE, evt, inc, ops};
use ograph_core::sparql;
use oxrdf::vocab::xsd;
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const START_MS: i64 = 1_790_812_800_000;
const DAY_MS: i64 = 86_400_000;
const WINDOW_MS: i64 = 600_000;
const RUNS: usize = 7;
const TEAMS: [&str; 10] = ["Bedrock", "Atlas", "Keystone", "Nimbus", "Orion", "Pulse", "Quarry", "Relay", "Summit", "Tundra"];
const REGIONS: [&str; 5] = ["us-east-1", "us-west-2", "eu-west-1", "ap-south-1", "sa-east-1"];
const EVENT_TYPES: [&str; 5] = [evt::DEPLOYMENT, evt::LOG_ERROR, evt::ALERT, evt::CONFIG_CHANGE, evt::METRIC_ANOMALY];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn iri(s: &str) -> NamedNode {
    NamedNode::new_unchecked(s)
}
fn id(local: &str) -> NamedNode {
    iri(&format!("{}{local}", ograph_core::vocab::DATA))
}
fn date_time(ms: i64) -> Term {
    Literal::new_typed_literal(term::format_millis(ms), xsd::DATE_TIME).into()
}

struct World {
    quads: Vec<Quad>,
    incidents: Vec<(NamedNode, u64)>,
    apps: Vec<NamedNode>,
    dependents: Vec<u32>,
}

fn generate(scale: u64, days: i64) -> World {
    let (n_apps, n_inc, n_ev) = (20_000 * scale, 100_000 * scale, 1_000_000 * scale);
    let mut rng = Rng(42);
    let mut quads = Vec::with_capacity((3_600_000 * scale) as usize);
    let catalog = iri("https://ograph.dev/graph/catalog");
    let incidents_g = iri("https://ograph.dev/graph/incidents");
    let day_graph = |ms: i64| iri(&format!("https://ograph.dev/graph/events/{}", (ms - START_MS) / DAY_MS));
    let mut add = |s: &NamedNode, p: &str, o: Term, g: &NamedNode| quads.push(Quad::new(s.clone(), iri(p), o, GraphName::NamedNode(g.clone())));
    for t in TEAMS {
        add(&id(&format!("team/{t}")), RDF_TYPE, iri(ops::TEAM).into(), &catalog);
    }
    let apps: Vec<NamedNode> = (0..n_apps).map(|i| id(&format!("app/{i}"))).collect();
    let mut deps = vec![Vec::new(); apps.len()];
    let mut dependents = vec![0u32; apps.len()];
    for (i, app) in apps.iter().enumerate() {
        add(app, RDF_TYPE, iri(ops::APPLICATION).into(), &catalog);
        add(app, ops::OWNED_BY, id(&format!("team/{}", TEAMS[rng.below(10) as usize])).into(), &catalog);
        add(app, ops::IN_REGION, id(&format!("region/{}", REGIONS[rng.below(5) as usize])).into(), &catalog);
        let k = (1 + rng.below(5) as usize).min(i);
        while deps[i].len() < k {
            let j = (i as f64 * rng.unit().powi(2)) as usize;
            if !deps[i].contains(&j) {
                deps[i].push(j);
                dependents[j] += 1;
                add(app, ops::DEPENDS_ON, apps[j].clone().into(), &catalog);
            }
        }
    }
    let span = days * DAY_MS;
    let mut n_events = 0u64;
    let event = |add: &mut dyn FnMut(&NamedNode, &str, Term, &NamedNode), rng: &mut Rng, n: u64, app: usize, ms: i64| {
        let e = id(&format!("ev/{n}"));
        let g = day_graph(ms);
        add(&e, RDF_TYPE, iri(EVENT_TYPES[rng.below(5) as usize]).into(), &g);
        add(&e, evt::ON, apps[app].clone().into(), &g);
        add(&e, evt::AT, date_time(ms), &g);
    };
    let mut incidents = Vec::new();
    for j in 0..n_inc {
        let node = id(&format!("inc/{j}"));
        let a = rng.below(n_apps) as usize;
        let ms = START_MS + 3_600_000 + rng.below((span - 3_600_000) as u64) as i64;
        add(&node, RDF_TYPE, iri(inc::INCIDENT).into(), &incidents_g);
        add(&node, evt::ON, apps[a].clone().into(), &incidents_g);
        add(&node, evt::SEVERITY, Literal::from(format!("SEV{}", 1 + rng.below(3))).into(), &incidents_g);
        add(&node, evt::AT, date_time(ms), &incidents_g);
        let precursors = rng.below(6);
        for _ in 0..precursors {
            let target = match deps[a].len() {
                0 => a,
                n if rng.below(2) == 0 => deps[a][rng.below(n as u64) as usize],
                _ => a,
            };
            let at = ms - rng.below(12 * 60_000) as i64;
            event(&mut add, &mut rng, n_events, target, at);
            n_events += 1;
        }
        incidents.push((node, precursors));
    }
    for n in n_events..n_ev {
        let (app, at) = (rng.below(n_apps) as usize, START_MS + rng.below(span as u64) as i64);
        event(&mut add, &mut rng, n, app, at);
    }
    World { quads, incidents, apps, dependents }
}

type Rows = Vec<String>;

fn canonical(mut rows: Vec<Vec<Option<Term>>>) -> Rows {
    let mut out: Rows = rows.drain(..).map(|r| r.iter().map(|t| t.as_ref().map_or("UNDEF".into(), Term::to_string)).collect::<Vec<_>>().join(" ")).collect();
    out.sort_unstable();
    out
}

fn ox_rows(store: &oxigraph::store::Store, q: &str) -> Rows {
    let query = sparql::parse(q).unwrap();
    let mut prepared = oxigraph::sparql::SparqlEvaluator::new().for_query(query);
    prepared.dataset_mut().set_default_graph_as_union();
    match prepared.on_store(store).execute().unwrap() {
        oxigraph::sparql::QueryResults::Solutions(s) => {
            let rows: Vec<Vec<Option<Term>>> = s.map(|s| s.unwrap().values().to_vec()).collect();
            // Oxigraph's union default graph keeps duplicates across graphs; ours deduplicates.
            let mut r = canonical(rows);
            r.dedup();
            r
        }
        _ => unreachable!(),
    }
}

struct Native<'a> {
    s: &'a Snapshot,
}
impl Native<'_> {
    fn id(&self, iri: &str) -> Id {
        self.s.iri(iri).unwrap().unwrap_or_else(|| panic!("{iri} not in store"))
    }
    fn rows(&self, ids: impl IntoIterator<Item = Vec<Id>>) -> Rows {
        let mut out: Rows = ids.into_iter().map(|r| r.iter().map(|&i| self.s.term(i).unwrap().to_string()).collect::<Vec<_>>().join(" ")).collect();
        out.sort_unstable();
        out
    }
    fn w1(&self) -> Rows {
        let team: HashSet<Id> = self.s.subjects(self.id(ops::OWNED_BY), self.id(&id("team/Bedrock").into_string())).unwrap().into_iter().collect();
        let (on, ty, incident) = (self.id(evt::ON), self.id(RDF_TYPE), self.id(inc::INCIDENT));
        let mut rows = Vec::new();
        for app in self.s.subjects(self.id(ops::IN_REGION), self.id(&id("region/us-east-1").into_string())).unwrap() {
            if !team.contains(&app) {
                continue;
            }
            for i in self.s.subjects(on, app).unwrap() {
                if self.s.contains([i, ty, incident]).unwrap() {
                    rows.push(vec![i, app]);
                }
            }
        }
        self.rows(rows)
    }
    fn w2(&self, incident: &NamedNode) -> Rows {
        let i = self.id(incident.as_str());
        let app = self.s.object(i, self.id(evt::ON)).unwrap().unwrap();
        let t0 = self.s.object(i, self.id(evt::AT)).unwrap().unwrap();
        let ms = term::millis(t0).unwrap();
        let mut scope: Vec<Id> = self.s.objects(app, self.id(ops::DEPENDS_ON)).unwrap();
        scope.push(app);
        scope.sort_unstable();
        scope.dedup();
        let mut hits = HashSet::new();
        for x in scope {
            for (_, e) in self.s.events_on(x, term::time_id(ms - WINDOW_MS), t0).unwrap() {
                hits.insert(e);
            }
        }
        self.rows(hits.into_iter().map(|e| vec![e]))
    }
    fn w3(&self, app: &NamedNode) -> Rows {
        let dep = self.id(ops::DEPENDS_ON);
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([self.id(app.as_str())]);
        while let Some(n) = queue.pop_front() {
            for x in self.s.subjects(dep, n).unwrap() {
                if seen.insert(x) {
                    queue.push_back(x);
                }
            }
        }
        self.rows(seen.into_iter().map(|x| vec![x]))
    }
}

fn w1_q() -> String {
    "SELECT ?inc ?app WHERE { ?inc a inc:Incident ; evt:on ?app . ?app ops:ownedBy id:team\\/Bedrock ; ops:inRegion id:region\\/us-east-1 . }".into()
}
fn w2_q(i: &NamedNode) -> String {
    format!("SELECT DISTINCT ?e WHERE {{ {i} evt:on ?a ; evt:at ?t0 . ?a ops:dependsOn? ?x . ?e evt:on ?x ; evt:at ?t . FILTER(?t >= ?t0 - \"PT10M\"^^xsd:dayTimeDuration && ?t <= ?t0) }}")
}
fn w3_q(a: &NamedNode) -> String {
    format!("SELECT DISTINCT ?x WHERE {{ ?x ops:dependsOn+ {a} }}")
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1e3)
}

fn median(mut f: impl FnMut() -> usize) -> (Duration, usize) {
    let rows = f();
    let mut t: Vec<Duration> = (0..RUNS)
        .map(|_| {
            let s = Instant::now();
            assert_eq!(f(), rows);
            s.elapsed()
        })
        .collect();
    t.sort();
    (t[RUNS / 2], rows)
}

fn pct(mut v: Vec<Duration>, p: f64) -> Duration {
    v.sort();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).map(|i| args[i + 1].clone());
    let scale: u64 = flag("--scale").map_or(1, |s| s.parse().unwrap());
    let days: i64 = flag("--history-days").map_or(30, |s| s.parse().unwrap());
    let dir = PathBuf::from(flag("--dir").unwrap_or_else(|| "target/bench".into()));
    let with_ox = !args.iter().any(|a| a == "--no-oxigraph");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("store-x{scale}-d{days}.redb"));
    let _ = std::fs::remove_file(&path);

    let t = Instant::now();
    let world = generate(scale, days);
    println!("generated {} quads in {} ms (scale {scale}, {days} days of history)", world.quads.len(), ms(t.elapsed()));

    let store = Store::open(&path)?;
    let t = Instant::now();
    let chunk_size: usize = flag("--chunk").map_or(4_000_000, |s| s.parse().unwrap());
    for chunk in world.quads.chunks(chunk_size) {
        store.transact(|tx| {
            let mut ids = Vec::with_capacity(chunk.len());
            for q in chunk {
                let GraphName::NamedNode(g) = &q.graph_name else { unreachable!() };
                ids.push([tx.intern(&q.subject.clone().into())?, tx.intern(&q.predicate.clone().into())?, tx.intern(&q.object)?, tx.intern(&g.clone().into())?]);
            }
            tx.insert(ids)
        })?;
    }
    let load = t.elapsed();
    let loose = std::fs::metadata(&path)?.len();
    let mut store = store;
    let t = Instant::now();
    store.compact()?;
    println!("compaction: {:.1} MiB -> {:.1} MiB in {} ms", loose as f64 / 1048576.0, std::fs::metadata(&path)?.len() as f64 / 1048576.0, ms(t.elapsed()));
    let size = std::fs::metadata(&path)?.len();
    drop(store);
    let t = Instant::now();
    let store = Store::open(&path)?;
    let snap = store.snapshot()?;
    let open = t.elapsed();
    println!("\n| load (s) | quads/s | on disk (MiB) | bytes/quad | open (ms) | union triples |\n|---|---|---|---|---|---|");
    println!("| {:.1} | {:.0} | {:.1} | {:.0} | {} | {} |", load.as_secs_f64(), world.quads.len() as f64 / load.as_secs_f64(), size as f64 / 1048576.0, size as f64 / world.quads.len() as f64, ms(open), snap.union_len()?);
    println!("\n| table | entries | stored MiB | overhead MiB |\n|---|---|---|---|");
    for (name, n, stored, meta) in snap.stats()? {
        println!("| {name} | {n} | {:.1} | {:.1} |", stored as f64 / 1048576.0, meta as f64 / 1048576.0);
    }

    let ox = with_ox.then(|| {
        let t = Instant::now();
        let s = oxigraph::store::Store::new().unwrap();
        let mut l = s.bulk_loader();
        l.load_quads(world.quads.iter().cloned()).unwrap();
        l.commit().unwrap();
        println!("\noxigraph in-memory load: {} ms", ms(t.elapsed()));
        s
    });

    let native = &Native { s: &snap };
    let apps = &world.apps;
    let ours = |q: &str| canonical(sparql::solutions(&snap, q).unwrap());
    let mut mismatches = 0;
    let mut check = |label: &str, q: &str, nat: Rows| {
        let mine = ours(q);
        if mine != nat {
            mismatches += 1;
            println!("MISMATCH {label}: sparql {} rows, native {} rows", mine.len(), nat.len());
        }
        if let Some(ox) = &ox {
            let theirs = ox_rows(ox, q);
            if theirs != mine {
                mismatches += 1;
                println!("MISMATCH {label}: oxigraph {} rows, ours {} rows", theirs.len(), mine.len());
            }
        }
        mine.len()
    };

    let mut rng = Rng(7);
    let mut picked = Vec::new();
    while picked.len() < 20 {
        let (node, pre) = &world.incidents[rng.below(world.incidents.len() as u64) as usize];
        if *pre > 0 && !picked.contains(&node) {
            picked.push(node);
        }
    }
    let hub = (0..world.apps.len()).max_by_key(|&i| world.dependents[i]).unwrap();
    let mut starts = vec![hub];
    while starts.len() < 20 {
        let i = rng.below(world.apps.len() as u64) as usize;
        if world.dependents[i] > 0 && !starts.contains(&i) {
            starts.push(i);
        }
    }

    check("W1", &w1_q(), native.w1());
    for i in &picked {
        check(i.as_str(), &w2_q(i), native.w2(i));
    }
    for &a in &starts {
        check(world.apps[a].as_str(), &w3_q(&world.apps[a]), native.w3(&world.apps[a]));
    }
    println!("\ncorrectness: {mismatches} mismatches over 41 queries{}", if with_ox { " (against Oxigraph and native)" } else { " (SPARQL against native)" });

    println!("\n| workload | rows | SPARQL p50 (ms) | SPARQL max (ms) | native p50 (ms) | native max (ms) | Oxigraph p50 (ms) |\n|---|---|---|---|---|---|---|");
    let row = |name: &str, cases: Vec<(String, Box<dyn Fn() -> Rows + '_>)>| {
        let (mut s, mut n, mut o, mut rows) = (vec![], vec![], vec![], 0);
        for (q, nat) in &cases {
            let (d, r) = median(|| ours(q).len());
            rows += r;
            s.push(d);
            n.push(median(|| nat().len()).0);
            if let Some(ox) = &ox {
                o.push(median(|| ox_rows(ox, q).len()).0);
            }
        }
        let ox_p50 = if o.is_empty() { "n/a".into() } else { ms(pct(o, 0.5)) };
        println!("| {name} | {rows} | {} | {} | {} | {} | {ox_p50} |", ms(pct(s.clone(), 0.5)), ms(pct(s, 1.0)), ms(pct(n.clone(), 0.5)), ms(pct(n, 1.0)));
    };
    row("W1 Bedrock incidents in us-east-1", vec![(w1_q(), Box::new(|| native.w1()))]);
    row("W2 10 min before incident (20)", picked.iter().map(|i| (w2_q(i), Box::new(move || native.w2(i)) as Box<dyn Fn() -> Rows>)).collect());
    row("W3 blast radius, hub app", vec![(w3_q(&world.apps[hub]), Box::new(|| native.w3(&world.apps[hub])))]);
    row("W3 blast radius (20 apps)", starts.iter().map(|&a| (w3_q(&world.apps[a]), Box::new(move || native.w3(&apps[a])) as Box<dyn Fn() -> Rows>)).collect());
    drop(snap);

    // Harvest replace: re-run one day's harvest unchanged, then with 10% of its events changed.
    let g = iri("https://ograph.dev/graph/events/3");
    let day: Vec<Quad> = world.quads.iter().filter(|q| q.graph_name == GraphName::NamedNode(g.clone())).cloned().collect();
    let replace = |quads: &[Quad]| -> anyhow::Result<(usize, usize, Duration)> {
        let t = Instant::now();
        let (r, a) = store.transact(|tx| {
            let gid = tx.intern(&g.clone().into())?;
            let mut ids = Vec::with_capacity(quads.len());
            for q in quads {
                ids.push([tx.intern(&q.subject.clone().into())?, tx.intern(&q.predicate.clone().into())?, tx.intern(&q.object)?]);
            }
            tx.replace_graph(gid, ids)
        })?;
        Ok((r, a, t.elapsed()))
    };
    let (r, a, d) = replace(&day)?;
    println!("\nre-harvest of one day graph ({} quads), unchanged: removed {r}, added {a} in {} ms", day.len(), ms(d));
    let changed: Vec<Quad> = day.iter().enumerate().map(|(i, q)| {
        if i % 30 < 3 && q.predicate.as_str() == evt::AT {
            let Term::Literal(l) = &q.object else { unreachable!() };
            let m = term::parse_millis(l.value()).unwrap() + 1000;
            Quad::new(q.subject.clone(), q.predicate.clone(), date_time(m), q.graph_name.clone())
        } else {
            q.clone()
        }
    }).collect();
    let (r, a, d) = replace(&changed)?;
    println!("re-harvest with changed timestamps: removed {r}, added {a} in {} ms", ms(d));
    let held = {
        let snap = store.snapshot()?;
        let native = Native { s: &snap };
        let mut held: Vec<Rows> = picked.iter().map(|i| native.w2(i)).collect();
        held.extend([native.w1(), native.w3(&world.apps[hub])]);
        held
    };
    std::thread::sleep(Duration::from_millis(5));
    let between = ograph_core::store::now_ms();
    std::thread::sleep(Duration::from_millis(5));
    let (r, a, d) = replace(&day)?;
    println!("re-harvest back to the original: removed {r}, added {a} in {} ms", ms(d));
    let t = Instant::now();
    let then = store.snapshot()?.known_at(between)?;
    let built = t.elapsed();
    let view = Native { s: &then };
    let mut queries: Vec<String> = picked.iter().map(|i| w2_q(i)).collect();
    queries.extend([w1_q(), w3_q(&world.apps[hub])]);
    let mut answers: Vec<Rows> = picked.iter().map(|i| view.w2(i)).collect();
    answers.extend([view.w1(), view.w3(&world.apps[hub])]);
    let stale = answers.iter().zip(&held).filter(|(a, b)| a != b).count()
        + queries.iter().zip(&held).filter(|(q, b)| canonical(sparql::solutions(&then, q).unwrap()) != **b).count();
    let (hidden, restored) = then.as_of_counts().unwrap();
    let table = |name: &str| then.stats().unwrap().into_iter().find(|t| t.0 == name).map(|t| (t.1, (t.2 + t.3) as f64 / 1024.0)).unwrap();
    let ((h_rows, h_kib), (a_rows, a_kib)) = (table("history"), table("added"));
    println!("\nhistory after the two replaces: {h_rows} rows in `history` ({h_kib:.0} KiB), {a_rows} in `added` ({a_kib:.0} KiB)");
    println!("view of the store between them: built in {} ms, hides {hidden} quads, restores {restored}; {stale} mismatches over {} answers (native and SPARQL on the view against the native answers saved then)", ms(built), 2 * held.len());
    println!("\n| workload on the view | native p50 (ms) | SPARQL p50 (ms) |\n|---|---|---|");
    for (name, nat, q) in [
        ("W1", Box::new(|| view.w1().len()) as Box<dyn Fn() -> usize>, w1_q()),
        ("W2, one incident", Box::new(|| view.w2(picked[0]).len()), w2_q(picked[0])),
        ("W3 blast radius, hub app", Box::new(|| view.w3(&world.apps[hub]).len()), w3_q(&world.apps[hub])),
    ] {
        println!("| {name} | {} | {} |", ms(median(nat).0), ms(median(|| sparql::solutions(&then, &q).unwrap().len()).0));
    }
    drop(then);
    if stale > 0 {
        std::process::exit(1);
    }
    let snap = store.snapshot()?;
    let native = Native { s: &snap };
    let again: usize = picked.iter().map(|i| native.w2(i).len()).sum();
    let sparql_rows: usize = picked.iter().map(|i| canonical(sparql::solutions(&snap, &w2_q(i)).unwrap()).len()).sum();
    println!("after the replaces: W2 native rows {again}, SPARQL rows {sparql_rows} (must equal the W2 rows above)");
    if mismatches > 0 {
        std::process::exit(1);
    }
    Ok(())
}
