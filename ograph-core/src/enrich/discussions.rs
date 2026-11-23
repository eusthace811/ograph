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
    let mut threads: BTreeMap<String, Vec<(i64, String, usize)>> = BTreeMap::new();
    let mut channel_of: BTreeMap<String, String> = BTreeMap::new();
    for (i, m) in json_records(path)?.iter().enumerate() {
        let Some(ts) = str_field(m, &["ts"]).map(str::to_owned).or_else(|| m.get("ts").map(|v| v.to_string())) else { continue };
        let root = str_field(m, &["thread_ts"]).map(str::to_owned).unwrap_or_else(|| ts.clone());
        let Some(t) = m.get("ts").and_then(parse_time) else { continue };
        threads.entry(root.clone()).or_default().push((t, str_field(m, &["text"]).unwrap_or("").to_owned(), i + 1));
        if let Some(c) = str_field(m, &["channel"]) {
            channel_of.insert(root, c.to_owned());
        }
    }
    let mut texts = Vec::new();
    let mut nodes = Vec::new();
    for (root, mut msgs) in threads {
        msgs.sort();
        let node = known("thread", &format!("{}-{}", channel_of.get(&root).map_or("chat", String::as_str), root.replace('.', "-")));
        let ev = b.record(format!("{key}#{}", msgs[0].2));
        let text: String = msgs.iter().map(|m| m.1.as_str()).collect::<Vec<_>>().join("\n");
        b.add(node.clone(), RDF_TYPE, obj(NamedNode::new_unchecked(disc::THREAD)), &ev, &rule);
        b.add(node.clone(), disc::AT, super::dt(msgs[0].0), &ev, &rule);
        b.add(node.clone(), disc::TEXT_HASH, lit(crate::llm::hash(&text)), &ev, &rule);
        if let Some(c) = channel_of.get(&root) {
            b.add(node.clone(), disc::CHANNEL, lit(c.as_str()), &ev, &rule);
        }
        for r in incident_refs().find_iter(&text) {
            b.add(node.clone(), disc::ABOUT, obj(crate::model::entity("inc", r.as_str())), &ev, &rule);
        }
        texts.push(text);
        nodes.push((node, ev));
    }
    let Some(llm) = ctx.llm else { return Ok(b) };
    let p = prompt(ctx.ontology);
    let prompt_hash = crate::llm::hash(&p);
    for (((node, ev), answer), text) in nodes.into_iter().zip(llm.ask_all(&p, &texts)).zip(&texts) {
        let answer = match answer {
            Ok(a) => a,
            Err(e) => {
                b.degraded.push(format!("model call failed for {}: {e:#}", crate::vocab::compact(&format!("{node:?}"))));
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
