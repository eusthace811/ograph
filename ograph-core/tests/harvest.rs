use ograph_core::workspace::Workspace;
use std::path::{Path, PathBuf};

/// A workspace whose catalog (a stand-in for ogrep) lists the one application `pay`.
fn workspace(name: &str, sources: &str, files: &[(&str, &str)]) -> PathBuf {
    let tmp = std::env::temp_dir();
    let ogrep = tmp.join(format!("ograph-harvest-test-ogrep-{}.sh", std::process::id()));
    let entity = r#"{"data":{"entity":{"id":"pay","kind":"service","locations":[{"file":"x","line":1}]}}}"#;
    std::fs::write(&ogrep, format!("#!/bin/sh\necho '{entity}'\n")).unwrap();
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
