use oxrdf::vocab::xsd;
use oxrdf::{BlankNode, GraphName, Literal, NamedNode, Quad, Term};
use oxsdatatypes::{DateTime, Double};
use redb::{
    Database, ReadOnlyTable, ReadTransaction, ReadableDatabase, ReadableTable,
    ReadableTableMetadata, TableDefinition,
};
use spareval::{ExpressionTerm, InternalQuad, QueryableDataset};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

type Id = u64;
pub type Result<T> = std::result::Result<T, redb::Error>;

const EXTRA: Id = 1 << 63;
const STR2ID: TableDefinition<&[u8], Id> = TableDefinition::new("str2id");
const ID2STR: TableDefinition<Id, &[u8]> = TableDefinition::new("id2str");
const TIME: TableDefinition<[Id; 2], ()> = TableDefinition::new("time");
const DEFAULT: [TableDefinition<[Id; 3], ()>; 3] = [
    TableDefinition::new("dspo"),
    TableDefinition::new("dpos"),
    TableDefinition::new("dosp"),
];
const IN_GRAPH: [TableDefinition<[Id; 4], ()>; 3] = [
    TableDefinition::new("gspo"),
    TableDefinition::new("gpos"),
    TableDefinition::new("gosp"),
];
const ANY_GRAPH: [TableDefinition<[Id; 4], ()>; 3] = [
    TableDefinition::new("spog"),
    TableDefinition::new("posg"),
    TableDefinition::new("ospg"),
];
// Positions in a quad: S=0, P=1, O=2, G=3. Each ordering lists the positions in key order.
const DEFAULT_ORDER: [[usize; 3]; 3] = [[0, 1, 2], [1, 2, 0], [2, 0, 1]];
const IN_GRAPH_ORDER: [[usize; 4]; 3] = [[3, 0, 1, 2], [3, 1, 2, 0], [3, 2, 0, 1]];
const ANY_GRAPH_ORDER: [[usize; 4]; 3] = [[0, 1, 2, 3], [1, 2, 0, 3], [2, 0, 1, 3]];

pub const AT: &str = "http://ops.example/at";

pub fn encode(term: &Term) -> Vec<u8> {
    let (tag, parts): (u8, [&str; 2]) = match term {
        Term::NamedNode(n) => (1, ["", n.as_str()]),
        Term::BlankNode(b) => (2, ["", b.as_str()]),
        Term::Literal(l) => match l.language() {
            Some(lang) => (4, [lang, l.value()]),
            None if l.datatype() == xsd::STRING => (3, ["", l.value()]),
            None => (5, [l.datatype().as_str(), l.value()]),
        },
    };
    let mut out = vec![tag];
    if tag >= 4 {
        out.extend_from_slice(parts[0].as_bytes());
        out.push(0);
    }
    out.extend_from_slice(parts[1].as_bytes());
    out
}

pub fn decode(bytes: &[u8]) -> Term {
    let s = std::str::from_utf8(&bytes[1..]).expect("dictionary holds UTF-8");
    let split = || s.split_once('\0').expect("tagged literal has a separator");
    match bytes[0] {
        1 => NamedNode::new_unchecked(s).into(),
        2 => BlankNode::new_unchecked(s).into(),
        3 => Literal::new_simple_literal(s).into(),
        4 => {
            let (lang, value) = split();
            Literal::new_language_tagged_literal_unchecked(value, lang).into()
        }
        _ => {
            let (datatype, value) = split();
            Literal::new_typed_literal(value, NamedNode::new_unchecked(datatype)).into()
        }
    }
}

pub fn date_time_millis(term: &Term) -> Option<u64> {
    let Term::Literal(l) = term else { return None };
    if l.datatype() != xsd::DATE_TIME {
        return None;
    }
    let epoch = DateTime::from_str("1970-01-01T00:00:00Z").unwrap();
    let seconds = DateTime::from_str(l.value()).ok()?.checked_sub(epoch)?.as_seconds();
    Some((f64::from(Double::from(seconds)) * 1000.0).round() as u64)
}

fn sorted_insert<const N: usize>(
    w: &redb::WriteTransaction,
    def: TableDefinition<[Id; N], ()>,
    mut keys: Vec<[Id; N]>,
) -> Result<()> {
    keys.sort_unstable();
    keys.dedup();
    let mut table = w.open_table(def)?;
    for k in keys {
        table.insert(k, ())?;
    }
    Ok(())
}

pub fn bulk_load(path: &Path, quads: &[Quad]) -> Result<Database> {
    let _ = std::fs::remove_file(path);
    let db = Database::create(path)?;
    let clock = std::time::Instant::now();
    let lap = |what: &str| eprintln!("load {what}: {:.0} ms", clock.elapsed().as_secs_f64() * 1e3);
    let mut dict: HashMap<Vec<u8>, Id> = HashMap::new();
    let mut by_id: Vec<Vec<u8>> = Vec::new();
    let mut intern = |t: Term| -> Id {
        let bytes = encode(&t);
        *dict.entry(bytes).or_insert_with_key(|b| {
            by_id.push(b.clone());
            by_id.len() as Id
        })
    };
    let at = intern(NamedNode::new_unchecked(AT).into());
    let mut default: [Vec<[Id; 3]>; 3] = Default::default();
    let mut named: [Vec<[Id; 4]>; 6] = Default::default();
    let mut time = Vec::new();
    for q in quads {
        let millis = (q.predicate.as_str() == AT).then(|| date_time_millis(&q.object)).flatten();
        let s = intern(q.subject.clone().into());
        let p = intern(q.predicate.clone().into());
        let o = intern(q.object.clone());
        if let Some(ms) = millis.filter(|_| p == at) {
            time.push([ms, s]);
        }
        match &q.graph_name {
            GraphName::DefaultGraph => {
                let spo = [s, p, o];
                for (i, order) in DEFAULT_ORDER.iter().enumerate() {
                    default[i].push(order.map(|pos| spo[pos]));
                }
            }
            GraphName::NamedNode(g) => {
                let spog = [s, p, o, intern(g.clone().into())];
                for (i, order) in IN_GRAPH_ORDER.iter().chain(&ANY_GRAPH_ORDER).enumerate() {
                    named[i].push(order.map(|pos| spog[pos]));
                }
            }
            GraphName::BlankNode(_) => unimplemented!("blank node graph names"),
        }
    }
    lap("encode + intern + permute in memory");
    let w = db.begin_write()?;
    {
        let mut entries: Vec<(&[u8], Id)> = dict.iter().map(|(k, v)| (k.as_slice(), *v)).collect();
        entries.sort_unstable();
        let mut str2id = w.open_table(STR2ID)?;
        for (k, v) in entries {
            str2id.insert(k, v)?;
        }
        let mut id2str = w.open_table(ID2STR)?;
        for (i, bytes) in by_id.iter().enumerate() {
            id2str.insert(i as Id + 1, bytes.as_slice())?;
        }
    }
    lap("dictionary tables written");
    for (def, keys) in DEFAULT.into_iter().zip(default) {
        sorted_insert(&w, def, keys)?;
    }
    for (def, keys) in IN_GRAPH.into_iter().chain(ANY_GRAPH).zip(named) {
        sorted_insert(&w, def, keys)?;
    }
    sorted_insert(&w, TIME, time)?;
    lap("quad and time indexes sorted and written");
    w.commit()?;
    lap("committed");
    Ok(db)
}

pub struct Tables {
    _txn: ReadTransaction,
    str2id: ReadOnlyTable<&'static [u8], Id>,
    id2str: ReadOnlyTable<Id, &'static [u8]>,
    default: [ReadOnlyTable<[Id; 3], ()>; 3],
    in_graph: [ReadOnlyTable<[Id; 4], ()>; 3],
    any_graph: [ReadOnlyTable<[Id; 4], ()>; 3],
    time: ReadOnlyTable<[Id; 2], ()>,
}

type QuadIter<'a> = Box<dyn Iterator<Item = Result<[Id; 4]>> + 'a>;

/// Picks the ordering whose leading positions are exactly the bound ones, then range-scans it.
fn scan<'a, const N: usize>(
    tables: &'a [ReadOnlyTable<[Id; N], ()>; 3],
    orders: &[[usize; N]; 3],
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

fn open3<const N: usize>(
    txn: &ReadTransaction,
    [a, b, c]: [TableDefinition<[Id; N], ()>; 3],
) -> Result<[ReadOnlyTable<[Id; N], ()>; 3]> {
    Ok([txn.open_table(a)?, txn.open_table(b)?, txn.open_table(c)?])
}

impl Tables {
    pub fn open(db: &Database) -> Result<Self> {
        let txn = db.begin_read()?;
        Ok(Self {
            str2id: txn.open_table(STR2ID)?,
            id2str: txn.open_table(ID2STR)?,
            default: open3(&txn, DEFAULT)?,
            in_graph: open3(&txn, IN_GRAPH)?,
            any_graph: open3(&txn, ANY_GRAPH)?,
            time: txn.open_table(TIME)?,
            _txn: txn,
        })
    }

    /// `graph`: `Some(None)` is the default graph, `Some(Some(g))` one named graph, `None` any named graph.
    pub fn quads(&self, spo: [Option<Id>; 3], graph: Option<Option<Id>>) -> QuadIter<'_> {
        let pattern = [spo[0], spo[1], spo[2], graph.flatten()];
        if pattern.iter().flatten().any(|id| id & EXTRA != 0) {
            return Box::new(std::iter::empty());
        }
        match graph {
            Some(None) => scan(&self.default, &DEFAULT_ORDER, pattern),
            Some(Some(_)) => scan(&self.in_graph, &IN_GRAPH_ORDER, pattern),
            None => scan(&self.any_graph, &ANY_GRAPH_ORDER, pattern),
        }
    }

    pub fn id(&self, term: &Term) -> Result<Option<Id>> {
        Ok(self.str2id.get(encode(term).as_slice())?.map(|g| g.value()))
    }

    pub fn term(&self, id: Id) -> Result<Term> {
        let bytes = self.id2str.get(id)?.ok_or_else(|| {
            redb::Error::Corrupted(format!("id {id} missing from the dictionary"))
        })?;
        Ok(decode(bytes.value()))
    }

    /// Default-graph matches of `(s, p, o)` with one position unbound, returning that position.
    pub fn lookup(&self, spo: [Option<Id>; 3]) -> Result<Vec<Id>> {
        let free = spo.iter().position(Option::is_none).expect("one free position");
        self.quads(spo, Some(None)).map(|q| q.map(|q| q[free])).collect()
    }

    /// Per table: name, entries, leaf bytes, and branch plus fragmented bytes.
    pub fn stats(&self) -> Result<Vec<(String, u64, u64, u64)>> {
        let mut tables: Vec<(&str, &dyn ReadableTableMetadata)> =
            vec![("str2id", &self.str2id), ("id2str", &self.id2str), ("time", &self.time)];
        let names = ["dspo", "dpos", "dosp"].into_iter().zip(&self.default);
        tables.extend(names.map(|(n, t)| (n, t as &dyn ReadableTableMetadata)));
        tables
            .into_iter()
            .map(|(name, t)| {
                let s = t.stats()?;
                Ok((name.to_owned(), t.len()?, s.stored_bytes(), s.metadata_bytes() + s.fragmented_bytes()))
            })
            .collect()
    }

    pub fn events_between(&self, from_ms: u64, to_ms: u64) -> Result<Vec<Id>> {
        let range = self.time.range([from_ms, 0]..=[to_ms, Id::MAX])?;
        range.map(|r| Ok(r?.0.value()[1])).collect()
    }
}

/// One query's view of the store: the shared read snapshot plus ephemeral query-time terms.
pub struct View<'a> {
    tables: &'a Tables,
    extra: RefCell<(Vec<Term>, HashMap<Term, Id>)>,
    memo: Option<RefCell<HashMap<Id, ExpressionTerm>>>,
}

impl<'a> View<'a> {
    pub fn new(tables: &'a Tables, memoize: bool) -> Self {
        Self { tables, extra: RefCell::default(), memo: memoize.then(RefCell::default) }
    }
}

impl<'a> QueryableDataset<'a> for View<'a> {
    type InternalTerm = Id;
    type Error = redb::Error;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&Id>,
        predicate: Option<&Id>,
        object: Option<&Id>,
        graph_name: Option<Option<&Id>>,
    ) -> impl Iterator<Item = Result<InternalQuad<Id>>> + use<'a> {
        let spo = [subject.copied(), predicate.copied(), object.copied()];
        let tables: &'a Tables = self.tables;
        tables.quads(spo, graph_name.map(|g| g.copied())).map(|q| {
            q.map(|[s, p, o, g]| InternalQuad {
                subject: s,
                predicate: p,
                object: o,
                graph_name: (g != 0).then_some(g),
            })
        })
    }

    fn internalize_term(&self, term: Term) -> Result<Id> {
        if let Some(id) = self.tables.id(&term)? {
            return Ok(id);
        }
        let (terms, ids) = &mut *self.extra.borrow_mut();
        Ok(*ids.entry(term).or_insert_with_key(|t| {
            terms.push(t.clone());
            EXTRA | (terms.len() as Id - 1)
        }))
    }

    fn externalize_expression_term(&self, id: Id) -> Result<ExpressionTerm> {
        let Some(memo) = &self.memo else { return Ok(self.externalize_term(id)?.into()) };
        if let Some(t) = memo.borrow().get(&id) {
            return Ok(t.clone());
        }
        let t = ExpressionTerm::from(self.externalize_term(id)?);
        memo.borrow_mut().insert(id, t.clone());
        Ok(t)
    }

    fn externalize_term(&self, id: Id) -> Result<Term> {
        if id & EXTRA != 0 {
            return Ok(self.extra.borrow().0[(id & !EXTRA) as usize].clone());
        }
        self.tables.term(id)
    }
}
