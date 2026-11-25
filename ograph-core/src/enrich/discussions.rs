//! Discussion threads (Slack or Teams exports). Deterministic layers first: incident references
//! by regex. Then, when a model is configured, classification into the ontology's closed
//! vocabularies. The prompt is generated from the ontology, so prompt and gate cannot disagree.

use super::{Ctx, json_records, parse_time, str_field};
use crate::model::{Batch, Confidence, EntityRef, Extraction, Object, known, lit, obj, slug};
use crate::ontology::Ontology;
use crate::vocab::{RDF_TYPE, disc, evt, ops};
use oxrdf::NamedNode;
use std::collections::BTreeMap;
use std::path::Path;

const UNKNOWN: &str = "UNKNOWN";

pub fn prompt(ont: &Ontology) -> String {
    let mut p = String::from(
        "You classify an operations chat thread. Reply with one JSON object and nothing else:\n\
         {\"category\": <one value>, \"cause_quote\": <the words of the thread that state the cause, copied exactly, or null>, \"sentiment\": <one value>, \"apps\": [<application names the thread is about>], \"calls\": [{\"from\": <application>, \"to\": <application it calls or depends on>, \"quote\": <the words that say so, copied exactly>}], \"actions\": [{\"kind\": <one action value>, \"app\": <the application acted on>, \"message\": <the number n of the [mn] message that reports it>, \"quote\": <the words of that message that say it was done, copied exactly>}], \"confidence\": <0 to 1>}\n\n",
    );
    for (prop, label) in [(disc::CATEGORY, "category"), (disc::SENTIMENT, "sentiment"), (evt::ACTION_KIND, "action")] {
        p.push_str(&format!("Allowed {label} values:\n"));
        for t in ont.vocabulary(prop).unwrap_or_default() {
            match &t.definition {
                Some(d) => p.push_str(&format!("- {}: {d}\n", t.value)),
                None => p.push_str(&format!("- {}\n", t.value)),
            }
        }
        p.push('\n');
    }
    p.push_str(
        "Use only these values. When the thread does not state what caused a problem (a question, a plan, or symptoms only), \
         answer UNKNOWN with cause_quote null; do not guess. Name only applications the thread names. List a call only when \
         the thread says one application calls, connects to or depends on another. List an action only when a message says \
         someone already did it to an application the thread names: a question (\"should we roll back?\"), a proposal, a plan, \
         something not done or something only considered is not an action. Each message starts with its number, such as [m2]. \
         The thread:\n",
    );
    p
}

fn incident_refs() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\b(INC|SEV|PD|IR)-?\d{2,}\b").unwrap())
}

pub fn harvest(path: &Path, key: &str, ctx: &Ctx<'_>) -> anyhow::Result<Batch> {
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
        let mut incidents: Vec<&str> = incident_refs().find_iter(&text).map(|r| r.as_str()).collect();
        for r in &incidents {
            b.add(node.clone(), disc::ABOUT, obj(crate::model::entity("inc", r)), &ev, &rule);
        }
        incidents.sort_by_key(|r| crate::model::slug(r));
        incidents.dedup_by_key(|r| crate::model::slug(r));
        let during = (incidents.len() == 1).then(|| crate::model::entity("inc", incidents[0]));
        texts.push(msgs.iter().enumerate().map(|(n, m)| format!("[m{}] {}", n + 1, m.1)).collect::<Vec<_>>().join("\n"));
        threads_out.push(Thread { node, ev, name, text, msgs, during });
    }
    let Some(llm) = ctx.llm else { return Ok(b) };
    let p = prompt(ctx.ontology);
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
        let Some(v) = crate::llm::first_json(&answer) else {
            b.degraded.push(format!("model answer was not JSON: {}", answer.chars().take(80).collect::<String>()));
            continue;
        };
        let confidence = v.get("confidence").and_then(|c| c.as_f64()).and_then(Confidence::new).unwrap_or(Confidence::new(0.0).unwrap());
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
        for a in v.get("actions").and_then(|x| x.as_array()).into_iter().flatten() {
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
/// thread does not name, or whose words read as a question, a plan or something not done is
/// withheld with the reason. The gateway then checks the kind and the application.
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
    } else if let Some(why) = not_done(said, quote) {
        Some(format!("\"{quote}\" {why}, not a done action"))
    } else {
        CUES.iter().find(|(k, _)| *k == kind).filter(|(_, cues)| !cues.iter().any(|c| quote.to_lowercase().contains(c))).map(|_| format!("\"{quote}\" does not say what a {kind} does"))
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

/// Word stems one of which the quote of an action of each kind must hold, so a quote that only
/// describes symptoms cannot carry an action. A kind outside this table is left to the gateway's
/// vocabulary check.
const CUES: &[(&str, &[&str])] = &[
    ("ROLLBACK", &["roll", "revert", "back to", "downgrad"]),
    ("RESTART", &["restart", "bounce", "reboot", "recycl", "kill", "cycled"]),
    ("SCALE", &["scal", "replica", "instance", "node", "worker", "capacity", "pod"]),
    ("FAILOVER", &["fail", "traffic", "promot", "switch", "drain", "moved", "evacuat"]),
    ("FLAG", &["flag", "flip", "toggl", "enabl", "disabl", "turned off", "turned on"]),
    ("CONFIG", &["config", "setting", "limit", "timeout", "pool", "max", "rais", "lower", "bump", "set ", "revert", "chang"]),
    ("DEPLOY", &["deploy", "rolled out", "shipp", "releas", "hotfix", "push"]),
];

/// Words that make a clause a question, a proposal, a plan or a condition when they come before
/// the action in it.
const NOT_YET: &[&str] = &["should", "shall", "let's", "lets", "could", "would", "can", "will", "i'll", "we'll", "gonna", "going to", "plan", "planning", "want to", "need to", "needs to", "might", "maybe", "about to", "considering", "consider", "suggest", "propose", "if", "unless", "before we", "whether"];
const NEGATIONS: &[&str] = &["not", "didn't", "don't", "won't", "never", "no", "without", "haven't", "hasn't", "wasn't"];

/// Why the clause of `message` that holds `quote` does not report a done action, if it does not:
/// it is a question, a proposal word comes before the quote in it, or a negation comes right
/// before the quote. Clauses end at `. ! ? ; ,` and line breaks.
fn not_done(message: &str, quote: &str) -> Option<&'static str> {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let (msg, q) = (squash(message), squash(quote.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())));
    let start = msg.find(&q)?;
    let end = start + q.len();
    let ends = |c: char| matches!(c, '.' | '!' | '?' | ';' | ',' | '\n');
    let from = msg[..start].rfind(ends).map_or(0, |i| i + 1);
    let to = msg[end..].find(ends).map(|i| end + i);
    if to.is_some_and(|i| msg[i..].starts_with('?')) || q.ends_with('?') {
        return Some("is a question");
    }
    let words = |s: &str| s.split(|c: char| !(c.is_alphanumeric() || c == '\'')).filter(|w| !w.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let before = words(&msg[from..start]);
    let lead = format!(" {} {} ", before.join(" "), words(&q).join(" "));
    if NOT_YET.iter().any(|w| lead.contains(&format!(" {w} "))) {
        return Some("is a proposal or a plan");
    }
    if before.iter().rev().take(3).any(|w| NEGATIONS.contains(&w.as_str())) {
        return Some("is negated");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::not_done;

    #[test]
    fn only_a_done_action_reads_as_done() {
        let cases = [
            ("rolled back checkout to 4.0.9, errors gone", "rolled back checkout", None),
            ("should we roll back checkout?", "roll back checkout", Some("is a question")),
            ("Let's restart payments-api and see", "restart payments-api", Some("is a proposal or a plan")),
            ("I'll scale orders-api after lunch", "scale orders-api", Some("is a proposal or a plan")),
            ("we did not roll back checkout yet", "roll back checkout", Some("is negated")),
            ("restarted payments-api to see if it helps", "restarted payments-api", None),
            ("We rolled back, should we also restart?", "rolled back", None),
        ];
        let got: Vec<Option<&str>> = cases.iter().map(|(m, q, _)| not_done(m, q)).collect();
        assert_eq!(got, cases.iter().map(|c| c.2).collect::<Vec<_>>());
    }
}
