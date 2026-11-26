//! Discussion threads (Slack or Teams exports). Deterministic layers first: incident references
//! by regex. Then, when a model is configured, classification into the ontology's closed
//! vocabularies. The prompt is generated from the ontology, so prompt and gate cannot disagree.

use super::{Ctx, json_records, parse_time, str_field};
use crate::gateway::KnownApp;
use crate::model::{Batch, EntityRef, Extraction, Object, known, lit, obj, slug};
use crate::ontology::Ontology;
use crate::vocab::{RDF_TYPE, disc, evt, ops};
use oxrdf::NamedNode;
use std::collections::BTreeMap;
use std::path::Path;

const UNKNOWN: &str = "UNKNOWN";

/// The thread prompt. With `actions` it also asks for what people did, and the messages arrive
/// numbered. Without, it is the prompt from before actions existed, so cached answers hold.
pub fn prompt(ont: &Ontology, actions: bool) -> String {
    let shape = if actions {
        ", \"actions\": [{\"kind\": <one action value>, \"app\": <the application acted on>, \"message\": <the number n of the [mn] message that reports it>, \"quote\": <the words of that message that say it was done, copied exactly>}]"
    } else {
        ""
    };
    let mut p = format!(
        "You classify an operations chat thread. Reply with one JSON object and nothing else:\n\
         {{\"category\": <one value>, \"cause_quote\": <the words of the thread that state the cause, copied exactly, or null>, \"sentiment\": <one value>, \"apps\": [<application names the thread is about>], \"calls\": [{{\"from\": <application>, \"to\": <application it calls or depends on>, \"quote\": <the words that say so, copied exactly>}}]{shape}, \"confidence\": <0 to 1>}}\n\n",
    );
    let all = [(disc::CATEGORY, "category"), (disc::SENTIMENT, "sentiment"), (evt::ACTION_KIND, "action")];
    for &(prop, label) in &all[..if actions { 3 } else { 2 }] {
        p.push_str(&format!("Allowed {label} values:\n"));
        super::push_vocabulary(&mut p, ont, prop);
        p.push('\n');
    }
    p.push_str(
        "Use only these values. When the thread does not state what caused a problem (a question, a plan, or symptoms only), \
         answer UNKNOWN with cause_quote null; do not guess. Name only applications the thread names. List a call only when \
         the thread says one application calls, connects to or depends on another. ",
    );
    if actions {
        p.push_str(
            "List an action only when a message says someone already did it to an application the thread names: a question \
             (\"should we roll back?\"), a proposal, a plan, something not done or something only considered is not an action. \
             Each message starts with its number, such as [m2]. ",
        );
    }
    p.push_str("The thread:\n");
    p
}

/// The thread, then the catalog applications it may name by another name: those with a name or
/// alias that shares a word with it, but none that it holds whole. A whole name already resolves
/// exactly, and listing it led the model to take a plain word (git checkout, an inventory of
/// hosts) for the application (docs/evidence/catalog-slice.txt). The list is per thread, so a
/// catalog change asks the model again only about the threads whose list it changes; a thread
/// with no list is sent as it always was.
fn with_known<'a>(thread: &str, apps: &'a [KnownApp]) -> String {
    let said = words(thread);
    let named = |w: &String| said.iter().any(|t| t == w || (t.len().min(w.len()) >= 4 && (t.starts_with(w.as_str()) || w.starts_with(t.as_str()))));
    let whole = |n: &String| {
        let w = words(n);
        !w.is_empty() && said.windows(w.len()).any(|x| x == w.as_slice())
    };
    let names = |a: &'a KnownApp| std::iter::once(&a.name).chain(&a.aliases);
    let known: Vec<&KnownApp> = apps.iter().filter(|a| !names(a).any(whole) && names(a).flat_map(|n| words(n)).filter(|w| w.len() >= 2 && !crate::gateway::FILLER.contains(&w.as_str()) && w != "db").any(|w| named(&w))).collect();
    if known.is_empty() {
        return thread.to_owned();
    }
    let mut out = format!(
        "{thread}\n\nKnown applications (from the catalog, not part of the thread; listed because a word of their name or alias appears above). \
         When the thread refers to one of them, name it as written before the parentheses. Being listed does not mean the thread is about it:\n"
    );
    for a in known {
        if a.aliases.is_empty() {
            out.push_str(&format!("- {}\n", a.name));
        } else {
            out.push_str(&format!("- {} ({})\n", a.name, a.aliases.join(", ")));
        }
    }
    out
}

use crate::gateway::words;

fn incident_refs() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    // `INC-4388`, `INC4388`, `INC-011F3B`, `INC-2024-001`, in any case. A part has two or more
    // letters and digits with a digit in it, so `SEV-2` is no incident.
    RE.get_or_init(|| regex::Regex::new(r"(?i)\b(?:INC|SEV|PD|IR)(?:\d{2,}|(?:-(?:[A-Z0-9]+\d[A-Z0-9]*|\d[A-Z0-9]+))+)\b").unwrap())
}

/// The incident ids a text names. A severity (`SEV-01`) is none, and `PD`/`IR` count only in
/// capitals, since `pd-2fa` or `ir-4xx` in lowercase are words.
fn incident_ids(text: &str) -> Vec<&str> {
    incident_refs()
        .find_iter(text)
        .map(|m| m.as_str())
        .filter(|id| {
            let (prefix, rest) = id.split_at(id.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(id.len()));
            let rest = rest.trim_start_matches('-');
            let severity = prefix.eq_ignore_ascii_case("sev") && rest.len() <= 2 && rest.bytes().all(|c| c.is_ascii_digit());
            let word = prefix.len() == 2 && prefix != prefix.to_ascii_uppercase();
            !severity && !word
        })
        .collect()
}

pub fn harvest(path: &Path, key: &str, ctx: &Ctx<'_>, actions: bool, known_apps: bool) -> anyhow::Result<Batch> {
    let mut b = Batch::new("discussions", key, "threads-v1");
    let rule = Extraction::Rule { extractor: "thread-regex".into() };
    // A thread is its channel and root timestamp: two channels can share a timestamp. Each
    // message is (time ms, text, record number).
    type Messages = Vec<(i64, String, usize)>;
    let mut threads: BTreeMap<(Option<String>, String), Messages> = BTreeMap::new();
    let ts_text = |m: &serde_json::Value, k: &str| str_field(m, &[k]).map(str::to_owned).or_else(|| m.get(k).filter(|v| v.is_number()).map(|v| v.to_string()));
    for (i, m) in json_records(path)?.iter().enumerate() {
        let Some(ts) = ts_text(m, "ts") else { continue };
        let root = ts_text(m, "thread_ts").unwrap_or(ts);
        let Some(t) = m.get("ts").and_then(parse_time) else { continue };
        let channel = str_field(m, &["channel"]).map(str::to_owned);
        threads.entry((channel, root)).or_default().push((t, str_field(m, &["text"]).unwrap_or("").to_owned(), i + 1));
    }
    let mut texts = Vec::new();
    let mut threads_out = Vec::new();
    for ((channel, root), mut msgs) in threads {
        msgs.sort();
        let name = format!("{}-{}", channel.as_deref().unwrap_or("chat"), root.replace('.', "-"));
        let node = known("thread", &name);
        let ev = b.record(format!("{key}#{}", msgs[0].2));
        let text: String = msgs.iter().map(|m| m.1.as_str()).collect::<Vec<_>>().join("\n");
        b.add(node.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(disc::THREAD)), &ev, &rule);
        b.add(node.clone(), disc::AT, super::dt(msgs[0].0), &ev, &rule);
        b.add(node.clone(), disc::TEXT_HASH, lit(crate::llm::hash(&text)), &ev, &rule);
        if let Some(c) = &channel {
            b.add(node.clone(), disc::CHANNEL, lit(c.as_str()), &ev, &rule);
        }
        for m in &msgs {
            let ev = b.record(format!("{key}#{}", m.2));
            b.add(node.clone(), evt::SOURCE_RECORD, lit(ev.0.as_str()), &ev, &rule);
        }
        let mut incidents: Vec<&str> = incident_ids(&text);
        for r in &incidents {
            b.add(node.clone(), disc::ABOUT, obj(crate::model::entity("inc", r)), &ev, &rule);
        }
        incidents.sort_by_key(|r| crate::model::slug(r));
        incidents.dedup_by_key(|r| crate::model::slug(r));
        let during = (incidents.len() == 1).then(|| crate::model::entity("inc", incidents[0]));
        texts.push(if actions { msgs.iter().enumerate().map(|(n, m)| format!("[m{}] {}", n + 1, m.1)).collect::<Vec<_>>().join("\n") } else if known_apps { with_known(&text, ctx.apps) } else { text.clone() });
        threads_out.push(Thread { node, ev, name, text, msgs, during });
    }
    let Some(llm) = ctx.llm else { return Ok(b) };
    if known_apps && actions {
        b.degraded.push("known_apps = true is used only with actions = false; the threads were sent without the catalog list".into());
    }
    let p = prompt(ctx.ontology, actions);
    let prompt_hash = crate::llm::hash(&p);
    for (th, answer) in threads_out.into_iter().zip(llm.ask_all(&p, &texts)) {
        let Thread { node, ev, name, text, msgs, during } = th;
        let text = text.as_str();
        let answer = match answer {
            Ok(a) => a,
            Err(e) => {
                b.degraded.push(format!("model call failed for thread {name}: {e:#}"));
                continue;
            }
        };
        let v = match super::model_json(&answer) {
            Ok(v) => v,
            Err(note) => {
                b.degraded.push(note);
                continue;
            }
        };
        let confidence = super::stated_confidence(&v);
        let m = Extraction::Llm { model: llm.config.model.clone(), prompt_hash: prompt_hash.clone(), confidence };
        // A cause the model cannot quote from the thread is not stated there, so it abstains. A
        // value outside the vocabulary still goes to the gateway, which quarantines it.
        if let Some(c) = str_field(&v, &["category"]) {
            let stated = str_field(&v, &["cause_quote"]).is_some_and(|q| crate::llm::quoted(text, q));
            let valid = ctx.ontology.vocabulary(disc::CATEGORY).unwrap_or_default().iter().any(|t| t.value == c);
            b.add(node.clone(), disc::CATEGORY, lit(if stated || !valid { c } else { UNKNOWN }), &ev, &m);
        }
        if let Some(val) = str_field(&v, &["sentiment"]) {
            b.add(node.clone(), disc::SENTIMENT, lit(val), &ev, &m);
        }
        let app = |name: &str| EntityRef::Provisional { class: ops::APPLICATION, name: name.to_owned() };
        for c in v.get("calls").and_then(|x| x.as_array()).into_iter().flatten() {
            let (Some(from), Some(to), Some(q)) = (str_field(c, &["from"]), str_field(c, &["to"]), str_field(c, &["quote"])) else { continue };
            if crate::llm::quoted(text, q) && [from, to].iter().all(|n| crate::llm::quoted(q, n)) {
                b.add(app(from), ops::DEPENDS_ON, Object::Entity(app(to)), &ev, &m);
            }
        }
        for a in v.get("apps").and_then(|x| x.as_array()).into_iter().flatten().filter_map(|x| x.as_str()) {
            b.add(node.clone(), disc::ABOUT, Object::Entity(app(a)), &ev, &m);
        }
        for a in v.get("actions").and_then(|x| x.as_array()).into_iter().flatten().filter(|_| actions) {
            action(&mut b, key, a, text, &msgs, during.as_ref(), &m);
        }
    }
    Ok(b)
}

/// A thread as the model reads it, with what its actions need.
struct Thread {
    node: EntityRef,
    ev: crate::model::EvidenceRef,
    name: String,
    text: String,
    msgs: Vec<(i64, String, usize)>,
    during: Option<NamedNode>,
}

/// One action the model reports. The model names the kind, the application, the message and
/// the words; the time is that message's, and the evidence is that message's record. An action
/// whose message does not exist, whose words are not in that message, whose application the
/// thread does not name, whose words read as a question, a plan or something not done, or whose
/// words do not state its kind is withheld with the reason. The gateway then checks the kind and
/// the application.
fn action(b: &mut Batch, key: &str, a: &serde_json::Value, thread: &str, msgs: &[(i64, String, usize)], during: Option<&NamedNode>, m: &Extraction) {
    let kind = str_field(a, &["kind"]).unwrap_or("").to_uppercase();
    let name = str_field(a, &["app"]).unwrap_or("");
    let quote = str_field(a, &["quote"]).unwrap_or("");
    let n = a.get("message").and_then(|x| x.as_u64().or_else(|| x.as_str()?.trim_matches(['[', ']', 'm']).parse().ok()));
    let Some(&(at, ref said, record)) = n.and_then(|n| msgs.get((n as usize).checked_sub(1)?)) else {
        let ev = b.record(format!("{key}#{}", msgs[0].2));
        b.withhold(known("ev", &format!("action/{}/{}", slug(name), slug(&kind))), evt::ACTION_KIND, lit(kind.as_str()), &ev, m, format!("message {} is not in the thread", a.get("message").map_or("(none)".to_owned(), |x| x.to_string())));
        return;
    };
    let ev = b.record(format!("{key}#{record}"));
    let s = known("ev", &format!("action/{}/{}/{at}", slug(name), slug(&kind)));
    let refused = if !crate::llm::quoted(said, quote) {
        Some(format!("\"{quote}\" is not in message {}", n.unwrap()))
    } else if name.is_empty() || !crate::llm::quoted(thread, name) {
        Some(format!("the thread does not name \"{name}\""))
    } else if let Some(why) = not_done(said, quote, &kind) {
        Some(format!("\"{quote}\" {why}, not a done action"))
    } else {
        let q = format!(" {}", quote.to_lowercase().replace(|c: char| !(c.is_alphanumeric() || c == '_'), " "));
        CUES.iter().find(|(k, _)| *k == kind).filter(|(_, cues)| !cues.iter().any(|c| q.contains(&format!(" {c}")))).map(|_| format!("\"{quote}\" does not say what a {kind} does"))
    };
    if let Some(reason) = refused {
        b.withhold(s, evt::ACTION_KIND, lit(kind.as_str()), &ev, m, reason);
        return;
    }
    let e = super::Event { class: evt::OPERATOR_ACTION, on: name, at, signal: "OPERATOR_ACTION", severity: "SEV4", value: None };
    super::event(b, &s, e, [(evt::ACTION_KIND, lit(kind.as_str())), (evt::SUMMARY, lit(quote))], &ev, m);
    if let Some(inc) = during {
        b.add(s, evt::DURING, obj(inc.clone()), &ev, m);
    }
}

/// Per kind, word stems one of which must start a word of an action's quote, so a quote that
/// only describes symptoms cannot carry an action. A kind outside this table is left to the
/// gateway's vocabulary check.
const CUES: &[(&str, &[&str])] = &[
    ("ROLLBACK", &["roll", "revert", "back to", "downgrad"]),
    ("RESTART", &["restart", "bounce", "reboot", "recycl", "kill", "cycled"]),
    ("SCALE", &["scal", "replica", "instance", "node", "worker", "capacity", "pod"]),
    ("FAILOVER", &["fail", "traffic", "promot", "switch", "drain", "moved", "evacuat"]),
    ("FLAG", &["flag", "flip", "toggl", "enabl", "disabl", "turned off", "turned on"]),
    ("CONFIG", &["config", "setting", "limit", "timeout", "pool", "max", "rais", "lower", "bump", "set", "revert", "chang"]),
    ("DEPLOY", &["deploy", "rolled out", "shipp", "releas", "hotfix", "push"]),
];

/// Words that make a clause a question, a proposal, a plan or a condition when they come before
/// the action in it.
const NOT_YET: &[&str] = &["should", "shall", "let's", "lets", "could", "would", "can", "will", "i'll", "we'll", "gonna", "going to", "plan", "planning", "want to", "need to", "needs to", "might", "maybe", "about to", "considering", "consider", "suggest", "propose", "if", "unless", "before we", "whether"];
/// Words that open a question about what follows them.
const ASKS: &[&str] = &["did", "do", "does", "has", "have", "had", "is", "are", "was", "were", "should", "shall", "can", "could", "will", "would", "anyone", "someone", "who"];
/// Who an asking word asks about.
const SUBJECTS: &[&str] = &["we", "you", "i", "they", "he", "she", "it", "anyone", "someone", "somebody", "anybody"];
/// Words that join what an action was done for, or a next step, to the action.
const LINKS: &[&str] = &["so", "because", "until", "and", "then", "but", "while", "since", "once"];
/// Negations besides any `n't` contraction, with the contractions chat leaves unapostrophised.
const NEGATIONS: &[&str] = &["not", "never", "no", "without", "cannot", "dont", "didnt", "doesnt", "wont", "cant", "couldnt", "wouldnt", "shouldnt", "isnt", "arent", "wasnt", "werent", "hasnt", "havent", "hadnt"];

/// Whether `words[i]` is the `that` of "now that", which gives a reason as `because` does.
fn now_that(words: &[String], i: usize) -> bool {
    words[i] == "that" && i > 0 && words[i - 1] == "now"
}

/// Why the clause of `message` that holds `quote` does not report a done action of `kind`, if it
/// does not: it is a question, a proposal word comes before the action in it, or a negation comes
/// right before the action. The action runs from the quote's first cue of the kind to its last cue
/// before a linking word such as `so` or `and`, so what the quote says after it ("restarted X so we
/// can see") is no plan; for a kind with no cues it is the quote's end. Clauses end at `. ! ? ; ,`
/// and line breaks.
fn not_done(message: &str, quote: &str, kind: &str) -> Option<&'static str> {
    // Squashed line by line, so a line break still ends a clause; the quote may span lines.
    let msg = message.lines().map(crate::llm::squash).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    let q = crate::llm::squash_quote(quote);
    let start = msg.replace('\n', " ").find(&q)?;
    let end = start + q.len();
    let ends = |c: char| matches!(c, '.' | '!' | '?' | ';' | ',' | '\n');
    let from = msg[..start].rfind(ends).map_or(0, |i| i + 1);
    // Chat apps type `’` for an apostrophe.
    let words = |s: &str| s.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '\u{2019}')).filter(|w| !w.is_empty()).map(|w| w.replace('\u{2019}', "'")).collect::<Vec<_>>();
    let before = words(&msg[from..start]);
    // A `.` inside a word (`2.3`, `v1.4`) ends no clause.
    let next_end = |stop: &dyn Fn(char) -> bool| msg[end..].char_indices().find(|&(i, c)| stop(c) && (c != '.' || msg[end + i + 1..].chars().next().is_none_or(char::is_whitespace))).map(|(i, _)| end + i);
    let to = next_end(&ends);
    // A sentence that opens as a question asks about the quote past any comma ("did anyone
    // restart X, or should I?").
    let sentence_end = next_end(&|c| matches!(c, '.' | '!' | '?' | '\n'));
    let reason = before.iter().rposition(|w| matches!(w.as_str(), "so" | "because" | "since" | "after" | "when" | "that")).map_or(0, |i| i + 1);
    // It opens as one when an asking word comes first with a subject after it ("did we", "has
    // anyone") or is itself the subject ("anyone"); "Had to restart X, anyone know why?" drops
    // its subject and reports.
    let opens_question = match (before.get(reason).map(String::as_str), before.get(reason + 1).map(String::as_str)) {
        (Some("anyone" | "someone" | "who"), _) => true,
        (Some(w), Some(subject)) => ASKS.contains(&w) && SUBJECTS.contains(&subject),
        _ => false,
    };
    if opens_question && sentence_end.is_some_and(|i| msg[i..].starts_with('?')) {
        return Some("is a question");
    }
    if q.ends_with('?') || to.is_some_and(|i| msg[i..].starts_with('?')) {
        // A dash or a bracket before the `?` opens an aside that asks its own question
        // ("rolled back X - can someone verify?"), unless the clause opened as one ("did we
        // restart X (the eu one)?").
        let tail = &msg[end..to.unwrap_or(msg.len())];
        let aside = tail.contains(['(', '\u{2013}', '\u{2014}']) || tail.contains(" - ");
        // A question about what followed a past action reports the action ("did latency recover
        // after we rolled back X?"); "is it true that we rolled back X?" and "will it recover
        // after we restart X?" still ask.
        // The tense is the verb's, which comes by the action's first cue word, not a later name's
        // ("restart shared-cache") or adjective's ("restart the failed pods").
        let quoted: Vec<String> = words(&q).iter().map(|w| w.to_lowercase()).collect();
        let cues = CUES.iter().find(|(k, _)| *k == kind).map_or(&[][..], |(_, c)| c);
        let first = quoted.iter().position(|w| cues.iter().any(|c| c.split(' ').next().is_some_and(|stem| w.starts_with(stem))));
        let past = quoted[..first.map_or(quoted.len(), |i| i + 1)].iter().any(|w| w.ends_with("ed"));
        let follows = before.iter().enumerate().rposition(|(i, w)| matches!(w.as_str(), "after" | "since" | "when") || now_that(&before, i)).map_or(0, |i| i + 1);
        let reports = past && follows > 0 && !before[follows..].iter().any(|w| ASKS.contains(&w.as_str()));
        if q.ends_with('?') || !reports && (!aside || before.iter().any(|w| ASKS.contains(&w.as_str()))) {
            return Some("is a question");
        }
    }
    let mut said = words(&q);
    // The quote's words before its action, which a negation may be among.
    let mut lead_in = 0;
    if let Some((_, cues)) = CUES.iter().find(|(k, _)| *k == kind) {
        let lower: Vec<String> = said.iter().map(|w| w.to_lowercase()).collect();
        let cue_at = |i: usize| cues.iter().any(|c| c.split(' ').enumerate().all(|(k, stem)| lower.get(i + k).is_some_and(|w| w.starts_with(stem))));
        if let Some(first) = (0..lower.len()).find(|&i| cue_at(i)) {
            let link = (first + 1..lower.len()).find(|&i| LINKS.contains(&lower[i].as_str())).unwrap_or(lower.len());
            let last = (first..link).rev().find(|&i| cue_at(i)).unwrap_or(first);
            said.truncate(last + 1);
            lead_in = first;
        }
    }
    // What comes before `so` or `because` is the reason ("it would not recover so we restarted
    // X"), as is what comes before "now that" ("should be fine now that we rolled back X"), and
    // "can confirm" reports; none makes the action a plan. Nothing else cuts it: "we
    // should check logs and restart X", "I suggest that we restart X" and "I'll post when we
    // roll back X" still plan.
    let cause = before.iter().enumerate().rposition(|(i, w)| matches!(w.as_str(), "so" | "because" | "since") || now_that(&before, i)).map_or(0, |i| i + 1);
    let lead = format!(" {} {} ", before[cause..].join(" "), said.join(" ")).replace(" can confirm ", " confirm ").replace(" can see ", " see ");
    if NOT_YET.iter().any(|w| lead.contains(&format!(" {w} "))) {
        return Some("is a proposal or a plan");
    }
    // A negation before a linking word ("no change so restarted X") belongs to an earlier part.
    let tail = before.iter().rposition(|w| LINKS.contains(&w.as_str())).map_or(0, |i| i + 1);
    if before[tail..].iter().chain(&said[..lead_in]).rev().take(3).any(|w| w.ends_with("n't") || NEGATIONS.contains(&w.as_str())) {
        return Some("is negated");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{not_done, with_known};
    use crate::gateway::KnownApp;

    #[test]
    fn a_thread_carries_the_applications_it_shares_a_word_with() {
        let app = |n: &str, a: &[&str]| KnownApp { name: n.into(), aliases: a.iter().map(|x| x.to_string()).collect() };
        let apps = [app("auth-service", &[]), app("checkout", &[]), app("ledger-db", &["ledger-pg"]), app("notifications", &[]), app("payments-api", &["pay-gw"])];
        let listed = |t: &str| with_known(t, &apps).lines().filter_map(|l| l.strip_prefix("- ")).map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(listed("auth svc and pay-gw both down"), ["auth-service"]);
        assert_eq!(listed("git checkout fails, the ledger is full"), ["ledger-db (ledger-pg)"]);
        assert_eq!(listed("notif queue is deep, the ledger is full"), ["ledger-db (ledger-pg)", "notifications"]);
        assert_eq!(listed("the api service db is slow"), Vec::<String>::new());
        assert_eq!(with_known("CardRail is down", &apps), "CardRail is down");
    }

    #[test]
    fn incident_ids_are_read_in_the_forms_a_store_holds() {
        let refs = |t: &str| super::incident_ids(t).into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(refs("rolled back for INC-011F3B"), ["INC-011F3B"]);
        assert_eq!(refs("INC-2024-001 is open, see inc-4388 and INC4120"), ["INC-2024-001", "inc-4388", "INC4120"]);
        // A severity is no incident, and neither is a lowercase word like `pd-2fa`.
        assert_eq!(refs("SEV1 paged, then SEV-2, then SEV-01"), Vec::<String>::new());
        assert_eq!(refs("INC-4388 declared SEV-01; pd-2fa and ir-4xx are not ids"), ["INC-4388"]);
    }

    #[test]
    fn only_a_done_action_reads_as_done() {
        let cases = [
            ("rolled back checkout to 4.0.9, errors gone", "rolled back checkout", None),
            ("should we roll back checkout?", "roll back checkout", Some("is a question")),
            ("Let's restart payments-api and see", "restart payments-api", Some("is a proposal or a plan")),
            ("I'll scale orders-api after lunch", "scale orders-api", Some("is a proposal or a plan")),
            ("we did not roll back checkout yet", "roll back checkout", Some("is negated")),
            ("restarted payments-api to see if it helps", "restarted payments-api", None),
            ("restarted payments-api so we can see if the leak returns", "restarted payments-api so we can see if the leak returns", None),
            ("rolled back checkout to 4.0.9 and will keep watching", "rolled back checkout to 4.0.9 and will keep watching", None),
            ("I'll restart payments-api so it picks up the fix", "I'll restart payments-api so it picks up the fix", Some("is a proposal or a plan")),
            ("pool will be raised to 50", "pool will be raised to 50", Some("is a proposal or a plan")),
            ("we did not roll back checkout yet", "did not roll back checkout", Some("is negated")),
            ("never restarted payments-api", "never restarted payments-api", Some("is negated")),
            ("we didn\u{2019}t roll back checkout", "roll back checkout", Some("is negated")),
            ("we won\u{2019}t restart payments-api", "restart payments-api", Some("is negated")),
            ("Let\u{2019}s restart payments-api and see", "restart payments-api", Some("is a proposal or a plan")),
            ("I\u{2019}ll scale orders-api after lunch", "scale orders-api", Some("is a proposal or a plan")),
            ("we couldn't roll back checkout", "roll back checkout", Some("is negated")),
            ("we can't restart payments-api", "restart payments-api", Some("is negated")),
            ("we didnt roll back checkout", "roll back checkout", Some("is negated")),
            ("we weren't able to restart payments-api", "restart payments-api", Some("is negated")),
            ("No change so restarted payments-api", "restarted payments-api", None),
            ("Rolled back checkout to 4.0.9 - can someone verify?", "Rolled back checkout to 4.0.9", None),
            ("restarted payments-api (can someone double check?)", "restarted payments-api", None),
            ("restarted payments-api \u{2014} errors gone?", "restarted payments-api", None),
            ("did we restart checkout-service (eu)?", "restart checkout-service", Some("is a question")),
            ("has anyone restarted ledger-db (the primary)?", "restarted ledger-db", Some("is a question")),
            ("did someone roll back payments - the 2.3 deploy?", "roll back payments", Some("is a question")),
            ("did anyone restart payments-api, or should I?", "restart payments-api", Some("is a question")),
            ("did we roll back checkout to 4.0.9, or just restart it?", "roll back checkout to 4.0.9", Some("is a question")),
            ("should be fine now that we rolled back checkout", "rolled back checkout", None),
            ("errors should drop now that we restarted payments-api", "restarted payments-api", None),
            ("did latency recover after we rolled back checkout?", "rolled back checkout", None),
            ("is it better now that we restarted payments-api?", "restarted payments-api", None),
            ("any change since we restarted payments-api?", "restarted payments-api", None),
            ("is it true that we rolled back checkout?", "rolled back checkout", Some("is a question")),
            ("will it recover after we restart payments-api?", "restart payments-api", Some("is a question")),
            ("when did we restart payments-api?", "restart payments-api", Some("is a question")),
            ("I suggest that we restart payments-api", "restart payments-api", Some("is a proposal or a plan")),
            ("what happens when we restart shared-cache?", "restart shared-cache", Some("is a question")),
            ("what happens when we restart the failed pods?", "restart the failed pods", Some("is a question")),
            ("did errors stop after we added two replicas to orders-api?", "added two replicas to orders-api", None),
            ("did latency drop after we increased the timeout on checkout?", "increased the timeout on checkout", None),
            ("what happens when we restart the killed workers?", "restart the killed workers", Some("is a question")),
            ("what happens when we deploy the released build of checkout?", "deploy the released build of checkout", Some("is a question")),
            ("we have restarted payments-api, errors gone?", "restarted payments-api", None),
            ("Had to restart payments-api, anyone know why it leaked?", "restart payments-api", None),
            ("Have restarted payments-api, errors gone?", "restarted payments-api", None),
            ("Was able to roll back checkout, can someone verify?", "roll back checkout", None),
            ("we restarted payments-api, is it better?", "restarted payments-api", None),
            ("has it recovered after we restarted payments-api, or still flapping?", "restarted payments-api", None),
            ("I suggest that we restart payments-api", "restart payments-api", Some("is a proposal or a plan")),
            ("I'll post an update when we roll back checkout", "roll back checkout", Some("is a proposal or a plan")),
            ("we'll know more after we restart payments-api", "restart payments-api", Some("is a proposal or a plan")),
            ("is it fixed now that we restarted payments-api, or still erroring?", "restarted payments-api", None),
            ("was it you who restarted payments-api, or the autoscaler?", "restarted payments-api", Some("is a question")),
            ("payments-api would not recover so we restarted payments-api", "restarted payments-api", None),
            ("checkout could not reach ledger-db so we rolled back checkout", "rolled back checkout", None),
            ("can confirm we rolled back checkout", "rolled back checkout", None),
            ("we should check logs and restart payments-api", "restart payments-api", Some("is a proposal or a plan")),
            ("failed over payments so it can stop failing", "failed over payments so it can stop failing", None),
            ("restarted payments-api so we can kill the stuck worker", "restarted payments-api so we can kill the stuck worker", None),
            ("We rolled back, should we also restart?", "rolled back", None),
            ("should we wait\nrolled back checkout", "rolled back checkout", None),
            ("we rolled back\n  checkout to 4.0.9", "rolled back checkout", None),
        ];
        let kind = |q: &str| ["roll", "restart", "scale", "failed over", "deploy"].iter().zip(["ROLLBACK", "RESTART", "SCALE", "FAILOVER", "DEPLOY"]).find(|(c, _)| q.to_lowercase().contains(*c)).map_or("CONFIG", |x| x.1);
        let got: Vec<Option<&str>> = cases.iter().map(|(m, q, _)| not_done(m, q, kind(q))).collect();
        assert_eq!(got, cases.iter().map(|c| c.2).collect::<Vec<_>>());
    }
}
