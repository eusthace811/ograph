//! TriG export of every named graph: the per-harvest archive Netflix found useful for debugging.

use crate::store::{GraphSel, Snapshot};
use oxrdf::{GraphName, NamedOrBlankNode, Quad, Term};
use std::io::BufWriter;
use std::path::Path;

pub fn trig(snap: &Snapshot, path: &Path) -> anyhow::Result<usize> {
    let mut ser = oxttl::TriGSerializer::new();
    for (p, ns) in crate::vocab::PREFIXES {
        ser = ser.with_prefix(*p, *ns)?;
    }
    let mut w = ser.for_writer(BufWriter::new(std::fs::File::create(path)?));
    let mut n = 0;
    for (g, _) in snap.graphs()? {
        let Term::NamedNode(gn) = snap.term(g)? else { continue };
        for q in snap.quads([None, None, None], GraphSel::Named(g)) {
            let [s, p, o, _] = q?;
            let subject: NamedOrBlankNode = match snap.term(s)? {
                Term::NamedNode(x) => x.into(),
                Term::BlankNode(x) => x.into(),
                _ => continue,
            };
            let Term::NamedNode(pred) = snap.term(p)? else { continue };
            w.serialize_quad(&Quad::new(subject, pred, snap.term(o)?, GraphName::NamedNode(gn.clone())))?;
            n += 1;
        }
    }
    w.finish()?;
    Ok(n)
}
