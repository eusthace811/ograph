//! Runbooks and other prose (Markdown or text). The only facts taken from prose are other names
//! for an application, and only where the text states one outright: "payments-api, also known
//! as pay-gw", "search-api (aka finder)", "ledger-db (ledger-pg)". No model reads prose: on the
//! demo runbooks a quote-checked model found no alias these phrases missed and added a false one
//! (`ledger-db (postgres)`). The first name must resolve against the catalog at the gateway, and
//! an alias that already names another application is quarantined there.

use crate::model::{Batch, EntityRef, Extraction, lit};
use crate::vocab::ops;
use std::path::Path;

const NAME: &str = r"[A-Za-z][A-Za-z0-9_.-]*[A-Za-z0-9]";

fn cued() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        let cue = r"(?:also known as|aka|a\.k\.a\.?|formerly(?: known as| called)?|previously(?: known as| called)?|also called|known internally as)";
        regex::Regex::new(&format!(r#"(?i)\b({NAME}),?\s+(?:is\s+)?(?:\(\s*)?{cue}:?\s+["'`“‘]?({NAME})"#)).unwrap()
    })
}

/// `name (other-name)` with no cue word: the parenthesis must hold a kebab or snake identifier
/// whose last part is not a number, so `checkout (INC-4388)` and `ledger-db (postgres)` are not
/// read as aliases. It must also share a part with the name, so `ledger-db (ledger-pg)` is one
/// and `ledger-db (read-only)` is not.
fn bare() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(&format!(r"\b({NAME})\s+\(([A-Za-z][A-Za-z0-9]*(?:[-_][A-Za-z0-9]+)*[-_][A-Za-z][A-Za-z0-9]*)\)")).unwrap())
}

/// Words that follow a cue as a preposition, as in "is also called by checkout".
const PREPOSITIONS: [&str; 9] = ["by", "from", "on", "in", "at", "to", "for", "with", "during"];

fn shares_part(a: &str, b: &str) -> bool {
    let parts = |s: &str| s.to_lowercase().split(['-', '_', '.']).map(str::to_owned).collect::<Vec<_>>();
    let a = parts(a);
    parts(b).iter().any(|p| a.contains(p))
}

pub fn harvest(path: &Path, key: &str) -> anyhow::Result<Batch> {
    let text = super::read_text(path)?;
    let mut b = Batch::new("docs", key, "aliases-v1");
    let rule = Extraction::Rule { extractor: "alias-phrases".into() };
    let app = |name: &str| EntityRef::Provisional { class: ops::APPLICATION, name: name.to_owned() };
    let mut seen = std::collections::HashSet::new();
    for (n, line) in text.lines().enumerate() {
        let cued = cued().captures_iter(line).filter(|c| !PREPOSITIONS.contains(&c[2].to_lowercase().as_str()));
        let bare = bare().captures_iter(line).filter(|c| shares_part(&c[1], &c[2]));
        for c in cued.chain(bare) {
            let (main, alias) = (&c[1], &c[2]);
            if !main.eq_ignore_ascii_case(alias) && seen.insert((main.to_lowercase(), alias.to_lowercase())) {
                let ev = b.record(format!("{key}:{}", n + 1));
                b.add(app(main), ops::ALIAS, lit(alias), &ev, &rule);
            }
        }
    }
    Ok(b)
}
