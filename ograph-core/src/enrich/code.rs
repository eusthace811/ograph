//! Code layer: the commits to each service of a repository, as change events. ogrep decides
//! which commit belongs to which service; ograph keeps the commit id, its subject and the files
//! it changed, so a chain can end at a commit and a file.

use super::{Event, event, ogrep, rel};
use crate::model::{Batch, Extraction, known, lit, slug};
use crate::vocab::evt;
use std::path::Path;

/// A commit that touches hundreds of files is a merge or a reformat; its first files are
/// enough to tell which.
const MAX_FILES: usize = 50;

pub fn harvest(root: &Path, path: &Path, since: &str) -> Batch {
    let dir = root.join(path);
    let key = rel(root, &dir);
    let mut b = Batch::new("code", &key, "ogrep-timeline");
    let services = match ogrep::services(&dir) {
        Ok(s) => s,
        Err(e) => {
            b.degraded.push(e.to_string());
            return b;
        }
    };
    let m = Extraction::Rule { extractor: "ogrep-timeline".into() };
    let timelines = ogrep::each(&services, |service| ogrep::timeline(&dir, service, since));
    for (service, commits) in services.iter().zip(timelines) {
        let commits = match commits {
            Ok(c) => c,
            Err(e) => {
                b.degraded.push(format!("{service}: {e}"));
                continue;
            }
        };
        for c in commits {
            let ev = b.record(format!("{key}@{}", c.sha));
            let s = known("ev", &format!("commit/{}/{}", slug(service), c.sha.get(..12).unwrap_or(&c.sha)));
            let details = [(evt::SUMMARY, lit(c.subject.chars().take(300).collect::<String>())), (evt::REVISION, lit(c.sha.as_str()))];
            event(&mut b, &s, Event { class: evt::COMMIT, on: service, at: c.time * 1000, signal: "COMMIT", severity: "SEV4", value: None }, details, &ev, &m);
            for file in c.files.iter().take(MAX_FILES) {
                b.add(s.clone(), evt::CHANGED_FILE, lit(rel(root, &dir.join(file))), &ev, &m);
            }
        }
    }
    b
}
