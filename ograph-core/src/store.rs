//! The quad store: one redb file.
//!
//! Every fact lives in a named graph (one per harvest batch). The default graph is the
//! deduplicated union of all named graphs, kept as reference-counted rows, so SPARQL over the
//! default graph never sees a triple twice even when several harvests assert it.
//!
//! | table | key | value | answers |
//! |---|---|---|---|
//! | `id2term` | id | encoded term | externalize |
//! | `hash2id` | sip128(encoded term) | id | internalize |
//! | `uspo` `upos` `uosp` | 3 ids | graphs holding it | default graph (union) |
//! | `gspo` | g + 3 ids | | one named graph's contents, graph replace |
//! | `spog` | 3 ids + g | | which graphs hold a triple (provenance) |
//!
//! Patterns over named graphs scan the union ordering that fits the bound positions and then
//! look up `spog` or `gspo`, so two graph-qualified orderings cover what Oxigraph needs six for.
//! | `time` | [at, event] | | events in a window |
//! | `app_time` | [app, at, event] | | events on one app in a window |
//! | `graphs` | g | quads | named graph listing |
//!
//! History is what a harvest removed, so the store can answer as it would have at an earlier
//! time ([`Snapshot::known_at`]). It grows with what changes, not with what is held: a graph's
//! first quads are dated by `born` alone.
//!
//! | table | key | value | answers |
//! |---|---|---|---|
//! | `born` | g | ms | when the graph's present run of quads began |
//! | `added` | g + 3 ids | ms | a present quad written after its graph was born |
//! | `history` | g + 3 ids + added ms | removed ms | quads that are gone, and when they were held |

use crate::term::{self, EPHEMERAL, Id};
use crate::vocab::evt;
use oxrdf::{NamedNode, Term};
use redb::{
    Database, ReadOnlyTable, ReadTransaction, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    Table, TableDefinition, WriteTransaction,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub type Error = redb::Error;
pub type Result<T> = std::result::Result<T, Error>;

fn read_only() -> Error {
    redb::Error::Io(std::io::Error::other("the store is open read-only"))
}

pub fn corrupted(msg: impl Into<String>) -> Error {
    redb::Error::Corrupted(msg.into())
}

const ID2TERM: TableDefinition<Id, &[u8]> = TableDefinition::new("id2term");
const HASH2ID: TableDefinition<u128, Id> = TableDefinition::new("hash2id");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const GRAPHS: TableDefinition<Id, u64> = TableDefinition::new("graphs");
const BORN: TableDefinition<Id, i64> = TableDefinition::new("born");
const ADDED: TableDefinition<[Id; 4], i64> = TableDefinition::new("added");
const HISTORY: TableDefinition<[Id; 5], i64> = TableDefinition::new("history");
const HISTORY_SINCE: &str = "history_since";
const TIME: TableDefinition<[Id; 2], ()> = TableDefinition::new("time");
const APP_TIME: TableDefinition<[Id; 3], ()> = TableDefinition::new("app_time");
const UNION: [TableDefinition<[Id; 3], u32>; 3] =
    [TableDefinition::new("uspo"), TableDefinition::new("upos"), TableDefinition::new("uosp")];
const NAMED: [TableDefinition<[Id; 4], ()>; 2] = [TableDefinition::new("gspo"), TableDefinition::new("spog")];
// Quad positions: S=0, P=1, O=2, G=3. Each ordering lists positions in key order.
const UNION_ORDER: [[usize; 3]; 3] = [[0, 1, 2], [1, 2, 0], [2, 0, 1]];
const NAMED_ORDER: [[usize; 4]; 2] = [[3, 0, 1, 2], [0, 1, 2, 3]];

/// Which graphs a pattern ranges over.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GraphSel {
    Union,
    Named(Id),
    AnyNamed,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub struct Store {
    db: Db,
    path: PathBuf,
}

enum Db {
    Write(Database),
    /// Opened without writing the file, so a read pays no fsync (see [`Store::open_read_only`]).
    Read(redb::ReadOnlyDatabase),
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        // redb's page cache; a large store loads far faster when its B-trees stay cached.
        let cache_mb: usize = std::env::var("OGRAPH_CACHE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(1024);
        // MultiWriter lets `serve`, a harvest and any number of read commands share the file;
        // write transactions still run one at a time.
        let db = Database::builder()
            .set_cache_size(cache_mb << 20)
            .set_concurrency_mode(redb::ConcurrencyMode::MultiWriter)
            .create(path)?;
        let store = Self { db: Db::Write(db), path: path.to_owned() };
        // Only a new file, or one from before the history tables, lacks a table, so a read
        // command opens without a commit and its fsync.
        if store.snapshot().is_err() {
            store.transact(|_| Ok(()))?;
        }
        Ok(store)
    }

    /// An existing store opened for reading only. Opening for writing marks the file open and
    /// syncs it, which on a slow disk costs more than a short read; this does neither. A file
    /// that needs recovery is refused, so the caller can fall back to [`Store::open`].
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let db = Database::builder().set_concurrency_mode(redb::ConcurrencyMode::MultiWriter).open_read_only(path)?;
        Ok(Self { db: Db::Read(db), path: path.to_owned() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// An exclusive lock across processes and threads, released when the file is dropped. A
    /// harvest or verdict holds it from its snapshot to its commit, so it never commits facts
    /// checked against a snapshot another writer has since changed.
    pub fn write_lock(&self) -> std::io::Result<std::fs::File> {
        let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(self.path.with_extension("lock"))?;
        f.lock()?;
        Ok(f)
    }

    /// Every commit is durable: redb refuses non-durable commits on a file shared between
    /// processes (see [`Store::open`]).
    pub fn transact<R>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<R>) -> Result<R> {
        self.transact_at(now_ms(), f)
    }

    /// A transaction whose additions and removals are dated `now_ms` in the history.
    pub fn transact_at<R>(&self, now_ms: i64, f: impl FnOnce(&mut Tx<'_>) -> Result<R>) -> Result<R> {
        let Db::Write(db) = &self.db else { return Err(read_only()) };
        let txn = db.begin_write()?;
        let out = {
            let mut tx = Tx::open(&txn, now_ms.max(0))?;
            let out = f(&mut tx)?;
            tx.meta.insert("next_id", tx.next_id)?;
            out
        };
        txn.commit()?;
        Ok(out)
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        Snapshot::open(match &self.db {
            Db::Write(db) => db.begin_read()?,
            Db::Read(db) => db.begin_read()?,
        })
    }

    /// Forgets what was removed before `before_ms`, and moves the start of the history there.
    /// Returns how many rows went.
    pub fn prune_history(&self, before_ms: i64) -> Result<u64> {
        self.transact(|tx| {
            let mut gone = 0;
            tx.history.retain(|_, removed| {
                gone += u64::from(removed < before_ms);
                removed >= before_ms
            })?;
            // Such a quad is then dated by its graph's birth, which is earlier still.
            tx.added.retain(|_, added| {
                gone += u64::from(added < before_ms);
                added >= before_ms
            })?;
            let since = tx.meta.get(HISTORY_SINCE)?.map_or(0, |v| v.value());
            if (before_ms.max(0) as u64) > since {
                tx.meta.insert(HISTORY_SINCE, before_ms as u64)?;
            }
            Ok(gone)
        })
    }

    pub fn compact(&mut self) -> Result<bool> {
        let Db::Write(db) = &mut self.db else { return Err(read_only()) };
        Ok(db.compact(redb::CompactionMode::default())?)
    }
}

fn open_n<'t, K: redb::Key + 'static, V: redb::Value + 'static, const N: usize>(
    txn: &'t WriteTransaction,
    defs: [TableDefinition<K, V>; N],
) -> Result<[Table<'t, K, V>; N]> {
    let tables: Vec<_> = defs.into_iter().map(|d| txn.open_table(d)).collect::<std::result::Result<_, _>>()?;
    Ok(tables.try_into().unwrap_or_else(|_| unreachable!()))
}

fn permute<const N: usize>(order: &[usize; N], quad: &[Id; 4]) -> [Id; N] {
    order.map(|pos| quad[pos])
}

/// One write transaction with every table open.
pub struct Tx<'t> {
    id2term: Table<'t, Id, &'static [u8]>,
    hash2id: Table<'t, u128, Id>,
    meta: Table<'t, &'static str, u64>,
    graphs: Table<'t, Id, u64>,
    born: Table<'t, Id, i64>,
    added: Table<'t, [Id; 4], i64>,
    history: Table<'t, [Id; 5], i64>,
    time: Table<'t, [Id; 2], ()>,
    app_time: Table<'t, [Id; 3], ()>,
    union: [Table<'t, [Id; 3], u32>; 3],
    named: [Table<'t, [Id; 4], ()>; 2],
    cache: FxHashMap<u128, Id>,
    next_id: Id,
    at: Id,
    on: Id,
    now: i64,
}

impl<'t> Tx<'t> {
    fn open(txn: &'t WriteTransaction, now: i64) -> Result<Self> {
        let mut meta = txn.open_table(META)?;
        let next_id = meta.get("next_id")?.map_or(1, |v| v.value());
        if meta.get(HISTORY_SINCE)?.is_none() {
            meta.insert(HISTORY_SINCE, now as u64)?;
        }
        let mut tx = Self {
            id2term: txn.open_table(ID2TERM)?,
            hash2id: txn.open_table(HASH2ID)?,
            meta,
            graphs: txn.open_table(GRAPHS)?,
            born: txn.open_table(BORN)?,
            added: txn.open_table(ADDED)?,
            history: txn.open_table(HISTORY)?,
            time: txn.open_table(TIME)?,
            app_time: txn.open_table(APP_TIME)?,
            union: open_n(txn, UNION)?,
            named: open_n(txn, NAMED)?,
            cache: FxHashMap::default(),
            next_id,
            at: 0,
            on: 0,
            now,
        };
        tx.at = tx.intern(&NamedNode::new_unchecked(evt::AT).into())?;
        tx.on = tx.intern(&NamedNode::new_unchecked(evt::ON).into())?;
        Ok(tx)
    }

    pub fn intern(&mut self, term: &Term) -> Result<Id> {
        if let Some(id) = term::inline(term) {
            return Ok(id);
        }
        let bytes = term::encode(term);
        let h = term::hash(&bytes);
        if let Some(&id) = self.cache.get(&h) {
            return Ok(id);
        }
        let found = self.hash2id.get(h)?.map(|v| v.value());
        let id = match found {
            Some(id) => {
                let stored = self.id2term.get(id)?.ok_or_else(|| corrupted(format!("id {id} has no term")))?;
                if stored.value() != bytes.as_slice() {
                    return Err(corrupted(format!("sip128 collision on {term}")));
                }
                id
            }
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.hash2id.insert(h, id)?;
                self.id2term.insert(id, bytes.as_slice())?;
                id
            }
        };
        self.cache.insert(h, id);
        Ok(id)
    }

    fn union_objects(&self, s: Id, p: Id) -> Result<Vec<Id>> {
        self.union[0].range([s, p, 0]..=[s, p, Id::MAX])?.map(|r| Ok(r?.0.value()[2])).collect()
    }

    /// Inserts quads `[s, p, o, g]`. Returns how many were new to their graph.
    pub fn insert(&mut self, mut quads: Vec<[Id; 4]>) -> Result<usize> {
        quads.sort_unstable_by_key(|q| permute(&NAMED_ORDER[0], q));
        quads.dedup();
        let mut fresh = Vec::with_capacity(quads.len());
        for q in quads {
            if self.named[0].insert(permute(&NAMED_ORDER[0], &q), ())?.is_none() {
                fresh.push(q);
            }
        }
        for (table, order) in self.named.iter_mut().zip(&NAMED_ORDER).skip(1) {
            let mut keys: Vec<[Id; 4]> = fresh.iter().map(|q| permute(order, q)).collect();
            keys.sort_unstable();
            for k in keys {
                table.insert(k, ())?;
            }
        }
        let mut per_graph: FxHashMap<Id, u64> = FxHashMap::default();
        let mut per_triple: FxHashMap<[Id; 3], u32> = FxHashMap::default();
        for q in &fresh {
            *per_graph.entry(q[3]).or_default() += 1;
            *per_triple.entry([q[0], q[1], q[2]]).or_default() += 1;
        }
        let mut held = FxHashSet::default();
        for (g, n) in per_graph {
            let old = self.graphs.get(g)?.map_or(0, |v| v.value());
            self.graphs.insert(g, old + n)?;
            if old == 0 {
                self.born.insert(g, self.now)?;
            } else {
                held.insert(g);
            }
        }
        for q in fresh.iter().filter(|q| held.contains(&q[3])) {
            self.added.insert(permute(&NAMED_ORDER[0], q), self.now)?;
        }
        let mut triples: Vec<([Id; 3], u32)> = per_triple.into_iter().collect();
        triples.sort_unstable();
        let mut new_triples = Vec::new();
        let mut counts = Vec::with_capacity(triples.len());
        for (spo, n) in triples {
            let old = self.union[0].get(spo)?.map_or(0, |v| v.value());
            if old == 0 {
                new_triples.push(spo);
            }
            counts.push(([spo[0], spo[1], spo[2], 0], old + n));
        }
        // Each ordering gets its keys in its own sorted order: inserting upos and uosp in spo
        // order is random access into large B-trees, the main cost of a big load.
        for (table, order) in self.union.iter_mut().zip(&UNION_ORDER) {
            let mut keys: Vec<([Id; 3], u32)> = counts.iter().map(|(q, c)| (permute(order, q), *c)).collect();
            keys.sort_unstable();
            for (k, c) in keys {
                table.insert(k, c)?;
            }
        }
        for [s, p, o] in new_triples {
            if p == self.at && term::millis(o).is_some() {
                self.time.insert([o, s], ())?;
                for app in self.union_objects(s, self.on)? {
                    self.app_time.insert([app, o, s], ())?;
                }
            } else if p == self.on {
                for t in self.union_objects(s, self.at)? {
                    if term::millis(t).is_some() {
                        self.app_time.insert([o, t, s], ())?;
                    }
                }
            }
        }
        Ok(fresh.len())
    }

    pub fn graph_quads(&self, g: Id) -> Result<Vec<[Id; 4]>> {
        self.named[0]
            .range([g, 0, 0, 0]..=[g, Id::MAX, Id::MAX, Id::MAX])?
            .map(|r| {
                let k = r?.0.value();
                Ok([k[1], k[2], k[3], k[0]])
            })
            .collect()
    }

    /// Removes every quad of graph `g`. Returns how many were removed.
    pub fn clear_graph(&mut self, g: Id) -> Result<usize> {
        let quads = self.graph_quads(g)?;
        self.remove(quads)
    }

    /// Replaces graph `g` with `triples`, writing only the difference. Returns (removed, added).
    pub fn replace_graph(&mut self, g: Id, triples: Vec<[Id; 3]>) -> Result<(usize, usize)> {
        let old: FxHashSet<[Id; 4]> = self.graph_quads(g)?.into_iter().collect();
        let new: FxHashSet<[Id; 4]> = triples.into_iter().map(|[s, p, o]| [s, p, o, g]).collect();
        let removed = self.remove(old.difference(&new).copied().collect())?;
        let added = self.insert(new.difference(&old).copied().collect())?;
        Ok((removed, added))
    }

    pub fn remove(&mut self, mut quads: Vec<[Id; 4]>) -> Result<usize> {
        quads.sort_unstable_by_key(|q| permute(&NAMED_ORDER[0], q));
        quads.dedup();
        let mut gone = Vec::with_capacity(quads.len());
        for q in quads {
            if self.named[0].remove(permute(&NAMED_ORDER[0], &q))?.is_some() {
                gone.push(q);
            }
        }
        for (table, order) in self.named.iter_mut().zip(&NAMED_ORDER).skip(1) {
            let mut keys: Vec<[Id; 4]> = gone.iter().map(|q| permute(order, q)).collect();
            keys.sort_unstable();
            for k in keys {
                table.remove(k)?;
            }
        }
        let mut per_graph: FxHashMap<Id, u64> = FxHashMap::default();
        let mut per_triple: FxHashMap<[Id; 3], u32> = FxHashMap::default();
        let mut born: FxHashMap<Id, i64> = FxHashMap::default();
        for q in &gone {
            *per_graph.entry(q[3]).or_default() += 1;
            *per_triple.entry([q[0], q[1], q[2]]).or_default() += 1;
            let [g, s, p, o] = permute(&NAMED_ORDER[0], q);
            let since = match self.added.remove([g, s, p, o])?.map(|v| v.value()) {
                Some(at) => at,
                None => match born.get(&g) {
                    Some(&at) => at,
                    None => {
                        let at = self.born.get(g)?.map_or(0, |v| v.value());
                        born.insert(g, at);
                        at
                    }
                },
            };
            self.history.insert([g, s, p, o, since as u64], self.now)?;
        }
        for (g, n) in per_graph {
            let old = self.graphs.get(g)?.map_or(0, |v| v.value());
            if old <= n {
                self.graphs.remove(g)?;
                self.born.remove(g)?;
            } else {
                self.graphs.insert(g, old - n)?;
            }
        }
        let mut dropped = Vec::new();
        let mut per_triple: Vec<([Id; 3], u32)> = per_triple.into_iter().collect();
        per_triple.sort_unstable();
        for (spo, n) in per_triple {
            let old = self.union[0].get(spo)?.map_or(0, |v| v.value());
            let quad = [spo[0], spo[1], spo[2], 0];
            for (table, order) in self.union.iter_mut().zip(&UNION_ORDER) {
                if old <= n {
                    table.remove(permute(order, &quad))?;
                } else {
                    table.insert(permute(order, &quad), old - n)?;
                }
            }
            if old <= n {
                dropped.push(spo);
            }
        }
        let mut dropped_on: FxHashMap<Id, Vec<Id>> = FxHashMap::default();
        for t in dropped.iter().filter(|t| t[1] == self.on) {
            dropped_on.entry(t[0]).or_default().push(t[2]);
        }
        for t in dropped.iter().filter(|t| t[1] == self.at) {
            let (s, at) = (t[0], t[2]);
            self.time.remove([at, s])?;
            let apps = self.union_objects(s, self.on)?;
            for &app in apps.iter().chain(dropped_on.get(&s).into_iter().flatten()) {
                self.app_time.remove([app, at, s])?;
            }
        }
        for (&s, apps) in &dropped_on {
            for t in self.union_objects(s, self.at)? {
                for &app in apps {
                    self.app_time.remove([app, t, s])?;
                }
            }
        }
        Ok(gone.len())
    }
}

type QuadIter<'a> = Box<dyn Iterator<Item = Result<[Id; 4]>> + 'a>;

/// The quads recorded and removed after a time, with when, before any view applies them.
/// Quads are `[s, p, o, g]`.
pub struct Delta {
    hidden: FxHashMap<[Id; 4], i64>,
    restored: FxHashMap<[Id; 4], i64>,
}

impl Delta {
    /// Every quad recorded or removed since the time.
    pub fn quads(&self) -> impl Iterator<Item = &[Id; 4]> {
        self.hidden.keys().chain(self.restored.keys())
    }
}

/// How the store at an earlier time differs from the store now. Quads are `[s, p, o, g]`.
struct AsOf {
    /// Present quads recorded after the time, with when.
    hidden: FxHashMap<[Id; 4], i64>,
    hidden_per_triple: FxHashMap<[Id; 3], u32>,
    /// Quads held at the time and removed since, with when.
    restored: FxHashMap<[Id; 4], i64>,
    /// The restored quads in each union ordering, graph last, and those of their triples the
    /// union would otherwise lack.
    restored_named: [BTreeSet<[Id; 4]>; 3],
    restored_union: [BTreeSet<[Id; 3]>; 3],
    graphs: Vec<(Id, u64)>,
}

/// A net difference between an earlier time and now, in the union graph. `quad` is
/// `[s, p, o, g]` with one of the graphs that added or removed it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Change {
    pub added: bool,
    pub at_ms: i64,
    pub quad: [Id; 4],
}

/// Two streams in one key order, as one.
struct Merge<'a, K> {
    live: std::iter::Peekable<QuadIter<'a>>,
    old: std::iter::Peekable<std::vec::IntoIter<[Id; 4]>>,
    key: K,
}

impl<K: Fn(&[Id; 4]) -> [Id; 4]> Iterator for Merge<'_, K> {
    type Item = Result<[Id; 4]>;
    fn next(&mut self) -> Option<Self::Item> {
        let live_first = match (self.live.peek(), self.old.peek()) {
            (Some(Ok(l)), Some(o)) => (self.key)(l) <= (self.key)(o),
            (Some(_), _) => true,
            (None, _) => false,
        };
        if live_first { self.live.next() } else { self.old.next().map(Ok) }
    }
}

/// A consistent read view of the whole store, as it is or, after [`Snapshot::known_at`], as it
/// was.
pub struct Snapshot {
    txn: std::sync::Arc<ReadTransaction>,
    as_of: Option<Box<AsOf>>,
    meta: ReadOnlyTable<&'static str, u64>,
    born: ReadOnlyTable<Id, i64>,
    added: ReadOnlyTable<[Id; 4], i64>,
    history: ReadOnlyTable<[Id; 5], i64>,
    id2term: ReadOnlyTable<Id, &'static [u8]>,
    hash2id: ReadOnlyTable<u128, Id>,
    graphs: ReadOnlyTable<Id, u64>,
    time: ReadOnlyTable<[Id; 2], ()>,
    app_time: ReadOnlyTable<[Id; 3], ()>,
    union: [ReadOnlyTable<[Id; 3], u32>; 3],
    named: [ReadOnlyTable<[Id; 4], ()>; 2],
}

fn open_ro<K: redb::Key + 'static, V: redb::Value + 'static, const N: usize>(
    txn: &ReadTransaction,
    defs: [TableDefinition<K, V>; N],
) -> Result<[ReadOnlyTable<K, V>; N]> {
    let tables: Vec<_> = defs.into_iter().map(|d| txn.open_table(d)).collect::<std::result::Result<_, _>>()?;
    Ok(tables.try_into().unwrap_or_else(|_| unreachable!()))
}

/// The ordering whose leading positions are exactly the bound ones, and its key range.
fn ordering<const N: usize>(orders: &[[usize; N]], pattern: [Option<Id>; 4]) -> (usize, [Id; N], [Id; N]) {
    let i = orders
        .iter()
        .position(|o| {
            let bound = o.iter().take_while(|&&pos| pattern[pos].is_some()).count();
            o[bound..].iter().all(|&pos| pattern[pos].is_none())
        })
        .expect("every bound set is a prefix of one ordering");
    let (mut lo, mut hi) = ([0; N], [Id::MAX; N]);
    for (k, &pos) in orders[i].iter().enumerate() {
        let Some(v) = pattern[pos] else { break };
        (lo[k], hi[k]) = (v, v);
    }
    (i, lo, hi)
}

/// Range-scans the ordering that fits the pattern.
fn scan<'a, V: redb::Value + 'static, const N: usize>(
    tables: &'a [ReadOnlyTable<[Id; N], V>],
    orders: &[[usize; N]],
    pattern: [Option<Id>; 4],
) -> QuadIter<'a> {
    let (i, lo, hi) = ordering(orders, pattern);
    let order = orders[i];
    match tables[i].range(lo..=hi) {
        Err(e) => Box::new(std::iter::once(Err(e.into()))),
        Ok(range) => Box::new(range.map(move |r| {
            let key = r?.0.value();
            let mut quad = [0; 4];
            for (k, &pos) in order.iter().enumerate() {
                quad[pos] = key[k];
            }
            Ok(quad)
        })),
    }
}

impl Snapshot {
    fn open(txn: ReadTransaction) -> Result<Self> {
        Self::on(std::sync::Arc::new(txn))
    }

    fn on(txn: std::sync::Arc<ReadTransaction>) -> Result<Self> {
        Ok(Self {
            as_of: None,
            meta: txn.open_table(META)?,
            born: txn.open_table(BORN)?,
            added: txn.open_table(ADDED)?,
            history: txn.open_table(HISTORY)?,
            id2term: txn.open_table(ID2TERM)?,
            hash2id: txn.open_table(HASH2ID)?,
            graphs: txn.open_table(GRAPHS)?,
            time: txn.open_table(TIME)?,
            app_time: txn.open_table(APP_TIME)?,
            union: open_ro(&txn, UNION)?,
            named: open_ro(&txn, NAMED)?,
            txn,
        })
    }

    /// The store as this view's transaction reads it now, with no earlier time applied, so
    /// several views can differ from one state of the store and from nothing else.
    pub fn now(&self) -> Result<Self> {
        Self::on(self.txn.clone())
    }

    /// When the history begins: nothing removed before it is kept. `None` for a store that has
    /// not been written since it gained a history.
    pub fn history_since(&self) -> Result<Option<i64>> {
        Ok(self.meta.get(HISTORY_SINCE)?.map(|v| v.value() as i64))
    }

    /// This view as the store was at `t_ms`: without what was recorded after it, with what was
    /// removed after it. It costs one pass over the history and over the graphs born since, and
    /// keeps both in memory. Only what the history still holds comes back.
    pub fn known_at(self, t_ms: i64) -> Result<Self> {
        let d = self.delta(t_ms)?;
        self.revert(&d, |_| true)
    }

    /// What every graph recorded and removed after `t_ms`, in one pass over the history and over
    /// the graphs born since, for [`Snapshot::revert`] to apply in part.
    pub fn delta(&self, t_ms: i64) -> Result<Delta> {
        let spog = |k: [Id; 4]| [k[1], k[2], k[3], k[0]];
        let mut hidden: FxHashMap<[Id; 4], i64> = FxHashMap::default();
        for r in self.added.iter()? {
            let (k, at) = r?;
            if at.value() > t_ms {
                hidden.insert(spog(k.value()), at.value());
            }
        }
        for r in self.born.iter()? {
            let (g, at) = r?;
            let (g, at) = (g.value(), at.value());
            if at > t_ms {
                for q in self.named[0].range([g, 0, 0, 0]..=[g, Id::MAX, Id::MAX, Id::MAX])? {
                    hidden.entry(spog(q?.0.value())).or_insert(at);
                }
            }
        }
        let mut restored: FxHashMap<[Id; 4], i64> = FxHashMap::default();
        for r in self.history.iter()? {
            let (k, removed) = r?;
            let [g, s, p, o, added] = k.value();
            // A quad the store holds and showed at the time is not also restored; that can
            // only happen when the clock went backwards between two harvests.
            if added as i64 <= t_ms && t_ms < removed.value() && (hidden.contains_key(&[s, p, o, g]) || self.named[0].get([g, s, p, o])?.is_none()) {
                restored.insert([s, p, o, g], removed.value());
            }
        }
        Ok(Delta { hidden, restored })
    }

    /// This view with the graphs `which` selects taken back to the time of `d`, and every other
    /// graph as it is now.
    pub fn revert(self, d: &Delta, which: impl Fn(Id) -> bool) -> Result<Self> {
        let hidden: FxHashMap<[Id; 4], i64> = d.hidden.iter().filter(|(q, _)| which(q[3])).map(|(&q, &t)| (q, t)).collect();
        let restored: FxHashMap<[Id; 4], i64> = d.restored.iter().filter(|(q, _)| which(q[3])).map(|(&q, &t)| (q, t)).collect();
        self.apply(hidden, restored)
    }

    /// This view without `quads` (`[s, p, o, g]`), as if no graph had stated them.
    pub fn without(self, quads: impl IntoIterator<Item = [Id; 4]>) -> Result<Self> {
        self.apply(quads.into_iter().map(|q| (q, 0)).collect(), FxHashMap::default())
    }

    fn apply(mut self, hidden: FxHashMap<[Id; 4], i64>, restored: FxHashMap<[Id; 4], i64>) -> Result<Self> {
        let mut hidden_per_triple: FxHashMap<[Id; 3], u32> = FxHashMap::default();
        for q in hidden.keys() {
            *hidden_per_triple.entry([q[0], q[1], q[2]]).or_default() += 1;
        }
        let mut per_graph: FxHashMap<Id, i64> = FxHashMap::default();
        for r in self.graphs.iter()? {
            let (g, n) = r?;
            per_graph.insert(g.value(), n.value() as i64);
        }
        for (q, sign) in hidden.keys().map(|q| (q, -1)).chain(restored.keys().map(|q| (q, 1))) {
            *per_graph.entry(q[3]).or_default() += sign;
        }
        let mut graphs: Vec<(Id, u64)> = per_graph.into_iter().filter(|g| g.1 > 0).map(|(g, n)| (g, n as u64)).collect();
        graphs.sort_unstable();
        let mut restored_named: [BTreeSet<[Id; 4]>; 3] = Default::default();
        let mut restored_union: [BTreeSet<[Id; 3]>; 3] = Default::default();
        for q in restored.keys() {
            let spo = [q[0], q[1], q[2]];
            let shown = self.union[0].get(spo)?.map_or(0, |n| n.value()) > hidden_per_triple.get(&spo).copied().unwrap_or(0);
            for (i, order) in UNION_ORDER.iter().enumerate() {
                let k = permute(order, q);
                restored_named[i].insert([k[0], k[1], k[2], q[3]]);
                if !shown {
                    restored_union[i].insert(k);
                }
            }
        }
        self.as_of = Some(Box::new(AsOf { hidden, hidden_per_triple, restored, restored_named, restored_union, graphs }));
        Ok(self)
    }

    /// On a [`Snapshot::known_at`] view: how many present quads it hides and how many removed
    /// quads it restores.
    pub fn as_of_counts(&self) -> Option<(usize, usize)> {
        self.as_of.as_ref().map(|a| (a.hidden.len(), a.restored.len()))
    }

    /// On a [`Snapshot::known_at`] view: the triples the union graph gained and lost between
    /// that time and now, oldest change first. A triple that only moved between graphs, or left
    /// and came back, is no change.
    pub fn changes(&self) -> Result<Vec<Change>> {
        let Some(a) = &self.as_of else { return Ok(vec![]) };
        let mut net: FxHashMap<([Id; 3], bool), Change> = FxHashMap::default();
        for (&quad, &at_ms) in &a.hidden {
            let spo = [quad[0], quad[1], quad[2]];
            if !self.contains(spo)? {
                let c = net.entry((spo, true)).or_insert(Change { added: true, at_ms, quad });
                if (at_ms, quad) < (c.at_ms, c.quad) {
                    *c = Change { added: true, at_ms, quad };
                }
            }
        }
        for (&quad, &at_ms) in &a.restored {
            let spo = [quad[0], quad[1], quad[2]];
            if self.union[0].get(spo)?.is_none() {
                let c = net.entry((spo, false)).or_insert(Change { added: false, at_ms, quad });
                if (at_ms, quad) > (c.at_ms, c.quad) {
                    *c = Change { added: false, at_ms, quad };
                }
            }
        }
        let mut out: Vec<Change> = net.into_values().collect();
        out.sort_unstable_by_key(|c| (c.at_ms, c.quad, c.added));
        Ok(out)
    }

    /// Quads matching `[s, p, o]` in the selected graphs. Union quads have graph 0.
    pub fn quads(&self, spo: [Option<Id>; 3], graph: GraphSel) -> QuadIter<'_> {
        match &self.as_of {
            None => self.live(spo, graph),
            Some(a) => self.past(a, spo, graph),
        }
    }

    /// The live quads the view shows, merged in key order with the restored ones, so a
    /// pattern's first match is the one the store gave at the time.
    fn past<'a>(&'a self, a: &'a AsOf, spo: [Option<Id>; 3], graph: GraphSel) -> QuadIter<'a> {
        let (i, lo, hi) = ordering(&UNION_ORDER, [spo[0], spo[1], spo[2], None]);
        let order = UNION_ORDER[i];
        let back = move |k: [Id; 3], g: Id| {
            let mut quad = [0, 0, 0, g];
            for (n, &pos) in order.iter().enumerate() {
                quad[pos] = k[n];
            }
            quad
        };
        let old: Vec<[Id; 4]> = match graph {
            GraphSel::Union => a.restored_union[i].range(lo..=hi).map(|&k| back(k, 0)).collect(),
            _ => a.restored_named[i]
                .range([lo[0], lo[1], lo[2], 0]..=[hi[0], hi[1], hi[2], Id::MAX])
                .filter(|k| graph == GraphSel::AnyNamed || graph == GraphSel::Named(k[3]))
                .map(|&[x, y, z, g]| back([x, y, z], g))
                .collect(),
        };
        let live: QuadIter<'a> = Box::new(self.live(spo, graph).filter_map(move |q| {
            let Ok(q) = q else { return Some(q) };
            let shown = match graph {
                GraphSel::Union => match a.hidden_per_triple.get(&[q[0], q[1], q[2]]) {
                    None => Ok(true),
                    Some(&hidden) => self.union[0].get([q[0], q[1], q[2]]).map(|n| n.is_some_and(|n| n.value() > hidden)),
                },
                _ => Ok(!a.hidden.contains_key(&q)),
            };
            match shown {
                Ok(true) => Some(Ok(q)),
                Ok(false) => None,
                Err(e) => Some(Err(e.into())),
            }
        }));
        if old.is_empty() {
            return live;
        }
        Box::new(Merge { live: live.peekable(), old: old.into_iter().peekable(), key: move |q: &[Id; 4]| [q[order[0]], q[order[1]], q[order[2]], q[3]] })
    }

    fn live(&self, spo: [Option<Id>; 3], graph: GraphSel) -> QuadIter<'_> {
        let g = match graph {
            GraphSel::Named(g) => Some(g),
            _ => None,
        };
        let pattern = [spo[0], spo[1], spo[2], g];
        if pattern.iter().flatten().any(|id| id & EPHEMERAL != 0) {
            return Box::new(std::iter::empty());
        }
        // A fully bound triple is one point lookup, much cheaper in redb than opening a range.
        if let ([Some(a), Some(b), Some(c)], GraphSel::Union) = (spo, graph) {
            return Box::new(match self.union[0].get([a, b, c]) {
                Ok(Some(_)) => Some(Ok([a, b, c, 0])),
                Ok(None) => None,
                Err(e) => Some(Err(e.into())),
            }.into_iter());
        }
        match graph {
            GraphSel::Union => scan(&self.union, &UNION_ORDER, pattern),
            GraphSel::Named(_) if spo[1..].iter().all(Option::is_none) || (spo[0].is_some() && (spo[2].is_none() || spo[1].is_some())) => {
                scan(&self.named[..1], &NAMED_ORDER[..1], pattern)
            }
            GraphSel::Named(g) => Box::new(scan(&self.union, &UNION_ORDER, [spo[0], spo[1], spo[2], None]).filter_map(move |q| {
                match q {
                    Ok([s, p, o, _]) => match self.named[0].get([g, s, p, o]) {
                        Ok(Some(_)) => Some(Ok([s, p, o, g])),
                        Ok(None) => None,
                        Err(e) => Some(Err(e.into())),
                    },
                    Err(e) => Some(Err(e)),
                }
            })),
            // Bound positions that lead spog: one range there, in the order the union would give.
            GraphSel::AnyNamed if spo[0].is_some() && (spo[1].is_some() || spo[2].is_none()) => scan(&self.named[1..], &NAMED_ORDER[1..], pattern),
            GraphSel::AnyNamed => Box::new(scan(&self.union, &UNION_ORDER, [spo[0], spo[1], spo[2], None]).flat_map(move |q| {
                let it: QuadIter<'_> = match q {
                    Ok([s, p, o, _]) => scan(&self.named[1..], &NAMED_ORDER[1..], [Some(s), Some(p), Some(o), None]),
                    Err(e) => Box::new(std::iter::once(Err(e))),
                };
                it
            })),
        }
    }

    pub fn contains(&self, spo: [Id; 3]) -> Result<bool> {
        if self.as_of.is_some() {
            return Ok(self.quads(spo.map(Some), GraphSel::Union).next().transpose()?.is_some());
        }
        Ok(self.union[0].get(spo)?.is_some())
    }

    pub fn id(&self, term: &Term) -> Result<Option<Id>> {
        if let Some(id) = term::inline(term) {
            return Ok(Some(id));
        }
        let bytes = term::encode(term);
        Ok(self.hash2id.get(term::hash(&bytes))?.map(|v| v.value()))
    }

    pub fn iri(&self, iri: &str) -> Result<Option<Id>> {
        self.id(&NamedNode::new_unchecked(iri).into())
    }

    pub fn term(&self, id: Id) -> Result<Term> {
        if let Some(t) = term::inline_term(id) {
            return Ok(t);
        }
        let bytes = self.id2term.get(id)?.ok_or_else(|| corrupted(format!("id {id} has no term")))?;
        Ok(term::decode(bytes.value()))
    }

    /// Objects of `s p ?o` in the union graph.
    pub fn objects(&self, s: Id, p: Id) -> Result<Vec<Id>> {
        self.quads([Some(s), Some(p), None], GraphSel::Union).map(|q| q.map(|q| q[2])).collect()
    }

    pub fn object(&self, s: Id, p: Id) -> Result<Option<Id>> {
        self.quads([Some(s), Some(p), None], GraphSel::Union).next().transpose().map(|q| q.map(|q| q[2]))
    }

    /// Subjects of `?s p o` in the union graph.
    pub fn subjects(&self, p: Id, o: Id) -> Result<Vec<Id>> {
        self.quads([None, Some(p), Some(o)], GraphSel::Union).map(|q| q.map(|q| q[0])).collect()
    }

    /// Events on `app` at `[from, to]`, in time order, as `(time id, event)`.
    pub fn events_on(&self, app: Id, from: Id, to: Id) -> Result<Vec<(Id, Id)>> {
        // The time index describes the store now, so an earlier view reads the facts instead.
        if self.as_of.is_some() {
            let (Some(on), Some(at)) = (self.iri(evt::ON)?, self.iri(evt::AT)?) else { return Ok(vec![]) };
            let mut out = Vec::new();
            for e in self.subjects(on, app)? {
                out.extend(self.objects(e, at)?.into_iter().filter(|&t| term::millis(t).is_some() && from <= t && t <= to).map(|t| (t, e)));
            }
            out.sort_unstable();
            return Ok(out);
        }
        self.app_time.range([app, from, 0]..=[app, to, Id::MAX])?.map(|r| {
            let k = r?.0.value();
            Ok((k[1], k[2]))
        }).collect()
    }

    pub fn graphs(&self) -> Result<Vec<(Id, u64)>> {
        if let Some(a) = &self.as_of {
            return Ok(a.graphs.clone());
        }
        self.graphs.iter()?.map(|r| {
            let (k, v) = r?;
            Ok((k.value(), v.value()))
        }).collect()
    }

    pub fn has_graph(&self, g: Id) -> Result<bool> {
        Ok(match &self.as_of {
            Some(a) => a.graphs.iter().any(|&(x, _)| x == g),
            None => self.graphs.get(g)?.is_some(),
        })
    }

    pub fn graphs_of(&self, spo: [Id; 3]) -> Result<Vec<Id>> {
        self.quads(spo.map(Some), GraphSel::AnyNamed).map(|q| q.map(|q| q[3])).collect()
    }

    pub fn union_len(&self) -> Result<u64> {
        Ok(self.union[0].len()?)
    }

    /// Per table: name, entries, stored bytes, overhead bytes.
    pub fn stats(&self) -> Result<Vec<(&'static str, u64, u64, u64)>> {
        let mut tables: Vec<(&'static str, &dyn ReadableTableMetadata)> = vec![
            ("id2term", &self.id2term),
            ("hash2id", &self.hash2id),
            ("time", &self.time),
            ("app_time", &self.app_time),
            ("graphs", &self.graphs),
            ("born", &self.born),
            ("added", &self.added),
            ("history", &self.history),
        ];
        for (name, t) in ["uspo", "upos", "uosp"].into_iter().zip(&self.union) {
            tables.push((name, t));
        }
        for (name, t) in ["gspo", "spog"].into_iter().zip(&self.named) {
            tables.push((name, t));
        }
        tables
            .into_iter()
            .map(|(name, t)| {
                let s = t.stats()?;
                Ok((name, t.len()?, s.stored_bytes(), s.metadata_bytes() + s.fragmented_bytes()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(name: &str, n: usize) -> (Store, Id, Id, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("ograph-store-{name}-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let (g, app) = store
            .transact(|tx| {
                let g = tx.intern(&NamedNode::new_unchecked("https://ograph.dev/g/test").into())?;
                let app = tx.intern(&NamedNode::new_unchecked("https://ograph.dev/id/app/a").into())?;
                let mut quads = Vec::new();
                for k in 0..n {
                    let e = tx.intern(&NamedNode::new_unchecked(format!("https://ograph.dev/id/ev/{k}")).into())?;
                    quads.push([e, tx.at, term::time_id(1_700_000_000_000 + k as i64 * 1000), g]);
                    quads.push([e, tx.on, app, g]);
                }
                tx.insert(quads)?;
                Ok((g, app))
            })
            .unwrap();
        (store, g, app, path)
    }

    #[test]
    fn clearing_events_empties_the_time_indexes() {
        let (store, g, app, path) = events("clear", 3);
        assert_eq!(store.snapshot().unwrap().events_on(app, 0, Id::MAX).unwrap().len(), 3);
        assert_eq!(store.transact(|tx| tx.clear_graph(g)).unwrap(), 6);
        let snap = store.snapshot().unwrap();
        assert_eq!(snap.events_on(app, 0, Id::MAX).unwrap(), vec![]);
        assert_eq!(snap.stats().unwrap().iter().find(|t| t.0 == "time").unwrap().1, 0);
        drop((snap, store));
        let _ = std::fs::remove_file(path);
    }

    fn rows(snap: &Snapshot, table: &str) -> u64 {
        snap.stats().unwrap().iter().find(|t| t.0 == table).unwrap().1
    }

    /// Graph `g` holds events 0 and 1 from time 1000. At 2000 a replace drops event 0, moves
    /// event 1 by a second and adds event 2.
    #[test]
    fn an_earlier_view_answers_as_the_store_did_then() {
        let path = std::env::temp_dir().join(format!("ograph-store-known-at-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let node = |tx: &mut Tx<'_>, iri: &str| tx.intern(&NamedNode::new_unchecked(format!("https://ograph.dev/{iri}")).into());
        let t = |k: i64| term::time_id(1_700_000_000_000 + k * 1000);
        let (g, app, e, at, on) = store
            .transact_at(1000, |tx| {
                let (g, app) = (node(tx, "g/test")?, node(tx, "id/app/a")?);
                let e = [node(tx, "id/ev/0")?, node(tx, "id/ev/1")?, node(tx, "id/ev/2")?];
                tx.insert(vec![[e[0], tx.at, t(0), g], [e[0], tx.on, app, g], [e[1], tx.at, t(1), g], [e[1], tx.on, app, g]])?;
                Ok((g, app, e, tx.at, tx.on))
            })
            .unwrap();
        let first = store.snapshot().unwrap();
        assert_eq!((rows(&first, "born"), rows(&first, "added"), rows(&first, "history")), (1, 0, 0));
        drop(first);
        let replaced = store.transact_at(2000, |tx| tx.replace_graph(g, vec![[e[1], at, t(5)], [e[1], on, app], [e[2], at, t(2)], [e[2], on, app]])).unwrap();
        assert_eq!(replaced, (3, 3));

        let now = store.snapshot().unwrap();
        assert_eq!(now.events_on(app, 0, Id::MAX).unwrap(), [(t(2), e[2]), (t(5), e[1])]);
        assert_eq!((rows(&now, "born"), rows(&now, "added"), rows(&now, "history")), (1, 3, 3));

        let then = store.snapshot().unwrap().known_at(1999).unwrap();
        assert_eq!(then.as_of_counts(), Some((3, 3)));
        assert_eq!(then.events_on(app, 0, Id::MAX).unwrap(), [(t(0), e[0]), (t(1), e[1])]);
        assert_eq!(then.objects(e[1], at).unwrap(), [t(1)]);
        assert_eq!(then.subjects(on, app).unwrap(), [e[0], e[1]]);
        assert_eq!(then.graphs().unwrap(), [(g, 4)]);
        assert!(then.contains([e[0], on, app]).unwrap());
        assert!(!then.contains([e[2], on, app]).unwrap());
        let named: Vec<[Id; 4]> = then.quads([None; 3], GraphSel::Named(g)).map(Result::unwrap).collect();
        assert_eq!(named, [[e[0], at, t(0), g], [e[0], on, app, g], [e[1], at, t(1), g], [e[1], on, app, g]]);
        assert_eq!(then.graphs_of([e[0], on, app]).unwrap(), [g]);
        assert_eq!(then.graphs_of([e[2], on, app]).unwrap(), Vec::<Id>::new());
        let change = |added, quad| Change { added, at_ms: 2000, quad };
        let mut expected = vec![
            change(false, [e[0], at, t(0), g]),
            change(false, [e[0], on, app, g]),
            change(false, [e[1], at, t(1), g]),
            change(true, [e[1], at, t(5), g]),
            change(true, [e[2], at, t(2), g]),
            change(true, [e[2], on, app, g]),
        ];
        expected.sort_unstable_by_key(|c| (c.at_ms, c.quad, c.added));
        assert_eq!(then.changes().unwrap(), expected);

        let before_any = store.snapshot().unwrap().known_at(999).unwrap();
        assert_eq!(before_any.events_on(app, 0, Id::MAX).unwrap(), []);
        assert_eq!(before_any.graphs().unwrap(), []);
        let same = store.snapshot().unwrap().known_at(2000).unwrap();
        assert_eq!(same.as_of_counts(), Some((0, 0)));
        assert_eq!(same.events_on(app, 0, Id::MAX).unwrap(), now.events_on(app, 0, Id::MAX).unwrap());
        drop((now, then, before_any, same));

        assert_eq!(store.prune_history(1500).unwrap(), 0);
        let later = now_ms() + 1000;
        assert_eq!(store.prune_history(later).unwrap(), 6);
        let pruned = store.snapshot().unwrap();
        assert_eq!((rows(&pruned, "added"), rows(&pruned, "history"), pruned.history_since().unwrap()), (0, 0, Some(later)));
        assert_eq!(pruned.known_at(later).unwrap().events_on(app, 0, Id::MAX).unwrap(), [(t(2), e[2]), (t(5), e[1])]);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    /// A graph cleared and written again is dated from its second birth, and a triple two
    /// graphs hold stays in the union while either shows it.
    #[test]
    fn a_graph_that_left_and_came_back_is_hidden_in_between() {
        let path = std::env::temp_dir().join(format!("ograph-store-reborn-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let node = |tx: &mut Tx<'_>, iri: &str| tx.intern(&NamedNode::new_unchecked(format!("https://ograph.dev/{iri}")).into());
        let (g, h, s, p, o) = store
            .transact_at(1000, |tx| {
                let ids = (node(tx, "g/one")?, node(tx, "g/two")?, node(tx, "id/s")?, node(tx, "p")?, node(tx, "id/o")?);
                tx.insert(vec![[ids.2, ids.3, ids.4, ids.0]])?;
                Ok(ids)
            })
            .unwrap();
        store.transact_at(2000, |tx| tx.clear_graph(g)).unwrap();
        store.transact_at(3000, |tx| tx.insert(vec![[s, p, o, g]])).unwrap();
        store.transact_at(4000, |tx| tx.insert(vec![[s, p, o, h]])).unwrap();
        let at = |t: i64| {
            let snap = store.snapshot().unwrap().known_at(t).unwrap();
            let named: Vec<Id> = snap.quads([Some(s), None, None], GraphSel::AnyNamed).map(|q| q.unwrap()[3]).collect();
            (snap.objects(s, p).unwrap(), named, snap.changes().unwrap().len())
        };
        assert_eq!(at(1500), (vec![o], vec![g], 0));
        assert_eq!(at(2500), (vec![], vec![], 1));
        assert_eq!(at(3500), (vec![o], vec![g], 0));
        assert_eq!(at(4000), (vec![o], vec![g, h], 0));
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_second_handle_shares_the_file() {
        let (first, g, app, path) = events("shared", 3);
        let second = Store::open(&path).unwrap();
        assert_eq!(second.snapshot().unwrap().events_on(app, 0, Id::MAX).unwrap().len(), 3);
        second.transact(|tx| tx.clear_graph(g)).unwrap();
        assert_eq!(first.snapshot().unwrap().events_on(app, 0, Id::MAX).unwrap(), vec![]);
        drop((first, second));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_read_only_handle_sees_commits_and_leaves_the_file_alone() {
        let (writer, g, app, path) = events("read-only", 3);
        let modified = || std::fs::metadata(&path).unwrap().modified().unwrap();
        let before = modified();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let reader = Store::open_read_only(&path).unwrap();
        assert_eq!(reader.snapshot().unwrap().events_on(app, 0, Id::MAX).unwrap().len(), 3);
        assert!(reader.transact(|_| Ok(())).is_err());
        writer.transact(|tx| tx.clear_graph(g)).unwrap();
        let after_commit = modified();
        assert_eq!(reader.snapshot().unwrap().events_on(app, 0, Id::MAX).unwrap(), vec![]);
        drop(reader);
        assert!(after_commit > before);
        assert_eq!(modified(), after_commit, "the reader wrote nothing");
        drop(writer);
        let _ = std::fs::remove_file(path);
    }
}
