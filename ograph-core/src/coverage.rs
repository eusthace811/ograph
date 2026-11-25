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
    /// Set when the model gave no usable answer: the question is unchecked, not covered.
    pub failed: Option<String>,
    /// Gaps a person has judged and declined; they are neither gaps nor proposals.
    pub declined: Vec<Declined>,
}

/// A term a person decided the ontology will not have, from `.ograph/declined.ttl`:
///
/// ```turtle
/// ops:KubernetesCluster og:declined "We model services, not the clusters under them." ;
///   og:by "dana" ; og:question "Which Kubernetes cluster runs payments-api?" .
/// ```
///
/// It declines the gap it would have been proposed for, under any question, and under each
/// `og:question` a gap whose words contain its words or are contained in them, since the model
/// may quote "cluster" one run and "Kubernetes cluster" the next.
#[derive(Debug, Clone, Serialize)]
pub struct Decline {
    pub term: String,
    pub reason: String,
    pub by: String,
    pub questions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Declined {
    pub need: Need,
    pub term: String,
    pub by: String,
    pub reason: String,
}

const META: &str = "https://ograph.dev/meta#";

/// Parses a declined.ttl. Every declined term must say who declined it.
pub fn declines(ttl: &str) -> anyhow::Result<Vec<Decline>> {
    let mut g = oxrdf::Graph::new();
    for t in oxttl::TurtleParser::new().for_slice(ttl) {
        g.insert(&t?);
    }
    let p = |l: &str| oxrdf::NamedNode::new_unchecked(format!("{META}{l}"));
    let (declined, by, question) = (p("declined"), p("by"), p("question"));
    let text = |s: oxrdf::NamedOrBlankNodeRef<'_>, q: &oxrdf::NamedNode| -> Vec<String> {
        g.objects_for_subject_predicate(s, q).filter_map(|o| match o {
            oxrdf::TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        }).collect()
    };
    let mut out = Vec::new();
    for t in g.triples_for_predicate(&declined) {
        let oxrdf::NamedOrBlankNodeRef::NamedNode(term) = t.subject else { anyhow::bail!("declined.ttl: a declined term must be an IRI") };
        let oxrdf::TermRef::Literal(reason) = t.object else { anyhow::bail!("declined.ttl: og:declined of {term} must be a reason in quotes") };
        let who = text(t.subject, &by);
        anyhow::ensure!(!who.is_empty(), "declined.ttl: {} has no og:by saying who declined it", vocab::compact(term.as_str()));
        out.push(Decline { term: term.as_str().to_owned(), reason: reason.value().to_owned(), by: who.join(", "), questions: text(t.subject, &question) });
    }
    out.sort_by(|a, b| a.term.cmp(&b.term));
    Ok(out)
}

fn words(text: &str) -> std::collections::BTreeSet<String> {
    let mut spaced = String::new();
    for (i, ch) in text.char_indices() {
        if ch.is_ascii_uppercase() && i > 0 && text[..i].chars().last().is_some_and(|p| p.is_ascii_lowercase()) {
            spaced.push(' ');
        }
        spaced.push(ch);
    }
    spaced.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Moves every gap a decline covers from `gaps` to `declined`.
pub fn decline(report: &mut [Coverage], declines: &[Decline]) {
    for c in report {
        let gaps = std::mem::take(&mut c.gaps);
        for need in gaps {
            let found = declines.iter().find(|d| {
                proposed_name(&need).is_some_and(|n| n == d.term) || {
                    let local = d.term.rsplit(['#', '/']).next().unwrap_or_default();
                    let (a, b) = (words(&need.text), words(local));
                    d.questions.contains(&c.question) && !a.is_empty() && !b.is_empty() && (a.is_subset(&b) || b.is_subset(&a))
                }
            });
            match found {
                Some(d) => c.declined.push(Declined { need, term: vocab::compact(&d.term), by: d.by.clone(), reason: d.reason.clone() }),
                None => c.gaps.push(need),
            }
        }
    }
}

/// The IRI `proposals` gives a gap, or `None` for a vocabulary value.
fn proposed_name(need: &Need) -> Option<String> {
    let name = match need.kind.as_str() {
        "value" => return None,
        "property" => camel(&need.text, false),
        _ => camel(&need.text, true),
    };
    (!name.is_empty()).then(|| format!("{}{name}", vocab::ops::NS))
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
         One concept often takes several words, such as a noun and the clause that qualifies it: give them one need with the \
         declared name that represents them together, rather than splitting off a word and leaving it null. A verb or condition \
         that only says how two concepts relate is represented by the property that links them. \
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

/// Models sometimes write a full IRI in angle brackets, or a vocabulary value with its property
/// ("rca:state accepted"); the bare IRI or the value alone is the declared name.
fn declared_form(ont: &Ontology, name: &str) -> String {
    let name = name.trim_start_matches('<').trim_end_matches('>');
    match name.rsplit_once([' ', '=']) {
        Some((_, value)) if !declared(ont, name) && declared(ont, value) => value.to_owned(),
        _ => name.to_owned(),
    }
}

/// `is_entity` says whether words name a known entity (an application in the catalog); such words
/// are data, so they are never a gap even when the model lists them.
pub fn report(ont: &Ontology, llm: &Llm, questions: &[String], is_entity: impl Fn(&str) -> bool) -> Vec<Coverage> {
    let p = prompt(ont);
    questions
        .iter()
        .zip(llm.ask_all(&p, questions))
        .map(|(q, answer)| {
            let mut c = Coverage { question: q.clone(), covered: vec![], gaps: vec![], notes: vec![], failed: None, declined: vec![] };
            let v = match answer.map(|a| first_json(&a)) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    c.failed = Some("model answer was not JSON".into());
                    return c;
                }
                Err(e) => {
                    c.failed = Some(format!("model call failed: {e:#}"));
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
                let need = Need { text, kind: s("kind").unwrap_or_else(|| "class".into()), name: s("name").map(|n| declared_form(ont, &n)) };
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
         # move the ones you accept into ontology.ttl, then run `ograph ontology diff`. To decline one, add\n\
         # `ops:Name og:declined \"why\" ; og:by \"who\" ; og:question \"...\" .` to .ograph/declined.ttl.\n\
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_spellings_of_declared_names() {
        let ont = Ontology::load(crate::ontology::DEFAULT_TTL, crate::ontology::DEFAULT_SHAPES).unwrap();
        assert_eq!(declared_form(&ont, "rca:state accepted"), "accepted");
        assert_eq!(declared_form(&ont, "evt:severity=SEV1"), "SEV1");
        assert_eq!(declared_form(&ont, "<https://ograph.dev/meta#Thing>"), "https://ograph.dev/meta#Thing");
        assert!(declared(&ont, &declared_form(&ont, "<https://ograph.dev/meta#Thing>")));
        assert_eq!(declared_form(&ont, "ops:Cluster name"), "ops:Cluster name");
    }

    const DECLINED: &str = r#"@prefix og: <https://ograph.dev/meta#> .
@prefix ops: <https://ograph.dev/ops#> .
ops:KubernetesCluster og:declined "We model services, not the clusters under them." ;
  og:by "dana" ; og:question "Which Kubernetes cluster runs payments-api?" .
ops:Revenue og:declined "Finance owns it." ; og:by "ana" .
"#;

    fn asked(question: &str, gaps: &[(&str, &str)]) -> Coverage {
        let gaps = gaps.iter().map(|(t, k)| Need { text: (*t).into(), kind: (*k).into(), name: None }).collect();
        Coverage { question: question.into(), covered: vec![], gaps, notes: vec![], failed: None, declined: vec![] }
    }

    #[test]
    fn a_declined_gap_is_reported_with_who_declined_it_and_never_proposed() {
        let mut report = [
            asked("Which Kubernetes cluster runs payments-api?", &[("cluster", "class"), ("runs", "property")]),
            asked("How much revenue was lost during INC-4120?", &[("revenue", "class"), ("lost", "property")]),
            asked("Which clusters are in eu-west-1?", &[("clusters", "class")]),
        ];
        decline(&mut report, &declines(DECLINED).unwrap());
        let gaps: Vec<Vec<&str>> = report.iter().map(|c| c.gaps.iter().map(|g| g.text.as_str()).collect()).collect();
        assert_eq!(gaps, [vec!["runs"], vec!["lost"], vec!["clusters"]]);
        let declined: Vec<(&str, &str, &str)> = report.iter().flat_map(|c| &c.declined).map(|d| (d.need.text.as_str(), d.term.as_str(), d.by.as_str())).collect();
        assert_eq!(declined, [("cluster", "ops:KubernetesCluster", "dana"), ("revenue", "ops:Revenue", "ana")]);
        let ttl = proposals(&report);
        assert!(!ttl.contains("ops:Cluster ") && !ttl.contains("ops:Revenue") && ttl.contains("ops:runs a") && ttl.contains("ops:Clusters a"), "{ttl}");
    }

    #[test]
    fn a_decline_must_say_who_declined_it() {
        let e = declines("@prefix og: <https://ograph.dev/meta#> . <https://ograph.dev/ops#Slo> og:declined \"later\" .").unwrap_err();
        assert_eq!(e.to_string(), "declined.ttl: ops:Slo has no og:by saying who declined it");
    }
}
