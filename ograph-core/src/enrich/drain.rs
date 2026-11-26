//! Drain log templating, copied from ogrep-core (MIT, same authors).

use rustc_hash::FxHashMap;

/// Share of token positions that must agree for a line to join a template.
const SIMILARITY: f64 = 0.5;
/// How masked and generalised positions are shown.
const WILDCARD: &str = "<*>";

/// One position of a template.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Word(String),
    /// A variable token (a number, an id), masked up front. Masked tokens are
    /// equal to each other, like Drain3's `<NUM>`/`<IP>` masks.
    Masked,
    /// A position the template generalised by merging lines that differ
    /// there. It counts for neither side of a comparison, as Drain's
    /// parameter does; a line never has one.
    Generalised,
}

impl Token {
    fn as_str(&self) -> &str {
        match self {
            Token::Word(w) => w,
            Token::Masked | Token::Generalised => WILDCARD,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub tokens: Vec<Token>,
}

impl Template {
    pub fn text(&self) -> String {
        let shown: Vec<&str> = self.tokens.iter().map(Token::as_str).collect();
        shown.join(" ")
    }
}

/// Seconds since the epoch for a leading ISO 8601 timestamp, and the rest of
/// the line after it.
pub fn leading_timestamp(line: &str) -> (Option<i64>, &str) {
    let s = line.trim_start_matches(['[', ' ']);
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return (None, line);
    }
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) = (
        digits(0..4),
        digits(5..7),
        digits(8..10),
        digits(11..13),
        digits(14..16),
        digits(17..19),
    ) else {
        return (None, line);
    };
    let mut end = 19;
    if b.get(end) == Some(&b'.') || b.get(end) == Some(&b',') {
        end += 1;
        while b.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
    }
    let mut offset = 0i64;
    // `17:30:00 +0530 INFO`: after one space only a full offset that ends the line or comes
    // before a capitalised word (a zone or a level), so `-1000 rows dropped` stays message text.
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let full = if (2..6).all(|k| digit(end + k)) {
        Some(end + 6)
    } else if digit(end + 2) && digit(end + 3) && b.get(end + 4) == Some(&b':') && digit(end + 5) && digit(end + 6) {
        Some(end + 7)
    } else {
        None
    };
    let then = |i: usize| match b.get(i) {
        None | Some(b']') => true,
        Some(b' ') => b.get(i + 1).is_some_and(|c| c.is_ascii_uppercase() || *c == b'['),
        _ => false,
    };
    if b.get(end) == Some(&b' ') && matches!(b.get(end + 1), Some(b'+' | b'-')) && full.is_some_and(then) {
        end += 1;
    }
    match b.get(end) {
        Some(b'Z') => end += 1,
        // +HH:MM, +HHMM (logback's Z) or +HH.
        Some(&c) if c == b'+' || c == b'-' => {
            let sign = if c == b'+' { 1 } else { -1 };
            // Only an offset that exists: `-2024 rows` is message text.
            if let Some(oh) = digits(end + 1..end + 3).filter(|&h| h <= 14) {
                let colon = b.get(end + 3) == Some(&b':');
                let m = end + 3 + usize::from(colon);
                match digits(m..m + 2) {
                    Some(om) if om < 60 => (offset, end) = ((oh * 3600 + om * 60) * sign, m + 2),
                    Some(_) => {}
                    None if !colon => (offset, end) = (oh * 3600 * sign, end + 3),
                    None => {}
                }
            }
        }
        _ => {}
    }
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - offset;
    let rest = s[end..].trim_start_matches([']', ' ']);
    (Some(secs), rest)
}

fn is_variable(token: &str) -> bool {
    let value = token.rsplit_once('=').map_or(token, |(_, v)| v);
    let t = value.trim_matches(|c: char| ",;:()[]{}\"'".contains(c));
    if t.is_empty() {
        return false;
    }
    let digits = t.bytes().filter(u8::is_ascii_digit).count();
    // Numbers, ids with digits, hex, durations (12ms), IPs, UUIDs, and opaque upper-case ids
    // such as product SKUs (L9ECAV7KIM), which carry too few digits for the first rule.
    digits > 0 && (digits * 2 >= t.len() || t.contains('-') && t.len() >= 16 || t.starts_with("0x"))
        || (t.len() >= 16 && t.bytes().all(|c| c.is_ascii_hexdigit()))
        || (digits > 0 && t.len() >= 8 && t.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

/// Groups lines into templates.
#[derive(Default)]
pub struct Drain {
    /// Templates by length, then by first token as shown.
    groups: FxHashMap<usize, FxHashMap<String, Vec<usize>>>,
    pub templates: Vec<Template>,
    /// Each template's tokens as ids from `vocab`, `MASKED` or `GENERALISED`, so matching
    /// compares integers instead of strings.
    ids: Vec<Vec<u32>>,
    vocab: FxHashMap<String, u32>,
}

const MASKED: u32 = u32::MAX;
const GENERALISED: u32 = u32::MAX - 1;
/// A word no template has, so it matches nothing.
const UNKNOWN: u32 = u32::MAX - 2;

/// A line's token, borrowed from the line until it starts a template.
#[derive(Clone, Copy)]
enum LineToken<'a> {
    Word(&'a str),
    Masked,
}

impl LineToken<'_> {
    fn as_str(&self) -> &str {
        match self {
            LineToken::Word(w) => w,
            LineToken::Masked => WILDCARD,
        }
    }

    fn to_owned(self) -> Token {
        match self {
            LineToken::Word(w) => Token::Word(w.to_string()),
            LineToken::Masked => Token::Masked,
        }
    }
}

impl Drain {
    /// Adds a line and returns the index of the template it joined or created, or `None` for
    /// a blank line.
    pub fn add(&mut self, line: &str) -> Option<usize> {
        let (_, rest) = leading_timestamp(line);
        let tokens: Vec<LineToken> = rest
            .split_whitespace()
            .map(|t| {
                // A literal `<*>` prints like a masked value, so it is one.
                if is_variable(t) || t == WILDCARD {
                    LineToken::Masked
                } else {
                    LineToken::Word(t)
                }
            })
            .collect();
        let first = tokens.first()?.as_str();
        let line: Vec<u32> = tokens
            .iter()
            .map(|t| match t {
                LineToken::Masked => MASKED,
                LineToken::Word(w) => self.word_id(w),
            })
            .collect();
        let by_first = self.groups.entry(tokens.len()).or_default();
        if !by_first.contains_key(first) {
            by_first.insert(first.to_owned(), Vec::new());
        }
        let slots = by_first.get_mut(first).expect("just inserted");
        // As in Drain3: similarity counts equal tokens (a line has no
        // generalised position, so those count for neither side), and the
        // most similar template wins, then the more general one.
        let best = slots
            .iter()
            .filter_map(|&i| {
                let t = &self.ids[i];
                let same = t.iter().zip(&line).filter(|(a, b)| a == b).count();
                (same as f64 / tokens.len() as f64 >= SIMILARITY).then(|| {
                    let wild = t.iter().filter(|a| **a == GENERALISED).count();
                    (same, wild, i)
                })
            })
            .max_by_key(|&(same, wild, i)| (same, wild, std::cmp::Reverse(i)));
        if let Some((_, _, i)) = best {
            let t = &mut self.templates[i];
            for ((a, id), b) in t.tokens.iter_mut().zip(&mut self.ids[i]).zip(&line) {
                if id != b {
                    *a = Token::Generalised;
                    *id = GENERALISED;
                }
            }
            return Some(i);
        }
        slots.push(self.templates.len());
        let ids = tokens
            .iter()
            .map(|t| match t {
                LineToken::Masked => MASKED,
                LineToken::Word(w) => {
                    let next = self.vocab.len() as u32;
                    *self.vocab.entry((*w).to_owned()).or_insert(next)
                }
            })
            .collect();
        self.ids.push(ids);
        self.templates.push(Template { tokens: tokens.into_iter().map(LineToken::to_owned).collect() });
        Some(self.templates.len() - 1)
    }

    fn word_id(&self, w: &str) -> u32 {
        self.vocab.get(w).copied().unwrap_or(UNKNOWN)
    }
}

#[cfg(test)]
mod tests {
    use super::leading_timestamp;

    #[test]
    fn a_leading_timestamp_takes_its_offset_in_any_iso_form() {
        let utc = leading_timestamp("2024-03-01T12:00:00Z msg").0;
        assert_eq!(utc, Some(1_709_294_400));
        for line in ["2024-03-01T17:30:00+05:30 msg", "2024-03-01T17:30:00+0530 msg", "[2024-03-01 17:30:00.123+0530] msg"] {
            assert_eq!(leading_timestamp(line), (utc, "msg"), "{line}");
        }
        assert_eq!(leading_timestamp("2024-03-01T07:00:00-05 msg"), (utc, "msg"));
        assert_eq!(leading_timestamp("2024-03-01T12:00:00 -request msg"), (utc, "-request msg"));
        // After a space only a full offset counts: a count or a size is message text.
        assert_eq!(leading_timestamp("2024-03-01 17:30:00 +0530 INFO msg"), (utc, "INFO msg"));
        assert_eq!(leading_timestamp("2024-03-01 07:00:00 -05:00"), (utc, ""));
        assert_eq!(leading_timestamp("2024-03-01 12:00:00 -1000 rows dropped"), (utc, "-1000 rows dropped"));
        assert_eq!(leading_timestamp("[2024-03-01 17:30:00 +0530] INFO msg"), (utc, "INFO msg"));
        assert_eq!(leading_timestamp("2024-03-01 12:00:00 -10 items"), (utc, "-10 items"));
        assert_eq!(leading_timestamp("2024-03-01 12:00:00 -12345 bytes"), (utc, "-12345 bytes"));
        assert_eq!(leading_timestamp("2024-03-01 12:00:00 -2024 rows"), (utc, "-2024 rows"));
        assert_eq!(leading_timestamp("2024-03-01T12:00:00+9999 msg"), (utc, "+9999 msg"));
    }
}
