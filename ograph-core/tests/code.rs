// The stand-in for ogrep is a shell script, which Windows cannot run as a command.
#![cfg(unix)]

use ograph_core::workspace::Workspace;
use std::path::{Path, PathBuf};

const T: i64 = 1_795_442_000;
const SHA: &str = "98aecc6b2ee7d4a7bcecf025720a5200f55f35ce";

/// A stand-in for ogrep that answers each flag ograph calls for a repository of three services,
/// checkout -> payments -> ledger-db. It appends its arguments to `.calls` in the repository,
/// and fails every call in a repository that holds a file named `.fail`.
fn ogrep() -> &'static Path {
    static OGREP: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    OGREP.get_or_init(|| {
        let commit = format!(r#""commit":{{"files":["services/payments/pool.py"],"sha":"{SHA}","subject":"payments: shrink ledger pool","time":{}}}"#, T - 720);
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> .calls
[ -f .fail ] && {{ echo "boom" >&2; exit 2; }}
case "$1" in
--files) find . -path '*/.*' -prune -o -type f -print ;;
--entities) cat <<'X'
{{"type":"entity","data":{{"edges":[{{"evidence":{{"file":"compose.yml","line":4}},"from":"checkout","provenance":"declared","relation":"depends_on","to":"payments"}}],"entity":{{"id":"checkout","kind":"service","locations":[{{"file":"compose.yml","line":2}}]}}}}}}
{{"type":"entity","data":{{"edges":[{{"evidence":{{"file":"compose.yml","line":7}},"from":"payments","provenance":"declared","relation":"depends_on","to":"ledger-db"}}],"entity":{{"id":"payments","kind":"service","locations":[{{"file":"compose.yml","line":5}}]}}}}}}
{{"type":"entity","data":{{"edges":[],"entity":{{"id":"ledger-db","kind":"service","locations":[{{"file":"compose.yml","line":10}}]}}}}}}
X
;;
--timeline) case "$2" in
  payments|checkout) echo '{{"type":"event","data":{{{commit},"service":"payments","time":1}}}}' ;;
  *) exit 1 ;;
  esac ;;
--log-templates)
  n=$(grep -n 'pool exhausted' "$4" | head -1 | cut -d: -f1)
  [ -n "$n" ] || exit 0
  echo '{{"type":"template","data":{{"code":["services/payments/pool.py",11,"log.error()"],"count":1,"example":{{"line_number":'"$n"',"path":"x"}},"template":"t"}}}}' ;;
--stack-trace)
  grep -q 'pool.py' || {{ echo "ogrep: no frame of the trace is in this repository" >&2; exit 1; }}
  echo '{{"type":"frame","data":{{"function":"acquire","index":1,"line_number":11,"path":"services/payments/pool.py"}}}}'
  echo '{{"type":"frame","data":{{"function":"pay","index":0,"line_number":6,"path":"services/checkout/client.py"}}}}' ;;
--about)
  echo '{{"type":"rank","data":{{"confidence":{{"level":"confident","margin":0.1}},"path":{{"text":"./services/payments/pool.py"}},"rank":1,"score":3.649268}}}}'
  echo '{{"type":"begin","data":{{"path":{{"text":"./services/payments/pool.py"}}}}}}'
  echo '{{"type":"match","data":{{"line_number":11,"path":{{"text":"./services/payments/pool.py"}}}}}}'
  echo '{{"type":"rank","data":{{"path":{{"text":"./compose.yml"}},"rank":2,"score":1.2}}}}' ;;
--similar-incidents)
  echo '{{"type":"incident","data":{{"path":"docs/postmortems/2026-01-ledger-pool.md","score":16.189942,"terms":["exhausted","pool"]}}}}' ;;
--suspects)
  echo '{{"type":"suspect","data":{{"rank":1,"suspect":{{"changes":[{{"files":["services/payments/pool.py"],"sha":"{SHA}","subject":"payments: shrink ledger pool","time":{at}}}],"distance":1.0,"explains":["checkout"],"score":2.727272727272727,"service":"payments"}}}}}}'
  echo '{{"type":"suspect","data":{{"rank":2,"suspect":{{"changes":[],"distance":0.0,"explains":["checkout"],"score":1.0,"service":"checkout"}}}}}}' ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            at = T - 720
        );
        let p = std::env::temp_dir().join(format!("ograph-code-test-ogrep-{}.sh", std::process::id()));
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        // SAFETY: every test sets the same value.
        unsafe { std::env::set_var("OGRAPH_OGREP", &p) };
        p
    })
}

const CATALOG: &str = "[[source]]\nkind = \"catalog\"\npath = \"shop\"\n";
const CODE: &str = "[[source]]\nkind = \"code\"\npath = \"shop\"\n";

/// A workspace whose repository `shop` the stand-in describes, with an incident on checkout
/// at `T`, the `sources` of `config.toml` and the `files` they read.
fn workspace(name: &str, sources: &str, files: &[(&str, &str)]) -> PathBuf {
    ogrep();
    let root = std::env::temp_dir().join(format!("ograph-code-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("shop")).unwrap();
    let incident = format!(r#"[{{"id":"INC-1","title":"Checkout failing: ledger-db connection pool exhausted","service":"checkout","opened":{T},"severity":"SEV1","status":"open"}}]"#);
    for (path, body) in [("incidents.json", incident.as_str())].iter().chain(files) {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let dir = Workspace::init(&root).unwrap();
    std::fs::write(dir.join("config.toml"), format!("{sources}[[source]]\nkind = \"incidents\"\npath = \"incidents.json\"\n")).unwrap();
    root
}

/// Routine lines, then a burst of one error template on payments 100 seconds before `T`.
fn logs() -> String {
    let mut out = String::new();
    for i in 0..10 {
        out += &format!("{{\"ts\":{},\"service\":\"payments\",\"level\":\"info\",\"message\":\"charge ok id={i} took 80ms\"}}\n", T - 600 + i * 30);
    }
    for i in 0..5 {
        out += &format!("{{\"ts\":{},\"service\":\"payments\",\"level\":\"error\",\"message\":\"ledger-db connection pool exhausted after 3000ms waiting=6{i}\"}}\n", T - 100 + i);
    }
    out
}

fn errors() -> String {
    let stack = r#"Traceback (most recent call last):\n  File \"/app/services/checkout/client.py\", line 6, in pay\n  File \"/app/services/payments/pool.py\", line 11, in acquire\nTimeoutError: pool exhausted"#;
    format!("{{\"service\":\"checkout\",\"at\":{},\"message\":\"TimeoutError: pool exhausted\",\"level\":\"error\",\"count\":4,\"stack\":\"{stack}\"}}\n{{\"service\":\"checkout\",\"at\":{},\"message\":\"KeyError: sku\",\"stack\":\"File \\\"/usr/lib/python3/json.py\\\", line 3\"}}\n", T - 60, T - 50)
}

fn harvest(root: &Path) -> Vec<ograph_core::gateway::Outcome> {
    let out = Workspace::open(root).unwrap().harvest(None, |_, _| {}).unwrap();
    assert!(out.iter().all(|o| o.quarantined.is_empty()), "{out:?}");
    out
}

fn values(root: &Path, query: &str) -> Vec<String> {
    let ws = Workspace::open(root).unwrap();
    let snap = ws.store.snapshot().unwrap();
    let mut out: Vec<String> = ograph_core::sparql::solutions(&snap, query)
        .unwrap()
        .into_iter()
        .map(|row| match &row[0] {
            Some(oxrdf::Term::Literal(l)) => l.value().to_owned(),
            Some(oxrdf::Term::NamedNode(n)) => n.as_str().to_owned(),
            other => format!("{other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn violations(root: &Path) -> usize {
    let ws = Workspace::open(root).unwrap();
    ograph_core::gateway::audit(&ws.store.snapshot().unwrap(), &ws.ontology).len()
}

fn calls(root: &Path) -> String {
    std::fs::read_to_string(root.join("shop/.calls")).unwrap_or_default()
}

#[test]
fn a_commit_is_a_change_event_on_its_service() {
    let root = workspace("commits", &format!("{CATALOG}{CODE}"), &[]);
    let out = harvest(&root);
    assert!(out.iter().all(|o| o.degraded.is_empty()), "{out:?}");
    let about = |p: &str| values(&root, &format!("SELECT ?v WHERE {{ ?e a evt:Commit ; evt:on id:app\\/payments ; evt:signal \"COMMIT\" ; {p} ?v }}"));
    assert_eq!(about("evt:revision"), [SHA]);
    assert_eq!(about("evt:summary"), ["payments: shrink ledger pool"]);
    assert_eq!(about("evt:changedFile"), ["shop/services/payments/pool.py"]);
    assert_eq!(about("evt:at"), [ograph_core::term::format_millis((T - 720) * 1000)]);
    assert_eq!(about("evt:sourceRecord"), [format!("shop@{SHA}")]);
    // The stand-in lists the payments commit in the timeline of checkout too, as ogrep lists
    // the commits of a service's dependencies; it stays one event, on payments.
    assert_eq!(values(&root, "SELECT ?e WHERE { ?e a evt:Commit }"), ["https://ograph.dev/id/ev/commit-payments-98aecc6b2ee7"]);
    assert_eq!(violations(&root), 0);
}

#[test]
fn a_log_burst_points_at_the_line_that_writes_it() {
    let sources = format!("{CATALOG}{CODE}[[source]]\nkind = \"logs\"\npath = \"logs.jsonl\"\n");
    let root = workspace("log-code", &sources, &[("logs.jsonl", &logs())]);
    harvest(&root);
    assert_eq!(values(&root, "SELECT ?l WHERE { ?e a evt:LogError ; evt:on id:app\\/payments ; evt:codeLocation ?l }"), ["shop/services/payments/pool.py:11"]);
    assert_eq!(violations(&root), 0);
}

#[test]
fn an_exception_points_at_the_frames_of_its_stack_trace() {
    let sources = format!("{CATALOG}{CODE}[[source]]\nkind = \"errors\"\npath = \"errors.jsonl\"\n");
    let root = workspace("errors", &sources, &[("errors.jsonl", &errors())]);
    harvest(&root);
    let timeout = "?e a evt:Exception ; evt:on id:app\\/checkout ; evt:signal \"EXCEPTION\" ; evt:severity \"SEV2\" ; evt:summary \"TimeoutError: pool exhausted\"";
    assert_eq!(values(&root, &format!("SELECT ?l WHERE {{ {timeout} ; evt:codeLocation ?l }}")), ["shop/services/checkout/client.py:6", "shop/services/payments/pool.py:11"]);
    assert_eq!(values(&root, &format!("SELECT ?n WHERE {{ {timeout} ; evt:count ?n }}")), ["4"]);
    // A trace with no frame in the repository is still an event, at the default severity.
    assert_eq!(values(&root, "SELECT ?s WHERE { ?e a evt:Exception ; evt:summary \"KeyError: sku\" ; evt:severity ?s }"), ["SEV3"]);
    assert!(values(&root, "SELECT ?l WHERE { ?e evt:summary \"KeyError: sku\" ; evt:codeLocation ?l }").is_empty());
    assert_eq!(violations(&root), 0);
}

#[test]
fn without_a_code_source_nothing_is_looked_up_in_code() {
    let sources = format!("{CATALOG}[[source]]\nkind = \"logs\"\npath = \"logs.jsonl\"\n[[source]]\nkind = \"errors\"\npath = \"errors.jsonl\"\n");
    let root = workspace("no-code", &sources, &[("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    harvest(&root);
    assert_eq!(values(&root, "SELECT ?e WHERE { ?e a evt:LogError }").len(), 1);
    assert_eq!(values(&root, "SELECT ?e WHERE { ?e a evt:Exception }").len(), 2);
    assert!(values(&root, "SELECT ?l WHERE { ?e evt:codeLocation ?l }").is_empty());
    let ws = Workspace::open(&root).unwrap();
    let answer = ws.code(&ws.store.snapshot().unwrap(), "INC-1", 5).unwrap();
    assert_eq!(serde_json::to_value(&answer).unwrap(), serde_json::json!({"incident": "id:inc/inc-1", "app": "checkout", "at": ograph_core::term::format_millis(T * 1000), "locations": [], "commits": [], "files": [], "files_confidence": null, "writeups": [], "degraded": []}));
    assert_eq!(calls(&root), "--entities . --json\n");
}

const EVERYTHING: &str = "[[source]]\nkind = \"logs\"\npath = \"logs.jsonl\"\n[[source]]\nkind = \"errors\"\npath = \"errors.jsonl\"\n";

#[test]
fn code_lists_the_lines_and_the_commits_behind_an_incident() {
    let root = workspace("code-query", &format!("{CATALOG}{CODE}{EVERYTHING}"), &[("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    harvest(&root);
    let ws = Workspace::open(&root).unwrap();
    let snap = ws.store.snapshot().unwrap();
    let (answer, text) = ograph_core::query::code(&snap, "INC-1", &ws.config.correlation).unwrap();
    let pool = "shop/services/payments/pool.py";
    let locations: Vec<_> = answer.locations.iter().map(|l| (l.app.as_str(), l.signal.as_str(), l.location.as_str(), l.summary.as_deref().unwrap_or(""))).collect();
    assert_eq!(
        locations,
        [
            ("checkout", "EXCEPTION", "shop/services/checkout/client.py:6", "TimeoutError: pool exhausted"),
            ("checkout", "EXCEPTION", "shop/services/payments/pool.py:11", "TimeoutError: pool exhausted"),
            ("payments", "LOG_ERROR", "shop/services/payments/pool.py:11", "ledger-db connection pool exhausted after <*> <*>"),
        ]
    );
    assert_eq!(
        serde_json::to_value(&answer.commits).unwrap(),
        serde_json::json!([{"event": "id:ev/commit-payments-98aecc6b2ee7", "app": "payments", "at": ograph_core::term::format_millis((T - 720) * 1000), "minutes_before": 12.0, "revision": SHA, "summary": "payments: shrink ledger pool", "files": [pool], "touches": [format!("{pool}:11")]}])
    );
    assert!(answer.files.is_empty() && answer.writeups.is_empty() && answer.degraded.is_empty());
    assert!(text.starts_with("Checkout failing: ledger-db connection pool exhausted payments "), "{text}");
    assert!(!text.contains("<*>"), "{text}");
}

#[test]
fn code_adds_the_files_and_write_ups_ogrep_finds_for_the_incident() {
    let root = workspace("code-search", &format!("{CATALOG}{CODE}{EVERYTHING}"), &[("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    harvest(&root);
    let ws = Workspace::open(&root).unwrap();
    let answer = serde_json::to_value(ws.code(&ws.store.snapshot().unwrap(), "INC-1", 5).unwrap()).unwrap();
    assert_eq!(answer["files"], serde_json::json!([{"path": "shop/services/payments/pool.py", "score": 3.65, "line": 11}, {"path": "shop/compose.yml", "score": 1.2, "line": null}]));
    assert_eq!(answer["files_confidence"], "confident");
    assert_eq!(answer["writeups"], serde_json::json!([{"path": "shop/docs/postmortems/2026-01-ledger-pool.md", "score": 16.19, "terms": ["exhausted", "pool"]}]));
    assert_eq!(answer["degraded"], serde_json::json!([]));
    let top = ws.code(&ws.store.snapshot().unwrap(), "INC-1", 1).unwrap();
    assert_eq!(top.files.len(), 1);
}

#[test]
fn code_names_the_repository_ogrep_failed_in_and_keeps_the_graph_half() {
    let sources = format!("{CATALOG}{CODE}[[source]]\nkind = \"code\"\npath = \"broken\"\n{EVERYTHING}");
    let root = workspace("code-degraded", &sources, &[("broken/.fail", ""), ("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    harvest(&root);
    let ws = Workspace::open(&root).unwrap();
    let answer = ws.code(&ws.store.snapshot().unwrap(), "INC-1", 5).unwrap();
    assert_eq!(answer.degraded, ["broken: ogrep --about failed: boom", "broken: ogrep --similar-incidents failed: boom"]);
    assert_eq!((answer.locations.len(), answer.commits.len(), answer.files.len(), answer.writeups.len()), (3, 1, 2, 1));
}

#[test]
fn a_failing_ogrep_degrades_the_code_lookups_and_keeps_the_events() {
    let sources = format!("{CATALOG}[[source]]\nkind = \"code\"\npath = \"broken\"\n[[source]]\nkind = \"logs\"\npath = \"logs.jsonl\"\n[[source]]\nkind = \"errors\"\npath = \"errors.jsonl\"\n");
    let root = workspace("failing", &sources, &[("broken/.fail", ""), ("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    let out = harvest(&root);
    for graph in ["/code/broken", "/logs/logs.jsonl", "/errors/errors.jsonl"] {
        let o = out.iter().find(|o| o.graph.ends_with(graph)).unwrap();
        assert!(o.degraded.iter().any(|d| d.contains("boom")), "{o:?}");
    }
    assert_eq!(values(&root, "SELECT ?e WHERE { ?e a evt:LogError }").len(), 1);
    assert_eq!(values(&root, "SELECT ?e WHERE { ?e a evt:Exception }").len(), 2);
    assert!(values(&root, "SELECT ?e WHERE { ?e a evt:Commit }").is_empty());
}

#[test]
fn the_server_watches_the_head_log_of_a_code_repository() {
    let root = workspace("watch", &format!("{CATALOG}{CODE}"), &[("shop/.git/logs/HEAD", "0 1 commit\n"), ("shop/services/payments/pool.py", "")]);
    let ws = Workspace::open(&root).unwrap();
    let code = ws.config.sources.iter().find(|s| s.name() == "code").unwrap();
    assert_eq!(ograph_core::enrich::files(&ws.root, code), [root.join("shop/.git/logs/HEAD")]);
    std::fs::remove_dir_all(root.join("shop/.git")).unwrap();
    assert!(ograph_core::enrich::files(&ws.root, code).is_empty());
}
