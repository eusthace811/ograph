//! Loops one SPARQL query against an existing store for N seconds, for `sample <pid>`.
//! Usage: profile_query STORE.redb SECONDS 'QUERY'
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let store = ograph_core::store::Store::open(std::path::Path::new(&a[1]))?;
    let snap = store.snapshot()?;
    if a[2] == "explain" {
        let q = ograph_core::sparql::parse(&a[3])?;
        let ev = spareval::QueryEvaluator::new().compute_statistics();
        let (r, ex) = ev.prepare(&q).explain(ograph_core::sparql::View::new(&snap));
        if let spareval::QueryResults::Solutions(s) = r? {
            s.for_each(drop);
        }
        let mut out = Vec::new();
        ex.write_in_json(&mut out)?;
        println!("{}", String::from_utf8(out)?);
        return Ok(());
    }
    let secs: u64 = a[2].parse()?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let (mut n, mut runs) = (0, 0);
    println!("pid {}", std::process::id());
    let t = std::time::Instant::now();
    while std::time::Instant::now() < until {
        n += ograph_core::sparql::solutions(&snap, &a[3])?.len();
        runs += 1;
    }
    println!("{runs} runs, {n} rows, {:.3} ms per run", t.elapsed().as_secs_f64() * 1e3 / runs as f64);
    Ok(())
}
