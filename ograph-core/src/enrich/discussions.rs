//! Discussion threads (Slack or Teams exports). Deterministic layers first: incident references
//! by regex. Then, when a model is configured, classification into the ontology's closed
//! vocabularies. The prompt is generated from the ontology, so prompt and gate cannot disagree.

use super::{Ctx, json_records, parse_time, str_field};
use crate::model::{Batch, Confidence, EntityRef, Extraction, Object, known, lit, obj};
use crate::ontology::Ontology;
use crate::vocab::{RDF_TYPE, disc, ops};
use oxrdf::NamedNode;
use std::collections::BTreeMap;
use std::path::Path;

const UNKNOWN: &str = "UNKNOWN";

pub fn prompt(ont: &Ontology) -> String {
    let mut p = String::from(
        "You classify an operations chat thread. Reply with one JSON object and nothing else:\n\
         {\"category\": <one value>, \"cause_quote\": <the words of the thread that state the cause, copied exactly, or null>, \"sentiment\": <one value>, \"apps\": [<application names the thread is about>], \"calls\": [{\"from\": <application>, \"to\": <application it calls or depends on>, \"quote\": <the words that say so, copied exactly>}], \"confidence\": <0 to 1>}\n\n",
    );
    for (prop, label) in [(disc::CATEGORY, "category"), (disc::SENTIMENT, "sentiment")] {
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
         the thread says one application calls, connects to or depends on another. The thread:\n",
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
    let mut nodes = Vec::new();
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
        for r in incident_refs().find_iter(&text) {
            b.add(node.clone(), disc::ABOUT, obj(crate::model::entity("inc", r.as_str())), &ev, &rule);
        }
        texts.push(text);
        nodes.push((node, ev, name));
    }
    let Some(llm) = ctx.llm else { return Ok(b) };
    let p = prompt(ctx.ontology);
    let prompt_hash = crate::llm::hash(&p);
    for (((node, ev, name), answer), text) in nodes.into_iter().zip(llm.ask_all(&p, &texts)).zip(&texts) {
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
    }
    Ok(b)
}
