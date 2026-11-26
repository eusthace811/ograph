pub mod sparql;
pub mod store;
pub mod term;
pub mod vocab;
pub mod shacl;
pub mod gateway;
pub mod model;
pub mod ontology;
pub mod enrich;
pub mod llm;
pub mod correlate;
pub mod coverage;
pub mod query;
pub mod workspace;
pub mod export;
pub mod serve;
pub mod lab;
pub mod cite;
pub mod status;

/// `f` of every item, in input order, on up to `threads` threads that each take the next item.
pub(crate) fn ordered_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut out: Vec<(usize, R)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..items.len().min(threads.max(1)))
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
        workers.into_iter().flat_map(|w| w.join().expect("a worker does not panic")).collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}
