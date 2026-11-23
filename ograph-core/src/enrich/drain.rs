//! Drain log templating, copied from ogrep-core (MIT, same authors).

use std::collections::HashMap;

/// Share of token positions that must agree for a line to join a template.
pub const SIMILARITY: f64 = 0.5;
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
    pub count: usize,
    pub first_seen: Option<i64>,
    pub last_seen: Option<i64>,
    /// Where the first line of this template was seen: `(file, line)`.
    pub example: (String, usize),
}

impl Template {
    pub fn text(&self) -> String {
        let shown: Vec<&str> = self.tokens.iter().map(Token::as_str).collect();
        shown.join(" ")
    }

    /// The runs of constant tokens between the variable ones, in order.
    pub fn constant_runs(&self) -> Vec<Vec<&str>> {
        self.tokens
            .split(|t| !matches!(t, Token::Word(_)))
            .filter(|run| !run.is_empty())
            .map(|run| run.iter().map(Token::as_str).collect())
            .collect()
    }

    /// The longest run of constant tokens, the part most likely to be written
    /// literally in the code that logs it.
    pub fn constant_part(&self) -> String {
        let len = |run: &[Token]| run.iter().map(|t| t.as_str().len() + 1).sum::<usize>();
        let mut best: &[Token] = &[];
        for run in self.tokens.split(|t| !matches!(t, Token::Word(_))) {
            if len(run) > len(best) {
                best = run;
            }
        }
        let words: Vec<&str> = best.iter().map(Token::as_str).collect();
        words.join(" ")
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
    match b.get(end) {
        Some(b'Z') => end += 1,
        Some(&c) if (c == b'+' || c == b'-') && b.len() >= end + 6 && b[end + 3] == b':' => {
            if let (Some(oh), Some(om)) = (digits(end + 1..end + 3), digits(end + 4..end + 6)) {
                offset = (oh * 3600 + om * 60) * if c == b'+' { 1 } else { -1 };
                end += 6;
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
    let t = token.trim_matches(|c: char| ",;:()[]{}\"'".contains(c));
    if t.is_empty() {
        return false;
    }
    let digits = t.bytes().filter(u8::is_ascii_digit).count();
    // Numbers, ids with digits, hex, durations (12ms), IPs, UUIDs.
    digits > 0 && (digits * 2 >= t.len() || t.contains('-') && t.len() >= 16 || t.starts_with("0x"))
        || (t.len() >= 16 && t.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// Groups lines into templates.
#[derive(Default)]
pub struct Drain {
    /// Templates by length, then by first token as shown.
    groups: HashMap<usize, HashMap<String, Vec<usize>>>,
    pub templates: Vec<Template>,
    /// Each template's tokens as ids from `vocab`, `MASKED` or `GENERALISED`, so matching
    /// compares integers instead of strings.
    ids: Vec<Vec<u32>>,
    /// Template indexes by length, ascending.
    by_len: HashMap<usize, Vec<usize>>,
    vocab: HashMap<String, u32>,
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
    pub fn add(&mut self, file: &str, line_number: usize, line: &str) {
        let (time, rest) = leading_timestamp(line);
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
        let Some(first) = tokens.first().map(LineToken::as_str) else {
            return;
        };
        let line: Vec<u32> = tokens
            .iter()
            .map(|t| match t {
                LineToken::Masked => MASKED,
                LineToken::Word(w) => self.word_id(w),
            })
            .collect();
        let by_first = self.groups.entry(tokens.len()).or_default();
        if !by_first.contains_key(first) {
            by_first.insert(first.to_string(), Vec::new());
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
            t.count += 1;
            if let Some(time) = time {
                t.first_seen = Some(t.first_seen.map_or(time, |f| f.min(time)));
                t.last_seen = Some(t.last_seen.map_or(time, |l| l.max(time)));
            }
            return;
        }
        slots.push(self.templates.len());
        self.by_len.entry(tokens.len()).or_default().push(self.templates.len());
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
        self.templates.push(Template {
            tokens: tokens.into_iter().map(LineToken::to_owned).collect(),
            count: 1,
            first_seen: time,
            last_seen: time,
            example: (file.to_string(), line_number),
        });
    }

    fn word_id(&self, w: &str) -> u32 {
        self.vocab.get(w).copied().unwrap_or(UNKNOWN)
    }

    /// The template of `line`'s length whose words agree with the line's at the most
    /// positions; ties go to the later template, and 0 when none has that length.
    pub fn most_words_in_common(&self, line: &str) -> usize {
        let words: Vec<u32> = line.split_whitespace().map(|w| self.word_id(w)).collect();
        let Some(same_len) = self.by_len.get(&words.len()) else {
            return 0;
        };
        same_len
            .iter()
            .max_by_key(|&&i| {
                self.ids[i]
                    .iter()
                    .zip(&words)
                    .filter(|(a, b)| a == b && **b != UNKNOWN)
                    .count()
            })
            .map_or(0, |&i| i)
    }

    /// Templates, most frequent first; ties by first sighting, then text.
    pub fn sorted(mut self) -> Vec<Template> {
        self.templates
            .sort_by_cached_key(|t| (std::cmp::Reverse(t.count), t.first_seen, t.text()));
        self.templates
    }
}

