//! Term ids. A term is one `u64`. Small canonical literals live inside the id, everything else
//! goes through the dictionary.
//!
//! | bits 63 | 62..60 | 59..0 |
//! |---|---|---|
//! | ephemeral flag | kind | payload |
//!
//! An inline id is only produced for a literal whose lexical form is the canonical one, so
//! `u64` equality stays RDF term equality: the same term always maps to the same id.

use oxrdf::vocab::xsd;
use oxrdf::{BlankNode, Literal, NamedNode, Term};
use oxsdatatypes::{Boolean, DateTime, Decimal, Integer};
use std::str::FromStr;
use std::sync::OnceLock;

pub type Id = u64;

pub const EPHEMERAL: Id = 1 << 63;
const KIND_SHIFT: u32 = 60;
const PAYLOAD: Id = (1 << KIND_SHIFT) - 1;
const INT_BIAS: i64 = 1 << 59;
const TIME_BIAS: i64 = 1 << 58;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    Dict = 0,
    Integer = 1,
    DateTime = 2,
    Boolean = 3,
}

pub fn kind(id: Id) -> Kind {
    match (id >> KIND_SHIFT) & 0b111 {
        0 => Kind::Dict,
        1 => Kind::Integer,
        2 => Kind::DateTime,
        3 => Kind::Boolean,
        k => unreachable!("kind {k} is never written"),
    }
}

fn pack(kind: Kind, payload: u64) -> Id {
    ((kind as Id) << KIND_SHIFT) | (payload & PAYLOAD)
}

pub fn is_dict(id: Id) -> bool {
    id & EPHEMERAL == 0 && kind(id) == Kind::Dict
}

/// Milliseconds since the Unix epoch, for an inline dateTime id.
pub fn millis(id: Id) -> Option<i64> {
    (id & EPHEMERAL == 0 && kind(id) == Kind::DateTime).then(|| (id & PAYLOAD) as i64 - TIME_BIAS)
}

pub fn time_id(millis: i64) -> Id {
    pack(Kind::DateTime, (millis + TIME_BIAS) as u64)
}

fn epoch_value() -> i128 {
    static EPOCH: OnceLock<i128> = OnceLock::new();
    *EPOCH.get_or_init(|| {
        let bytes = DateTime::from_str("1970-01-01T00:00:00Z").unwrap().to_be_bytes();
        i128::from_be_bytes(bytes[0..16].try_into().unwrap())
    })
}

/// Builds a UTC dateTime from milliseconds without parsing. `Decimal` is an i128 scaled by 10^18.
pub fn date_time_from_millis(millis: i64) -> DateTime {
    let value = epoch_value() + i128::from(millis) * 1_000_000_000_000_000;
    let mut bytes = [0; 18];
    bytes[0..16].copy_from_slice(&value.to_be_bytes());
    bytes[16..18].copy_from_slice(&0i16.to_be_bytes());
    DateTime::from_be_bytes(bytes)
}

pub fn millis_of(dt: DateTime) -> Option<i64> {
    let bytes = dt.to_be_bytes();
    if bytes[16..18] != 0i16.to_be_bytes() {
        return None;
    }
    let delta = i128::from_be_bytes(bytes[0..16].try_into().unwrap()) - epoch_value();
    if delta % 1_000_000_000_000_000 != 0 {
        return None;
    }
    i64::try_from(delta / 1_000_000_000_000_000).ok().filter(|m| m.unsigned_abs() < TIME_BIAS as u64)
}

pub fn parse_millis(lexical: &str) -> Option<i64> {
    millis_of(DateTime::from_str(lexical).ok()?)
}

pub fn format_millis(millis: i64) -> String {
    date_time_from_millis(millis).to_string()
}

/// The inline id of a term, when it has one.
pub fn inline(term: &Term) -> Option<Id> {
    let Term::Literal(l) = term else { return None };
    let dt = l.datatype();
    let value = l.value();
    if dt == xsd::INTEGER {
        let v = i64::from_str(value).ok().filter(|v| v.unsigned_abs() < INT_BIAS as u64)?;
        (v.to_string() == value).then(|| pack(Kind::Integer, (v + INT_BIAS) as u64))
    } else if dt == xsd::DATE_TIME {
        let m = parse_millis(value)?;
        (format_millis(m) == value).then(|| time_id(m))
    } else if dt == xsd::BOOLEAN {
        match value {
            "true" => Some(pack(Kind::Boolean, 1)),
            "false" => Some(pack(Kind::Boolean, 0)),
            _ => None,
        }
    } else {
        None
    }
}

/// The term of an inline id.
pub fn inline_term(id: Id) -> Option<Term> {
    let payload = id & PAYLOAD;
    Some(match kind(id) {
        Kind::Dict => return None,
        Kind::Integer => Literal::from(payload as i64 - INT_BIAS).into(),
        Kind::DateTime => Literal::new_typed_literal(format_millis(payload as i64 - TIME_BIAS), xsd::DATE_TIME).into(),
        Kind::Boolean => Literal::from(payload == 1).into(),
    })
}

pub fn inline_expression(id: Id) -> Option<spareval::ExpressionTerm> {
    use spareval::ExpressionTerm as E;
    let payload = id & PAYLOAD;
    Some(match kind(id) {
        Kind::Dict => return None,
        Kind::Integer => E::IntegerLiteral(Integer::from(payload as i64 - INT_BIAS)),
        Kind::DateTime => E::DateTimeLiteral(date_time_from_millis(payload as i64 - TIME_BIAS)),
        Kind::Boolean => E::BooleanLiteral(Boolean::from(payload == 1)),
    })
}

pub fn decimal(v: f64) -> Literal {
    let text = format!("{:.4}", if v.is_finite() { v } else { 0.0 });
    let text = text.trim_end_matches('0').trim_end_matches('.');
    let text = if text.is_empty() || text == "-" { "0" } else { text };
    let _ = Decimal::default();
    Literal::new_typed_literal(text, xsd::DECIMAL)
}

/// Dictionary encoding: one tag byte, then the payload.
pub fn encode(term: &Term) -> Vec<u8> {
    let (tag, a, b): (u8, &str, &str) = match term {
        Term::NamedNode(n) => (1, "", n.as_str()),
        Term::BlankNode(n) => (2, "", n.as_str()),
        Term::Literal(l) => match l.language() {
            Some(lang) => (4, lang, l.value()),
            None if l.datatype() == xsd::STRING => (3, "", l.value()),
            None => (5, l.datatype().as_str(), l.value()),
        },
    };
    let mut out = Vec::with_capacity(1 + a.len() + b.len() + 1);
    out.push(tag);
    if tag >= 4 {
        out.extend_from_slice(a.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(b.as_bytes());
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

pub fn hash(bytes: &[u8]) -> u128 {
    use std::hash::Hasher;
    let mut h = siphasher::sip128::SipHasher13::new_with_keys(0x6f67_7261_7068, 0x6964_7321);
    h.write(bytes);
    siphasher::sip128::Hasher128::finish128(&h).as_u128()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_times_round_trip_through_inline_ids() {
        let lex = "2026-11-22T21:37:05.123Z";
        let term: Term = Literal::new_typed_literal(lex, xsd::DATE_TIME).into();
        let id = inline(&term).expect("canonical UTC dateTime is inlined");
        assert_eq!(inline_term(id), Some(term));
        assert_eq!(millis(id), Some(1_795_383_425_123));
        let spareval::ExpressionTerm::DateTimeLiteral(dt) = inline_expression(id).unwrap() else { panic!() };
        assert_eq!(dt, DateTime::from_str(lex).unwrap());
    }

    #[test]
    fn non_canonical_literals_stay_in_the_dictionary() {
        for (lex, dt) in [("007", xsd::INTEGER), ("2026-11-22T21:37:05.000Z", xsd::DATE_TIME), ("2026-11-22T21:37:05+01:00", xsd::DATE_TIME), ("1", xsd::BOOLEAN)] {
            assert_eq!(inline(&Literal::new_typed_literal(lex, dt).into()), None, "{lex}");
        }
        assert_eq!(inline_term(inline(&Literal::from(-42).into()).unwrap()), Some(Literal::from(-42).into()));
    }
}
