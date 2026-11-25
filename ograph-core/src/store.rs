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

use crate::term::{self, EPHEMERAL, Id};
use crate::vocab::evt;
use oxrdf::{NamedNode, Term};
use redb::{
    Database, ReadOnlyTable, ReadTransaction, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    Table, TableDefinition, WriteTransaction,
};
use rustc_hash::{FxHashMap, FxHashSet};
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
        // Only a new file lacks the tables, so a read command opens without a commit and its fsync.
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

    pub fn transact<R>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<R>) -> Result<R> {
        self.transact_with(true, f)
    }

    /// Every commit is durable whatever `durable` says: redb refuses non-durable commits on a
    /// file shared between processes (see [`Store::open`]).
    pub fn transact_with<R>(&self, _durable: bool, f: impl FnOnce(&mut Tx<'_>) -> Result<R>) -> Result<R> {
        let Db::Write(db) = &self.db else { return Err(read_only()) };
        let txn = db.begin_write()?;
        let out = {
            let mut tx = Tx::open(&txn)?;
            let out = f(&mut tx)?;
            tx.meta.insert("next_id", tx.next_id)?;
            out
        };
        txn.commit()?;
        Ok(out)
    }

    /// Nothing to do: every commit is durable (see [`Store::transact_with`]).
    pub fn flush(&self) -> Result<()> {
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        Snapshot::open(match &self.db {
            Db::Write(db) => db.begin_read()?,
            Db::Read(db) => db.begin_read()?,
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
    time: Table<'t, [Id; 2], ()>,
    app_time: Table<'t, [Id; 3], ()>,
    union: [Table<'t, [Id; 3], u32>; 3],
    named: [Table<'t, [Id; 4], ()>; 2],
    cache: FxHashMap<u128, Id>,
    next_id: Id,
    at: Id,
    on: Id,
}

impl<'t> Tx<'t> {
    fn open(txn: &'t WriteTransaction) -> Result<Self> {
        let meta = txn.open_table(META)?;
        let next_id = meta.get("next_id")?.map_or(1, |v| v.value());
        let mut tx = Self {
            id2term: txn.open_table(ID2TERM)?,
            hash2id: txn.open_table(HASH2ID)?,
            meta,
            graphs: txn.open_table(GRAPHS)?,
            time: txn.open_table(TIME)?,
            app_time: txn.open_table(APP_TIME)?,
            union: open_n(txn, UNION)?,
            named: open_n(txn, NAMED)?,
            cache: FxHashMap::default(),
            next_id,
            at: 0,
            on: 0,
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

    pub fn term(&self, id: Id) -> Result<Term> {
        if let Some(t) = term::inline_term(id) {
            return Ok(t);
        }
        let bytes = self.id2term.get(id)?.ok_or_else(|| corrupted(format!("id {id} has no term")))?;
        Ok(term::decode(bytes.value()))
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
            if self.named[0].get(permute(&NAMED_ORDER[0], &q))?.is_none() {
                fresh.push(q);
            }
        }
        for (table, order) in self.named.iter_mut().zip(&NAMED_ORDER) {
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
        for (g, n) in per_graph {
            let old = self.graphs.get(g)?.map_or(0, |v| v.value());
            self.graphs.insert(g, old + n)?;
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
        for q in &gone {
            *per_graph.entry(q[3]).or_default() += 1;
            *per_triple.entry([q[0], q[1], q[2]]).or_default() += 1;
        }
        for (g, n) in per_graph {
            let old = self.graphs.get(g)?.map_or(0, |v| v.value());
            if old <= n {
                self.graphs.remove(g)?;
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

/// A consistent read view of the whole store.
pub struct Snapshot {
    _txn: ReadTransaction,
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

/// Picks the ordering whose leading positions are exactly the bound ones, then range-scans it.
fn scan<'a, V: redb::Value + 'static, const N: usize>(
    tables: &'a [ReadOnlyTable<[Id; N], V>],
    orders: &[[usize; N]],
    pattern: [Option<Id>; 4],
) -> QuadIter<'a> {
    let i = orders
        .iter()
        .position(|o| {
            let bound = o.iter().take_while(|&&pos| pattern[pos].is_some()).count();
            o[bound..].iter().all(|&pos| pattern[pos].is_none())
        })
        .expect("every bound set is a prefix of one ordering");
    let order = orders[i];
    let (mut lo, mut hi) = ([0; N], [Id::MAX; N]);
    for (k, &pos) in order.iter().enumerate() {
        let Some(v) = pattern[pos] else { break };
        (lo[k], hi[k]) = (v, v);
    }
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
        Ok(Self {
            id2term: txn.open_table(ID2TERM)?,
            hash2id: txn.open_table(HASH2ID)?,
            graphs: txn.open_table(GRAPHS)?,
            time: txn.open_table(TIME)?,
            app_time: txn.open_table(APP_TIME)?,
            union: open_ro(&txn, UNION)?,
            named: open_ro(&txn, NAMED)?,
            _txn: txn,
        })
    }

    /// Quads matching `[s, p, o]` in the selected graphs. Union quads have graph 0.
    pub fn quads(&self, spo: [Option<Id>; 3], graph: GraphSel) -> QuadIter<'_> {
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
        Ok(self.union[0].get(spo)?.is_some())
    }

    pub fn id(&self, term: &Term) -> Result<Option<Id>> {
        if let Some(id) = term::inline(term) {
            return Ok(Some(id));
        }
        let bytes = term::encode(term);
        let Some(id) = self.hash2id.get(term::hash(&bytes))?.map(|v| v.value()) else { return Ok(None) };
        Ok(Some(id))
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
        self.app_time.range([app, from, 0]..=[app, to, Id::MAX])?.map(|r| {
            let k = r?.0.value();
            Ok((k[1], k[2]))
        }).collect()
    }

    pub fn graphs(&self) -> Result<Vec<(Id, u64)>> {
        self.graphs.iter()?.map(|r| {
            let (k, v) = r?;
            Ok((k.value(), v.value()))
        }).collect()
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
