//! Rust driver for Apache AGE (tokio-postgres) and Apache HugeGraph (reqwest, REST + Gremlin).
//! Checks every query's sorted rows against data/expected (redb-quadstore's native results)
//! and times 7 alternating warm runs per query.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

const RUNS: usize = 7;
const PG: &str = "host=127.0.0.1 port=55432 user=postgres password=pw dbname=obs";
const HG: &str = "http://127.0.0.1:18080";
const GRAPH: &str = "hugegraph";

struct Expected {
    w1: Vec<String>,
    w2: Vec<(u64, Vec<String>)>,
    w3: Vec<(u64, Vec<String>)>,
}

fn num(s: &str) -> u64 {
    s.rsplit('/').next().unwrap().parse().unwrap()
}

fn expected() -> Expected {
    let read = |f: &str| std::fs::read_to_string(format!("data/expected/{f}")).unwrap();
    let per = |f: &str| {
        read(f)
            .lines()
            .map(|l| {
                let (k, v) = l.split_once('\t').unwrap();
                (num(k), v.split_whitespace().map(String::from).collect())
            })
            .collect()
    };
    Expected { w1: read("w1.txt").lines().map(String::from).collect(), w2: per("w2.tsv"), w3: per("w3.tsv") }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

type Fut<'a> = Pin<Box<dyn Future<Output = Vec<String>> + 'a>>;

struct Measured {
    median: f64,
    max: f64,
    rows: usize,
}

/// One untimed correctness run per engine, then RUNS rounds alternating the engines.
async fn bench<'a>(label: &str, want: &[String], engines: &[(&str, &dyn Fn() -> Fut<'a>)]) -> Vec<Measured> {
    let mut want = want.to_vec();
    want.sort();
    for (name, f) in engines {
        let mut got = f().await;
        got.sort();
        if got != want {
            let missing: Vec<_> = want.iter().filter(|w| !got.contains(w)).take(3).collect();
            let extra: Vec<_> = got.iter().filter(|g| !want.contains(g)).take(3).collect();
            println!("MISMATCH {label} {name}: got {} rows, want {} (missing {missing:?}, extra {extra:?})", got.len(), want.len());
        }
    }
    let mut times = vec![Vec::new(); engines.len()];
    for _ in 0..RUNS {
        for (i, (_, f)) in engines.iter().enumerate() {
            let start = Instant::now();
            let rows = f().await;
            times[i].push(ms(start.elapsed()));
            assert_eq!(rows.len(), want.len(), "{label}: row count changed between runs");
        }
    }
    times
        .into_iter()
        .map(|mut t| {
            t.sort_by(f64::total_cmp);
            Measured { median: t[RUNS / 2], max: t[RUNS - 1], rows: want.len() }
        })
        .collect()
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn report(title: &str, names: &[&str], per: &[Vec<Measured>]) {
    println!("\n### {title}\n\n| engine | min of medians (ms) | p50 | p90 | max of medians | max of all runs |\n|---|---|---|---|---|---|");
    for (e, name) in names.iter().enumerate() {
        let mut med: Vec<f64> = per.iter().map(|m| m[e].median).collect();
        let worst = per.iter().map(|m| m[e].max).fold(0.0, f64::max);
        let (a, b, c, d) = (pct(&mut med, 0.0), pct(&mut med, 0.5), pct(&mut med, 0.9), pct(&mut med, 1.0));
        println!("| {name} | {a:.3} | {b:.3} | {c:.3} | {d:.3} | {worst:.3} |");
    }
}

// ---------------------------------------------------------------- AGE

async fn pg() -> Client {
    let (client, conn) = tokio_postgres::connect(PG, NoTls).await.unwrap();
    tokio::spawn(conn);
    client.batch_execute("LOAD 'age'; SET search_path = ag_catalog, \"$user\", public;").await.unwrap();
    client
}

async fn rows(c: &Client, sql: &str) -> Vec<Vec<String>> {
    c.simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("{e:?}\n{sql}"))
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).unwrap_or("").to_string()).collect()),
            _ => None,
        })
        .collect()
}

fn cypher(graph: &str, q: &str, cols: &[&str]) -> String {
    let decl: Vec<String> = cols.iter().map(|c| format!("{c} agtype")).collect();
    let sel: Vec<String> = cols.iter().map(|c| format!("{c}::text")).collect();
    format!("SELECT {} FROM cypher('{graph}', $$ {q} $$) AS ({})", sel.join(", "), decl.join(", "))
}

fn age_w1(g: &str) -> String {
    cypher(g, "MATCH (i:incident)-[:affects]->(a:app {team: 'Bedrock', region: 'us-east-1'}) RETURN i.id, a.id ORDER BY i.id", &["i", "a"])
}

fn age_w2_graph(inc: u64) -> String {
    cypher("obs", &format!(
        "MATCH (i:incident)-[:affects]->(a:app)-[:depends_on*0..1]->(x:app)<-[:about]-(e:event) \
         WHERE i.id = {inc} AND e.at >= i.opened_at - 600000 AND e.at <= i.opened_at RETURN DISTINCT e.id"), &["e"])
}

fn age_w2_hybrid(inc: u64) -> String {
    format!(
        "SELECT e.id::text FROM cypher('obs_h', $$ MATCH (i:incident)-[:affects]->(a:app)-[:depends_on*0..1]->(x:app) \
         WHERE i.id = {inc} RETURN DISTINCT x.id, i.opened_at $$) AS (x agtype, t0 agtype) \
         JOIN public.events e ON e.app_id = x::text::int AND e.at BETWEEN t0::text::bigint - 600000 AND t0::text::bigint"
    )
}

fn age_w2_graph_union(inc: u64) -> String {
    let w = format!("i.id = {inc} AND e.at >= i.opened_at - 600000 AND e.at <= i.opened_at RETURN e.id");
    cypher("obs", &format!(
        "MATCH (i:incident)-[:affects]->(x:app)<-[:about]-(e:event) WHERE {w} \
         UNION MATCH (i:incident)-[:affects]->(:app)-[:depends_on]->(x:app)<-[:about]-(e:event) WHERE {w}"), &["e"])
}

fn age_w2_hybrid_union(inc: u64) -> String {
    format!(
        "SELECT e.id::text FROM cypher('obs_h', $$ MATCH (i:incident)-[:affects]->(x:app) WHERE i.id = {inc} RETURN x.id, i.opened_at \
         UNION MATCH (i:incident)-[:affects]->(:app)-[:depends_on]->(x:app) WHERE i.id = {inc} RETURN x.id, i.opened_at $$) AS (x agtype, t0 agtype) \
         JOIN public.events e ON e.app_id = x::text::int AND e.at BETWEEN t0::text::bigint - 600000 AND t0::text::bigint"
    )
}

/// What a CLI pays per invocation: a new connection, LOAD 'age', one W2 query.
async fn age_fresh() {
    let exp = expected();
    let (inc, want) = &exp.w2[4];
    let variants: [(&str, String); 4] = [
        ("graph, VLE *0..1", age_w2_graph(*inc)),
        ("graph, UNION", age_w2_graph_union(*inc)),
        ("hybrid, VLE *0..1", age_w2_hybrid(*inc)),
        ("hybrid, UNION", age_w2_hybrid_union(*inc)),
    ];
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    let mut warm: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
    for _ in 0..RUNS {
        for (i, (_, sql)) in variants.iter().enumerate() {
            let start = Instant::now();
            let c = pg().await;
            let mut got = age_parse(rows(&c, sql).await, &["ev"]);
            times[i].push(ms(start.elapsed()));
            got.sort();
            assert_eq!(&got, want, "{}", variants[i].0);
            let start = Instant::now();
            rows(&c, sql).await;
            warm[i].push(ms(start.elapsed()));
        }
    }
    println!("W2 inc/{inc}, {RUNS} runs each, new connection per run\n\n| variant | connect + first query median (ms) | max | second query on that connection median (ms) |\n|---|---|---|---|");
    for (i, (name, _)) in variants.iter().enumerate() {
        let max = times[i].iter().cloned().fold(0.0, f64::max);
        println!("| {name} | {:.1} | {max:.1} | {:.3} |", pct(&mut times[i], 0.5), pct(&mut warm[i], 0.5));
    }
}

fn age_w3_vle_unanchored(app: u64) -> String {
    cypher("obs", &format!("MATCH (x:app)-[:depends_on*1..]->(a:app) WHERE a.id = {app} RETURN DISTINCT x.id"), &["x"])
}

fn age_w3_vle(app: u64) -> String {
    cypher("obs", &format!("MATCH (a:app {{id: {app}}})<-[:depends_on*1..]-(x:app) RETURN DISTINCT x.id"), &["x"])
}

/// Breadth-first closure over AGE's edge table: UNION in a recursive CTE drops rows already seen.
fn age_w3_cte(g: &str, app: u64) -> String {
    format!(
        "WITH RECURSIVE r(v) AS ( \
           SELECT d.start_id FROM {g}.depends_on d JOIN {g}.app s ON d.end_id = s.id \
             WHERE agtype_access_operator(VARIADIC ARRAY[s.properties, '\"id\"'::agtype]) = '{app}'::agtype \
           UNION SELECT d.start_id FROM {g}.depends_on d JOIN r ON d.end_id = r.v) \
         SELECT agtype_access_operator(VARIADIC ARRAY[a.properties, '\"id\"'::agtype])::text FROM r JOIN {g}.app a ON a.id = r.v"
    )
}

async fn server_ms(c: &Client, sql: &str) -> (f64, f64) {
    let plan = rows(c, &format!("EXPLAIN (ANALYZE, BUFFERS) {sql}")).await;
    let grab = |k: &str| {
        plan.iter()
            .find_map(|r| r[0].strip_prefix(k).map(|s| s.trim().trim_end_matches(" ms").parse::<f64>().unwrap()))
            .unwrap_or(f64::NAN)
    };
    (grab("Planning Time:"), grab("Execution Time:"))
}

async fn age_load() {
    let c = pg().await;
    let sql = std::fs::read_to_string("sql/age_load.sql").unwrap();
    let mut phases: BTreeMap<String, f64> = BTreeMap::new();
    let (mut phase, total) = (String::new(), Instant::now());
    for stmt in sql.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
        let body: String = stmt.lines().filter(|l| {
            if let Some(p) = l.strip_prefix("-- step ") {
                phase = p.to_string();
            }
            !l.starts_with("--")
        }).collect::<Vec<_>>().join("\n");
        if body.trim().is_empty() {
            continue;
        }
        let start = Instant::now();
        c.batch_execute(&body).await.unwrap_or_else(|e| panic!("{e:?}\n{body}"));
        let t = ms(start.elapsed());
        *phases.entry(phase.clone()).or_default() += t;
        if t > 500.0 {
            println!("{t:>9.0} ms  {}", body.lines().next().unwrap());
        }
    }
    println!("\n| phase | ms |\n|---|---|");
    for (p, t) in &phases {
        println!("| {p} | {t:.0} |");
    }
    println!("| total | {:.0} |", ms(total.elapsed()));
    for q in [
        "SELECT count(*) FROM obs.app", "SELECT count(*) FROM obs.incident", "SELECT count(*) FROM obs.event",
        "SELECT count(*) FROM obs.depends_on", "SELECT count(*) FROM obs.affects", "SELECT count(*) FROM obs.about",
        "SELECT count(*) FROM public.events",
    ] {
        println!("{q}: {}", rows(&c, q).await[0][0]);
    }
    age_sizes(&c).await;
}

async fn age_sizes(c: &Client) {
    let q = "SELECT n.nspname, pg_size_pretty(sum(pg_total_relation_size(r.oid))), sum(pg_total_relation_size(r.oid)) \
             FROM pg_class r JOIN pg_namespace n ON n.oid = r.relnamespace \
             WHERE r.relkind IN ('r','m') AND n.nspname IN ('obs','obs_h','public') GROUP BY 1 ORDER BY 1";
    println!("\n| schema | size (tables + indexes + toast) |\n|---|---|");
    for r in rows(c, q).await {
        println!("| {} | {} |", r[0], r[1]);
    }
    let q = "SELECT n.nspname||'.'||r.relname, pg_size_pretty(pg_relation_size(r.oid)), r.relkind FROM pg_class r \
             JOIN pg_namespace n ON n.oid = r.relnamespace WHERE n.nspname IN ('obs','obs_h','public') \
             AND r.relkind IN ('r','i') AND pg_relation_size(r.oid) > 1048576 ORDER BY pg_relation_size(r.oid) DESC";
    println!("\n| relation | size | kind |\n|---|---|---|");
    for r in rows(c, q).await {
        println!("| {} | {} | {} |", r[0], r[1], r[2]);
    }
    println!("database: {}", rows(c, "SELECT pg_size_pretty(pg_database_size('obs'))").await[0][0]);
}

fn age_parse(rows: Vec<Vec<String>>, prefixes: &[&str]) -> Vec<String> {
    rows.into_iter()
        .map(|r| r.iter().zip(prefixes).map(|(v, p)| format!("{p}/{v}")).collect::<Vec<_>>().join(" "))
        .collect()
}

async fn age_bench() {
    let c = pg().await;
    let exp = expected();
    let w1_rows = |g: &'static str| {
        let c = &c;
        move || -> Fut<'_> { Box::pin(async move { age_parse(rows(c, &age_w1(g)).await, &["inc", "app"]) }) }
    };
    let (f1, f2) = (w1_rows("obs"), w1_rows("obs_h"));
    let got = rows(&c, &age_w1("obs")).await;
    let ids: Vec<u64> = got.iter().map(|r| r[0].parse().unwrap()).collect();
    println!("W1 order: AGE returns ascending numeric incident id: {}", ids.windows(2).all(|w| w[0] < w[1]));
    let w1 = bench("W1", &exp.w1, &[("AGE", &f1), ("AGE hybrid", &f2)]).await;
    println!("\n### W1 ({} rows)\n\n| engine | median (ms) | max (ms) | server planning (ms) | server execution (ms) |\n|---|---|---|---|---|", w1[0].rows);
    for (name, (m, g)) in ["AGE", "AGE hybrid"].iter().zip(w1.iter().zip(["obs", "obs_h"])) {
        let (p, e) = server_ms(&c, &age_w1(g)).await;
        println!("| {name} | {:.3} | {:.3} | {p:.3} | {e:.3} |", m.median, m.max);
    }

    let mut ping = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        rows(&c, "SELECT 1").await;
        ping.push(ms(start.elapsed()));
    }
    println!("\nround trip floor, SELECT 1 x50: median {:.3} ms", pct(&mut ping, 0.5));
    let mut w2 = Vec::new();
    let mut w2_server = Vec::new();
    for (inc, want) in &exp.w2 {
        let inc = *inc;
        let graph = || -> Fut<'_> { Box::pin(async { age_parse(rows(&c, &age_w2_graph(inc)).await, &["ev"]) }) };
        let hybrid = || -> Fut<'_> { Box::pin(async { age_parse(rows(&c, &age_w2_hybrid(inc)).await, &["ev"]) }) };
        w2.push(bench(&format!("W2 inc/{inc}"), want, &[("AGE graph", &graph), ("AGE hybrid", &hybrid)]).await);
        let (pg_, eg) = server_ms(&c, &age_w2_graph(inc)).await;
        let (ph, eh) = server_ms(&c, &age_w2_hybrid(inc)).await;
        w2_server.push((inc, want.len(), pg_ + eg, ph + eh));
    }
    report("W2, 20 incidents", &["AGE graph", "AGE hybrid"], &w2);
    println!("\n| incident | rows | AGE graph median | server plan+exec | AGE hybrid median | server plan+exec |\n|---|---|---|---|---|---|");
    for ((inc, n, sg, sh), m) in w2_server.iter().zip(&w2) {
        println!("| inc/{inc} | {n} | {:.3} | {sg:.3} | {:.3} | {sh:.3} |", m[0].median, m[1].median);
    }

    let fresh = pg().await;
    let start = Instant::now();
    rows(&fresh, &age_w3_vle(exp.w3[1].0)).await;
    println!("\nfirst VLE query on a new connection (builds AGE's in-memory graph context): {:.0} ms", ms(start.elapsed()));
    fresh.batch_execute("SET statement_timeout = '120s'").await.unwrap();
    let hub = exp.w3[0].0;
    let probes = if std::env::var("PROBE").is_ok() { vec![("unanchored, app/5549", age_w3_vle_unanchored(exp.w3[1].0)), ("anchored, hub", age_w3_vle(hub))] } else { vec![] };
    for (label, sql) in probes {
        let _ = [("unanchored, app/5549", age_w3_vle_unanchored(exp.w3[1].0)), ("anchored, hub", age_w3_vle(hub))];
        let start = Instant::now();
        let r = fresh.simple_query(&sql).await;
        println!("W3 Cypher VLE {label}: {} after {:.0} ms", r.map(|m| format!("{} rows", m.len().saturating_sub(1))).unwrap_or_else(|e| format!("{:?}", e.as_db_error().map(|d| d.message().to_string()))), ms(start.elapsed()));
    }
    c.batch_execute("SET statement_timeout = '10s'").await.unwrap();
    let mut vle = Vec::new();
    let mut timed_out = Vec::new();
    for (app, want) in exp.w3.iter().skip(1) {
        let app = *app;
        let start = Instant::now();
        if let Err(e) = c.simple_query(&age_w3_vle(app)).await {
            timed_out.push(format!("app/{app} ({} expected rows, {:?} after {:.0} ms)", want.len(), e.as_db_error().map(|d| d.message().to_string()), ms(start.elapsed())));
            continue;
        }
        let f = || -> Fut<'_> { Box::pin(async { age_parse(rows(&c, &age_w3_vle(app)).await, &["app"]) }) };
        vle.push(bench(&format!("W3 VLE app/{app}"), want, &[("AGE Cypher VLE", &f)]).await);
    }
    c.batch_execute("SET statement_timeout = 0").await.unwrap();
    report(&format!("W3 Cypher VLE, {} of 19 non-hub start apps that finish within 10 s", vle.len()), &["AGE Cypher VLE"], &vle);
    println!("VLE timeouts: {}", if timed_out.is_empty() { "none".to_string() } else { timed_out.join("; ") });
    let mut w3 = Vec::new();
    let mut w3_server = Vec::new();
    for (app, want) in &exp.w3 {
        let app = *app;
        let cte = || -> Fut<'_> { Box::pin(async { age_parse(rows(&c, &age_w3_cte("obs", app)).await, &["app"]) }) };
        let cte_h = || -> Fut<'_> { Box::pin(async { age_parse(rows(&c, &age_w3_cte("obs_h", app)).await, &["app"]) }) };
        w3.push(bench(&format!("W3 app/{app}"), want, &[("AGE SQL CTE", &cte), ("AGE hybrid SQL CTE", &cte_h)]).await);
        let (p, e) = server_ms(&c, &age_w3_cte("obs", app)).await;
        w3_server.push((app, want.len(), p + e));
    }
    report("W3, 20 start apps (recursive CTE over the AGE edge table)", &["AGE SQL CTE", "AGE hybrid SQL CTE"], &w3);
    for ((app, n, s), m) in w3_server.iter().zip(&w3).take(3) {
        println!("app/{app}: rows {n}, client median {:.3} ms, server plan+exec {s:.3} ms", m[0].median);
    }
}

fn iso(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// Dumps the AGE graph as one Turtle file (the per-harvest archive), then parses it back.
async fn age_turtle() {
    use std::io::Write;
    let c = pg().await;
    let start = Instant::now();
    let path = "data/harvest.ttl";
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    writeln!(out, "@prefix ops: <http://ops.example/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .").unwrap();
    let dt = |v: &str| format!("\"{}\"^^xsd:dateTime", iso(v.parse().unwrap()));
    for r in rows(&c, &cypher("obs", "MATCH (a:app) RETURN a.id, a.team, a.region", &["a", "t", "r"])).await {
        writeln!(out, "ops:app\\/{} a ops:Application ; ops:ownedBy \"{}\" ; ops:deployedIn \"{}\" .", r[0], r[1], r[2]).unwrap();
    }
    for r in rows(&c, &cypher("obs", "MATCH (a:app)-[:depends_on]->(b:app) RETURN a.id, b.id", &["a", "b"])).await {
        writeln!(out, "ops:app\\/{} ops:dependsOn ops:app\\/{} .", r[0], r[1]).unwrap();
    }
    for r in rows(&c, &cypher("obs", "MATCH (i:incident)-[:affects]->(a:app) RETURN i.id, i.severity, i.opened_at, a.id", &["i", "s", "t", "a"])).await {
        writeln!(out, "ops:inc\\/{} a ops:Incident ; ops:affects ops:app\\/{} ; ops:severity \"{}\" ; ops:openedAt {} .", r[0], r[3], r[1], dt(&r[2])).unwrap();
    }
    for r in rows(&c, &cypher("obs", "MATCH (e:event)-[:about]->(a:app) RETURN e.id, e.kind, e.at, a.id", &["e", "k", "t", "a"])).await {
        writeln!(out, "ops:ev\\/{} a ops:{} ; ops:about ops:app\\/{} ; ops:at {} .", r[0], r[1].trim_matches('"'), r[3], dt(&r[2])).unwrap();
    }
    drop(out);
    let written = ms(start.elapsed());
    let start = Instant::now();
    let mut n = 0;
    let mut sample = None;
    for t in oxttl::TurtleParser::new().for_reader(std::fs::File::open(path).unwrap()) {
        let t = t.unwrap();
        if sample.is_none() && t.predicate.as_str().ends_with("openedAt") {
            sample = Some(t.to_string());
        }
        n += 1;
    }
    let size = std::fs::metadata(path).unwrap().len();
    println!("Turtle dump: {written:.0} ms, {:.1} MiB, parsed back {n} triples in {:.0} ms; sample {}", size as f64 / 1048576.0, ms(start.elapsed()), sample.unwrap());
}

// ---------------------------------------------------------------- HugeGraph

fn http() -> reqwest::Client {
    reqwest::Client::builder().pool_max_idle_per_host(16).build().unwrap()
}

async fn gremlin(h: &reqwest::Client, script: &str) -> Value {
    gremlin_b(h, script, json!({})).await
}

/// Bindings keep the script text constant, so the server compiles it once and caches it.
async fn gremlin_b(h: &reqwest::Client, script: &str, bindings: Value) -> Value {
    let body = json!({"gremlin": script, "bindings": bindings, "language": "gremlin-groovy",
                      "aliases": {"graph": format!("DEFAULT-{GRAPH}"), "g": format!("__g_DEFAULT-{GRAPH}")}});
    let r = h.post(format!("{HG}/gremlin")).json(&body).send().await.unwrap();
    let status = r.status();
    let v: Value = r.json().await.unwrap();
    assert!(status.is_success(), "gremlin {status}: {v}\n{script}");
    v["result"]["data"].clone()
}

fn hg_strings(v: Value, prefix: &str) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| format!("{prefix}/{}", x.as_i64().map(|n| n % OFFSET).unwrap_or_else(|| panic!("{x}")))).collect()
}

/// Vertex ids are CUSTOMIZE_NUMBER and must be unique across labels.
const OFFSET: i64 = 10_000_000;
const APP: i64 = 0;
const INC: i64 = OFFSET;
const EV: i64 = 2 * OFFSET;

async fn hg_post(h: &reqwest::Client, path: &str, body: &Value) -> Value {
    let r = h.post(format!("{HG}/graphs/{GRAPH}/{path}")).json(body).send().await.unwrap();
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        panic!("{path} {status}: {v}");
    }
    v
}

fn csv(name: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(format!("data/export/{name}.csv"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| l.split(',').map(String::from).collect())
        .collect()
}

const HG_SCHEMA: &str = r#"
def s = graph.schema()
s.propertyKey('team').asText().ifNotExist().create()
s.propertyKey('region').asText().ifNotExist().create()
s.propertyKey('severity').asText().valueSingle().ifNotExist().create()
s.propertyKey('kind').asText().ifNotExist().create()
s.propertyKey('openedAt').asLong().ifNotExist().create()
s.propertyKey('at').asLong().ifNotExist().create()
s.vertexLabel('app').useCustomizeNumberId().properties('team','region').nullableKeys().ifNotExist().create()
s.vertexLabel('incident').useCustomizeNumberId().properties('severity','openedAt').ifNotExist().create()
s.vertexLabel('event').useCustomizeNumberId().properties('kind','at').ifNotExist().create()
s.edgeLabel('dependsOn').sourceLabel('app').targetLabel('app').ifNotExist().create()
s.edgeLabel('affects').sourceLabel('incident').targetLabel('app').ifNotExist().create()
s.edgeLabel('about').sourceLabel('event').targetLabel('app').ifNotExist().create()
s.indexLabel('appByTeamRegion').onV('app').by('team','region').secondary().ifNotExist().create()
s.indexLabel('eventByAt').onV('event').by('at').range().ifNotExist().create()
'ok'
"#;

async fn hg_load() {
    let h = http();
    let total = Instant::now();
    gremlin(&h, HG_SCHEMA).await;
    println!("schema {:.0} ms", ms(total.elapsed()));
    let vertices: Vec<Value> = csv("app").into_iter().map(|r| json!({"label":"app","id":APP + r[0].parse::<i64>().unwrap(),"properties":{"team":r[1],"region":r[2]}}))
        .chain(csv("incident").into_iter().map(|r| json!({"label":"incident","id":INC + r[0].parse::<i64>().unwrap(),"properties":{"severity":r[1],"openedAt":r[2].parse::<i64>().unwrap()}})))
        .chain(csv("event").into_iter().map(|r| json!({"label":"event","id":EV + r[0].parse::<i64>().unwrap(),"properties":{"kind":r[1],"at":r[2].parse::<i64>().unwrap()}})))
        .collect();
    let mut edges = Vec::new();
    for (file, label, from, from_off, to_off) in [("depends_on", "dependsOn", "app", APP, APP), ("affects", "affects", "incident", INC, APP), ("about", "about", "event", EV, APP)] {
        for r in csv(file) {
            edges.push(json!({"label":label,"outV":from_off + r[0].parse::<i64>().unwrap(),"inV":to_off + r[2].parse::<i64>().unwrap(),"outVLabel":from,"inVLabel":"app","properties":{}}));
        }
    }
    let batch = 500;
    for (path, items) in [("graph/vertices/batch", vertices), ("graph/edges/batch", edges)] {
        let start = Instant::now();
        let chunks: Vec<Value> = items.chunks(batch).map(|c| Value::Array(c.to_vec())).collect();
        let n = items.len();
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
        let mut tasks = Vec::new();
        for chunk in chunks {
            let (h, sem) = (h.clone(), sem.clone());
            tasks.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                let url = if path.starts_with("graph/edges") { format!("{path}?check_vertex=false") } else { path.to_string() };
                hg_post(&h, &url, &chunk).await.as_array().map(|a| a.len()).unwrap_or(0)
            }));
        }
        let mut written = 0;
        for t in tasks {
            written += t.await.unwrap();
        }
        println!("{path}: {written} of {n} written in {:.0} ms", ms(start.elapsed()));
    }
    println!("load total {:.0} ms", ms(total.elapsed()));
    for q in ["g.V().hasLabel('app').count()", "g.V().hasLabel('event').count()", "g.V().hasLabel('incident').count()", "g.E().count()", "g.V().hasLabel('app').inE('about').count()", "g.V().hasLabel('app').outE('dependsOn').count()", "g.V().hasLabel('app').inE('affects').count()"] {
        println!("{q}: {}", gremlin(&h, q).await);
    }
}

fn hg_w1() -> String {
    "g.V().hasLabel('app').has('team','Bedrock').has('region','us-east-1').as('a').in('affects').hasLabel('incident').as('i')\
     .order().by(id).select('i','a').by(id)".to_string()
}

const HG_W2_ADJ: &str = "def i = g.V(inc).next(); def t0 = i.value('openedAt'); \
    g.V(i).out('affects').union(identity(), out('dependsOn')).dedup().in('about').has('at', P.between(t0 - 600000L, t0 + 1L)).id()";
const HG_W2_RANGE: &str = "def i = g.V(inc).next(); def t0 = i.value('openedAt'); \
    def scope = g.V(i).out('affects').union(identity(), out('dependsOn')).id().toSet(); \
    g.V().hasLabel('event').has('at', P.between(t0 - 600000L, t0 + 1L)).where(out('about').hasId(within(scope))).id()";
const HG_W3: &str = "g.V(app).repeat(__.in('dependsOn').dedup()).emit().dedup().id()";

async fn hg_w3_kneighbor(h: &reqwest::Client, app: i64) -> Vec<String> {
    let r = h.get(format!("{HG}/graphs/{GRAPH}/traversers/kneighbor"))
        .query(&[("source", app.to_string()), ("direction", "IN".into()), ("label", "dependsOn".into()), ("max_depth", "40".into()), ("limit", "10000000".into())])
        .send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    hg_strings(v["vertices"].clone(), "app")
}

async fn hg_bench() {
    let h = http();
    let exp = expected();
    let w1 = || -> Fut<'_> {
        Box::pin(async {
            gremlin(&h, &hg_w1()).await.as_array().unwrap().iter()
                .map(|r| format!("inc/{} app/{}", r["i"].as_i64().unwrap() - INC, r["a"].as_i64().unwrap())).collect()
        })
    };
    let mut ping = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        gremlin(&h, "1").await;
        ping.push(ms(start.elapsed()));
    }
    let mut vping = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        h.get(format!("{HG}/versions")).send().await.unwrap().bytes().await.unwrap();
        vping.push(ms(start.elapsed()));
    }
    println!("round trip floor x50: Gremlin script `1` median {:.3} ms, GET /versions median {:.3} ms", pct(&mut ping, 0.5), pct(&mut vping, 0.5));
    let m = bench("W1", &exp.w1, &[("HugeGraph", &w1)]).await;
    println!("\n### W1 ({} rows)\n\n| engine | median (ms) | max (ms) |\n|---|---|---|\n| HugeGraph Gremlin | {:.3} | {:.3} |", m[0].rows, m[0].median, m[0].max);

    let mut w2 = Vec::new();
    for (inc, want) in &exp.w2 {
        let inc = *inc as i64;
        let b = json!({"inc": INC + inc});
        let adj = || -> Fut<'_> { Box::pin(async { hg_strings(gremlin_b(&h, HG_W2_ADJ, b.clone()).await, "ev") }) };
        let rng = || -> Fut<'_> { Box::pin(async { hg_strings(gremlin_b(&h, HG_W2_RANGE, b.clone()).await, "ev") }) };
        w2.push(bench(&format!("W2 inc/{inc}"), want, &[("HugeGraph adjacency", &adj), ("HugeGraph range index", &rng)]).await);
    }
    report("W2, 20 incidents", &["HugeGraph adjacency", "HugeGraph range index"], &w2);

    let mut w3 = Vec::new();
    for (app, want) in &exp.w3 {
        let app = *app as i64;
        let b = json!({"app": app});
        let gr = || -> Fut<'_> { Box::pin(async { hg_strings(gremlin_b(&h, HG_W3, b.clone()).await, "app") }) };
        let kn = || -> Fut<'_> { Box::pin(async { hg_w3_kneighbor(&h, app).await }) };
        w3.push(bench(&format!("W3 app/{app}"), want, &[("HugeGraph Gremlin repeat", &gr), ("HugeGraph kneighbor REST", &kn)]).await);
    }
    report("W3, 20 start apps", &["HugeGraph Gremlin repeat", "HugeGraph kneighbor REST"], &w3);
    let profile = |script: String, b: Value| {
        let h = &h;
        async move {
            let v = gremlin_b(h, &format!("{script}.profile()"), b).await;
            v[0]["dur"].as_f64().unwrap_or(f64::NAN)
        }
    };
    println!("\nserver time from Gremlin profile() (ms): W1 {:.3}, W2 inc/{} {:.3}, W3 app/0 {:.3}, W3 app/{} {:.3}",
        profile(hg_w1(), json!({})).await,
        exp.w2[4].0, profile(HG_W2_ADJ.to_string(), json!({"inc": INC + exp.w2[4].0 as i64})).await,
        profile(HG_W3.to_string(), json!({"app": exp.w3[0].0})).await,
        exp.w3[1].0, profile(HG_W3.to_string(), json!({"app": exp.w3[1].0})).await);
    for (i, m) in w3.iter().enumerate().take(3) {
        println!("app/{}: rows {}, medians {:.3} / {:.3} ms", exp.w3[i].0, m[0].rows, m[0].median, m[1].median);
    }
}

// ---------------------------------------------------------------- cold start

/// Restarts the container and times until W1 returns the expected rows.
async fn coldstart(engine: &str) {
    let exp = expected();
    let container = if engine == "age" { "age" } else { "hugegraph" };
    let out = std::process::Command::new("docker").args(["stop", container]).output().unwrap();
    assert!(out.status.success());
    if std::env::var("DROP").is_ok() {
        let out = std::process::Command::new("docker").args(["run", "--rm", "--privileged", "alpine:3.20", "sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let start = Instant::now();
    let out = std::process::Command::new("docker").args(["start", container]).output().unwrap();
    assert!(out.status.success());
    let started = ms(start.elapsed());
    loop {
        let ok = if engine == "age" {
            match tokio_postgres::connect(PG, NoTls).await {
                Ok((c, conn)) => {
                    tokio::spawn(conn);
                    let _ = c.batch_execute("LOAD 'age'; SET search_path = ag_catalog, \"$user\", public;").await;
                    match c.simple_query(&age_w1("obs")).await {
                        Ok(m) => m.iter().filter(|x| matches!(x, SimpleQueryMessage::Row(_))).count() == exp.w1.len(),
                        Err(_) => false,
                    }
                }
                Err(_) => false,
            }
        } else {
            let h = http();
            let body = json!({"gremlin": format!("{}.count()", hg_w1()), "bindings": {}, "language": "gremlin-groovy",
                              "aliases": {"graph": format!("DEFAULT-{GRAPH}"), "g": format!("__g_DEFAULT-{GRAPH}")}});
            match h.post(format!("{HG}/gremlin")).json(&body).send().await {
                Ok(r) if r.status().is_success() => r.json::<Value>().await.map(|v| v["result"]["data"][0] == json!(exp.w1.len())).unwrap_or(false),
                _ => false,
            }
        };
        if ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("{engine}: docker start {started:.0} ms, first correct W1 at {:.0} ms", ms(start.elapsed()));
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("age-load") => age_load().await,
        Some("age-bench") => age_bench().await,
        Some("age-sizes") => age_sizes(&pg().await).await,
        Some("hg-load") => hg_load().await,
        Some("hg-bench") => hg_bench().await,
        Some("coldstart") => coldstart(&args[2]).await,
        Some("age-turtle") => age_turtle().await,
        Some("age-fresh") => age_fresh().await,
        _ => eprintln!("usage: bench age-load|age-bench|age-sizes|hg-load|hg-bench|coldstart age|hg"),
    }
}
