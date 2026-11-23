// The stand-in for ogrep is a shell script, which Windows cannot run as a command.
#![cfg(unix)]

use ograph_core::workspace::Workspace;
use std::path::{Path, PathBuf};

/// A workspace whose catalog (a stand-in for ogrep) lists the one application `pay` and every
/// file not hidden.
fn workspace(name: &str, sources: &str, files: &[(&str, &str)]) -> PathBuf {
    let tmp = std::env::temp_dir();
    let ogrep = tmp.join(format!("ograph-harvest-test-ogrep-{}.sh", std::process::id()));
    let entity = r#"{"data":{"entity":{"id":"pay","kind":"service","locations":[{"file":"x","line":1}]}}}"#;
    let list = "find . -path '*/.*' -prune -o -type f -print";
    std::fs::write(&ogrep, format!("#!/bin/sh\nif [ \"$1\" = --files ]; then {list}; else echo '{entity}'; fi\n")).unwrap();
    std::fs::set_permissions(&ogrep, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // SAFETY: every test sets the same value.
    unsafe { std::env::set_var("OGRAPH_OGREP", &ogrep) };
    let root = tmp.join(format!("ograph-harvest-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("catalog")).unwrap();
    for (path, body) in files {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let dir = Workspace::init(&root).unwrap();
    std::fs::write(dir.join("config.toml"), format!("[[source]]\nkind = \"catalog\"\npath = \"catalog\"\n{sources}")).unwrap();
    root
}

fn values(root: &Path, query: &str) -> Vec<String> {
    let ws = Workspace::open(root).unwrap();
    let snap = ws.store.snapshot().unwrap();
    let mut out: Vec<String> = ograph_core::sparql::solutions(&snap, query)
        .unwrap()
        .into_iter()
        .map(|row| match &row[0] {
            Some(oxrdf::Term::Literal(l)) => l.value().to_owned(),
            other => format!("{other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn harvest(root: &Path, only: Option<&str>) -> Vec<ograph_core::gateway::Outcome> {
    Workspace::open(root).unwrap().harvest(only, |_, _| {}).unwrap()
}

const DEPLOYMENTS: &str = "[[source]]\nkind = \"deployments\"\npath = \"events\"\n";
const VERSIONS: &str = "PREFIX evt: <https://ograph.dev/evt#> SELECT ?v WHERE { ?e evt:version ?v }";

fn deploy(version: &str) -> String {
    format!(r#"{{"service": "pay", "version": "{version}", "at": "2026-11-22T10:00:00Z"}}"#)
}

#[test]
fn an_unreadable_subdirectory_keeps_its_graphs_and_degrades_the_harvest() {
    use std::os::unix::fs::PermissionsExt;
    let root = workspace("unreadable", DEPLOYMENTS, &[("events/a/b.json", &deploy("1.0.0")), ("events/other.json", &deploy("2.0.0"))]);
    harvest(&root, None);
    assert_eq!(values(&root, VERSIONS), ["1.0.0", "2.0.0"]);
    let locked = root.join("events/a");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = harvest(&root, Some("deployments"));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(out.iter().any(|o| o.degraded.iter().any(|d| d.contains("events/a"))), "{out:?}");
    assert_eq!(values(&root, VERSIONS), ["1.0.0", "2.0.0"]);
}

#[test]
fn a_backslash_in_a_file_name_is_not_a_directory_separator() {
    let root = workspace("backslash", DEPLOYMENTS, &[("events/a/b", &deploy("1.0.0")), ("events/a\\b", &deploy("1.9.0"))]);
    harvest(&root, None);
    assert_eq!(values(&root, VERSIONS), ["1.0.0", "1.9.0"]);
}

const INCIDENTS: &str = "[[source]]\nkind = \"incidents\"\npath = \"incidents\"\n";
const STATUS: &str = "PREFIX inc: <https://ograph.dev/inc#> SELECT ?s WHERE { ?i inc:status ?s }";
const MITIGATION: &str = "PREFIX inc: <https://ograph.dev/inc#> SELECT ?m WHERE { ?i inc:mitigation ?m }";

#[test]
fn the_last_record_of_an_incident_in_a_file_wins() {
    let lines = [
        r#"{"id": "INC-1", "title": "pay down", "service": "pay", "opened": "2026-11-22T10:00:00Z", "status": "open"}"#,
        r#"{"id": "INC-1", "status": "resolved", "mitigation": "rolled back"}"#,
    ];
    let root = workspace("incident-lines", INCIDENTS, &[("incidents/all.json", &lines.join("\n"))]);
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    assert_eq!(values(&root, STATUS), ["resolved"]);
    assert_eq!(values(&root, MITIGATION), ["rolled back"]);
}

#[test]
fn the_last_file_that_states_an_incident_field_wins() {
    let root = workspace(
        "incident-files",
        INCIDENTS,
        &[
            ("incidents/2026-11-22.json", r#"{"id": "INC-1", "title": "pay down", "service": "pay", "opened": "2026-11-22T10:00:00Z", "status": "open"}"#),
            ("incidents/2026-11-23.json", r#"{"id": "INC-1", "status": "resolved", "mitigation": "rolled back"}"#),
        ],
    );
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    assert_eq!(values(&root, STATUS), ["resolved"]);
    assert_eq!(values(&root, "PREFIX inc: <https://ograph.dev/inc#> SELECT ?t WHERE { ?i inc:title ?t }"), ["pay down"]);
}

#[test]
fn a_numeric_incident_id_is_read_as_text() {
    let lines = [r#"{"id": 4120, "title": "pay down", "service": "pay", "opened": "2026-11-22T10:00:00Z"}"#, r#"{"id": "INC-2", "title": "pay slow", "service": "pay", "opened": "2026-11-22T11:00:00Z"}"#];
    let root = workspace("incident-numeric", INCIDENTS, &[("incidents/all.json", &lines.join("\n"))]);
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.degraded.is_empty()), "{out:?}");
    assert_eq!(values(&root, "PREFIX inc: <https://ograph.dev/inc#> SELECT ?id WHERE { ?i inc:id ?id }"), ["4120", "INC-2"]);
}

#[test]
fn a_typo_in_the_model_settings_is_an_error() {
    let root = workspace("llm-typo", "", &[]);
    let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
    let llm = "[llm.extract]\ncommand = [\"pi\"]\nmodel = \"pi:m\"\n";
    std::fs::write(root.join(".ograph/config.toml"), format!("{config}{llm}parallel = 8\n")).unwrap();
    assert!(Workspace::open(&root).is_ok());
    for typo in ["paralel = 8\n", "timeuot_s = 1\n"] {
        std::fs::write(root.join(".ograph/config.toml"), format!("{config}{llm}{typo}")).unwrap();
        assert!(Workspace::open(&root).is_err(), "accepted {typo:?}");
    }
}

const TWO_INCIDENT_SOURCES: &str = "[[source]]\nkind = \"incidents\"\npath = \"east\"\n[[source]]\nkind = \"incidents\"\npath = \"west\"\n";
const IDS: &str = "PREFIX inc: <https://ograph.dev/inc#> SELECT ?id WHERE { ?i inc:id ?id }";
const TITLE: &str = "PREFIX inc: <https://ograph.dev/inc#> SELECT ?t WHERE { ?i inc:title ?t }";

#[test]
fn an_incident_whose_required_fields_are_in_different_files_is_whole_after_one_harvest() {
    let root = workspace(
        "incident-split",
        INCIDENTS,
        &[("incidents/a.json", r#"{"id": "INC-1", "title": "pay down", "service": "pay", "status": "open"}"#), ("incidents/b.json", r#"{"id": "INC-1", "opened": "2026-11-22T10:00:00Z"}"#)],
    );
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    assert_eq!(values(&root, IDS), ["INC-1"]);
    assert_eq!(values(&root, TITLE), ["pay down"]);
}

#[test]
fn the_last_file_wins_across_incident_sources() {
    let root = workspace(
        "incident-sources",
        TWO_INCIDENT_SOURCES,
        &[
            ("east/1.json", r#"{"id": "INC-1", "title": "pay down", "service": "pay", "opened": "2026-11-22T10:00:00Z", "status": "open"}"#),
            ("west/1.json", r#"{"id": "INC-1", "status": "resolved"}"#),
        ],
    );
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    assert_eq!(values(&root, STATUS), ["resolved"]);
}

#[test]
fn a_deleted_incident_file_is_pruned_and_the_first_harvest_has_no_conflict() {
    let full = |status: &str| format!(r#"{{"id": "INC-1", "title": "pay down", "service": "pay", "opened": "2026-11-22T10:00:00Z", "status": "{status}"}}"#);
    let root = workspace("incident-deleted", INCIDENTS, &[("incidents/a.json", &full("resolved")), ("incidents/b.json", &full("open"))]);
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    assert_eq!(values(&root, STATUS), ["open"]);
    std::fs::remove_file(root.join("incidents/b.json")).unwrap();
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    assert_eq!(values(&root, STATUS), ["resolved"]);
}

#[test]
fn an_unreadable_incident_file_or_source_keeps_every_incident() {
    let incident = |id: &str| format!(r#"{{"id": "{id}", "title": "t", "service": "pay", "opened": "2026-11-22T10:00:00Z"}}"#);
    let root = workspace("incident-unreadable", TWO_INCIDENT_SOURCES, &[("east/1.json", &incident("INC-1")), ("west/1.json", &incident("INC-2")), ("west/2.json", &incident("INC-3"))]);
    harvest(&root, None);
    assert_eq!(values(&root, IDS), ["INC-1", "INC-2", "INC-3"]);
    std::fs::write(root.join("west/1.json"), "{not json").unwrap();
    std::fs::remove_file(root.join("west/2.json")).unwrap();
    let out = harvest(&root, None);
    assert!(out.iter().any(|o| o.degraded.iter().any(|d| d.contains("west/1.json"))), "{out:?}");
    assert_eq!(values(&root, IDS), ["INC-1", "INC-2", "INC-3"]);
    std::fs::write(root.join("west/1.json"), incident("INC-2")).unwrap();
    std::fs::rename(root.join("east"), root.join("east-away")).unwrap();
    let out = harvest(&root, None);
    assert!(out.iter().any(|o| o.degraded.iter().any(|d| d.contains("east"))), "{out:?}");
    assert_eq!(values(&root, IDS), ["INC-1", "INC-2", "INC-3"]);
    std::fs::rename(root.join("east-away"), root.join("east")).unwrap();
    harvest(&root, None);
    assert_eq!(values(&root, IDS), ["INC-1", "INC-2"]);
}

#[test]
fn the_server_watches_only_the_files_a_harvest_reads() {
    let root = workspace("watch", INCIDENTS, &[("deploy/compose.yaml", "services: {}\n"), ("incidents/a.json", "[]")]);
    std::fs::write(root.join(".ograph/config.toml"), format!("[[source]]\nkind = \"catalog\"\npath = \".\"\n{INCIDENTS}")).unwrap();
    let inventory = || ograph_core::serve::inventory(&Workspace::open(&root).unwrap());
    let watched: Vec<String> = inventory().into_iter().map(|(f, ..)| f.strip_prefix(&root).unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(watched, [".ograph/config.toml", ".ograph/ontology.ttl", ".ograph/shapes.ttl", "deploy/compose.yaml", "incidents/a.json"]);
    let before = inventory();
    std::fs::write(root.join("server.log"), "started").unwrap();
    std::fs::write(root.join(".ograph/serve.json"), "{}").unwrap();
    assert_eq!(inventory(), before);
    std::fs::write(root.join("deploy/compose.yaml"), "services: {pay: {}}\n").unwrap();
    assert_ne!(inventory(), before);
}

#[test]
fn ids_that_differ_only_in_case_are_one_incident() {
    let lines = [
        r#"{"id": "INC-1", "title": "pay down", "service": "pay", "opened": "2026-11-22T10:00:00Z", "status": "open"}"#,
        r#"{"id": "inc-1", "status": "resolved"}"#,
    ];
    let root = workspace("incident-case", INCIDENTS, &[("incidents/all.json", &lines.join("\n"))]);
    let out = harvest(&root, None);
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    assert_eq!(values(&root, STATUS), ["resolved"]);
}
