//! `ograph ontology coverage`: which parts of a set of competency questions the ontology cannot
//! represent. A model maps each question's concepts to declared names; code keeps a mapping only
//! when the name is declared and the words are in the question. What is left becomes proposed
//! Turtle for a person to edit into the ontology, checked afterwards with `ograph ontology diff`.

use crate::llm::{Llm, first_json, quoted};
use crate::ontology::{Ontology, Range};
use crate::vocab;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Need {
    pub text: String,
    pub kind: String,
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Coverage {
    pub question: String,
    pub covered: Vec<Need>,
    pub gaps: Vec<Need>,
    pub notes: Vec<String>,
}

fn expand(name: &str) -> String {
    match name.split_once(':') {
        Some((p, local)) => vocab::PREFIXES.iter().find(|(q, _)| *q == p).map_or_else(|| name.to_owned(), |(_, ns)| format!("{ns}{local}")),
        None => name.to_owned(),
    }
}

pub fn prompt(ont: &Ontology) -> String {
    let comment = |iri: &str| {
        let s = oxrdf::NamedNode::new_unchecked(iri);
        match ont.graph.object_for_subject_predicate(&s, oxrdf::vocab::rdfs::COMMENT) {
            Some(oxrdf::TermRef::Literal(l)) => format!(": {}", l.value()),
            _ => String::new(),
        }
    };
    let mut p = String::from(
        "You check whether an operations knowledge graph's ontology can represent a question. The ontology declares:\n\nClasses:\n",
    );
    for c in ont.classes.keys() {
        p.push_str(&format!("- {}{}\n", vocab::compact(c), comment(c)));
    }
    p.push_str("\nProperties (domain -> range):\n");
    for (iri, prop) in &ont.properties {
        let range = match &prop.range {
            Some(Range::Class(c) | Range::Datatype(c)) => vocab::compact(c),
            None => "?".into(),
        };
        p.push_str(&format!("- {} ({} -> {range}){}\n", vocab::compact(iri), prop.domain.as_deref().map_or("?".into(), vocab::compact), comment(iri)));
    }
    p.push_str("\nClosed vocabularies:\n");
    for (prop, terms) in &ont.vocabularies {
        p.push_str(&format!("- {}: {}\n", vocab::compact(prop), terms.iter().map(|t| t.value.as_str()).collect::<Vec<_>>().join(", ")));
    }
    p.push_str(
        "\nList every concept the question needs: kinds of things, relationships, attributes and categorical values. \
         Skip words that only name one specific thing (an application name, an incident id, a time); those are data. \
         Words of time or order (before, during, last day, this month) are comparisons on evt:at and need no name. \
         Reply with one JSON object and nothing else:\n\
         {\"needs\": [{\"text\": <the words of the question, copied exactly>, \"kind\": \"class\" | \"property\" | \"value\", \"name\": <the declared class, property or vocabulary value that represents it, or null>}]}\n\
         Use null when nothing declared represents the concept; do not stretch a name to fit. The question:\n",
    );
    p
}

/// Whether the ontology declares `name` as a class, a property or a closed-vocabulary value.
fn declared(ont: &Ontology, name: &str) -> bool {
    let iri = expand(name);
    ont.is_class(&iri) || ont.property(&iri).is_some() || ont.vocabularies.values().flatten().any(|t| t.value == name)
}

/// `is_entity` says whether words name a known entity (an application in the catalog); such words
/// are data, so they are never a gap even when the model lists them.
pub fn report(ont: &Ontology, llm: &Llm, questions: &[String], is_entity: impl Fn(&str) -> bool) -> Vec<Coverage> {
    let p = prompt(ont);
    questions
        .iter()
        .zip(llm.ask_all(&p, questions))
        .map(|(q, answer)| {
            let mut c = Coverage { question: q.clone(), covered: vec![], gaps: vec![], notes: vec![] };
            let v = match answer.map(|a| first_json(&a)) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    c.notes.push("model answer was not JSON".into());
                    return c;
                }
                Err(e) => {
                    c.notes.push(format!("model call failed: {e:#}"));
                    return c;
                }
            };
            for n in v.get("needs").and_then(|x| x.as_array()).into_iter().flatten() {
                let s = |k: &str| n.get(k).and_then(|x| x.as_str()).map(str::to_owned);
                let Some(text) = s("text") else { continue };
                if !quoted(q, &text) {
                    c.notes.push(format!("dropped \"{text}\": not in the question"));
                    continue;
                }
                let need = Need { text, kind: s("kind").unwrap_or_else(|| "class".into()), name: s("name") };
                match &need.name {
                    Some(name) if declared(ont, name) => c.covered.push(need),
                    Some(name) => {
                        c.notes.push(format!("\"{}\" was mapped to {name}, which the ontology does not declare", need.text));
                        c.gaps.push(Need { name: None, ..need });
                    }
                    None if is_entity(&need.text) => c.notes.push(format!("\"{}\" names a catalog entity, which is data", need.text)),
                    None => c.gaps.push(need),
                }
            }
            c
        })
        .collect()
}

fn camel(text: &str, upper_first: bool) -> String {
    let mut out = String::new();
    for (i, w) in text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).enumerate() {
        let w = w.to_lowercase();
        let mut cs = w.chars();
        if let Some(f) = cs.next() {
            if i == 0 && !upper_first {
                out.push(f);
            } else {
                out.push(f.to_ascii_uppercase());
            }
            out.extend(cs);
        }
    }
    out
}

/// Proposed additions as Turtle, one term per distinct gap, each commented with the questions
/// that need it. Nothing here is loaded until a person edits it into the ontology.
pub fn proposals(report: &[Coverage]) -> String {
    let mut terms: std::collections::BTreeMap<(String, String), Vec<&str>> = std::collections::BTreeMap::new();
    for c in report {
        for g in &c.gaps {
            terms.entry((g.kind.clone(), g.text.to_lowercase())).or_default().push(&c.question);
        }
    }
    let mut out = String::from(
        "# Proposed by `ograph ontology coverage`. Review each term, give it a domain, range or vocabulary,\n\
         # move the ones you accept into ontology.ttl, then run `ograph ontology diff`.\n\
         @prefix ops: <https://ograph.dev/ops#> .\n@prefix owl: <http://www.w3.org/2002/07/owl#> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\n",
    );
    for ((kind, text), qs) in terms {
        let (name, ty) = match kind.as_str() {
            "property" => (camel(&text, false), "owl:DatatypeProperty"),
            "value" => {
                out.push_str(&format!("# value \"{text}\" for a closed vocabulary, needed by: {}\n\n", qs.join(" | ")));
                continue;
            }
            _ => (camel(&text, true), "owl:Class"),
        };
        if name.is_empty() {
            continue;
        }
        let why = format!("Proposed: needed by {}", qs.iter().map(|q| format!("\"{q}\"")).collect::<Vec<_>>().join(", "));
        out.push_str(&format!("ops:{name} a {ty} ;\n  rdfs:comment {} .\n\n", serde_json::to_string(&why).unwrap_or_default()));
    }
    out
}
