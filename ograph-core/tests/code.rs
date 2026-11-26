// The stand-in for ogrep is a shell script, which Windows cannot run as a command.
#![cfg(unix)]

use ograph_core::workspace::Workspace;
use std::path::{Path, PathBuf};

const T: i64 = 1_795_442_000;
const SHA: &str = "98aecc6b2ee7d4a7bcecf025720a5200f55f35ce";
const FIRST: &str = "0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f60718293";

/// A stand-in for ogrep that answers each flag ograph calls for a repository of four services,
/// checkout -> payments -> ledger-db as files declare them, and checkout -> mailer as ogrep
/// infers it. It appends its arguments to `.calls` in the repository, fails every call in a
/// repository that holds a file named `.fail`, and in one that holds `.old` refuses what ogrep
/// 0.2.0 refuses: a list of services and `--until` for `--timeline` and `--suspects`. With
/// `--dated` every edge came with its service on 2001-09-09 except checkout -> mailer, added on
/// 2004-11-09; in a repository that holds `.undated` it refuses `--dated`, as ogrep 0.3.0 does.
fn ogrep() -> &'static Path {
    static OGREP: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    OGREP.get_or_init(|| {
        let commit = format!(r#""commit":{{"files":["services/payments/pool.py"],"sha":"{SHA}","subject":"payments: shrink ledger pool","time":{}}}"#, T - 720);
        let script = format!(
            r#"#!/bin/sh
echo "$*" >> .calls
[ -f .fail ] && {{ echo "boom" >&2; exit 2; }}
case "$*" in *--dated*) [ -f .undated ] && {{ echo "error: unexpected argument '--dated' found" >&2; exit 2; }} ;; esac
undate() {{ case "$*" in *--dated*) cat ;; *) sed 's/,"since":{{[^}}]*}}//g' ;; esac; }}
if [ -f .old ]; then case "$1 $*" in
  "--timeline "*" --until "*|"--suspects "*" --until "*) echo "ogrep: --until and --author need --history or --introduced" >&2; exit 2 ;;
  "--timeline --timeline "*,*) echo "ogrep: no service named \"$2\"" >&2; exit 2 ;;
esac; fi
case "$1" in
--files) find . -path '*/.*' -prune -o -type f -print ;;
--entities) undate "$@" <<'X'
{{"type":"entity","data":{{"edges":[{{"evidence":{{"file":"compose.yml","line":4}},"from":"checkout","provenance":"declared","relation":"depends_on","to":"payments","since":{{"commit":"c1","time":1000000000}}}},{{"evidence":{{"file":"compose.yml","line":3}},"from":"checkout","provenance":"inferred","relation":"calls","to":"payments","since":{{"commit":"c1","time":1000000000}}}},{{"evidence":{{"file":"compose.yml","line":3}},"from":"checkout","provenance":"inferred","relation":"calls","to":"mailer","since":{{"commit":"c2","time":1100000000}}}},{{"evidence":{{"file":"compose.yml","line":3}},"from":"checkout","provenance":"inferred","relation":"telemetry","to":"ledger-db","since":{{"commit":"c1","time":1000000000}}}}],"entity":{{"aliases":["checkout-external"],"id":"checkout","kind":"service","locations":[{{"file":"compose.yml","line":2}}],"since":{{"commit":"c1","time":1000000000}}}}}}}}
{{"type":"entity","data":{{"edges":[{{"evidence":{{"file":"compose.yml","line":7}},"from":"payments","provenance":"declared","relation":"depends_on","to":"ledger-db","since":{{"commit":"c1","time":1000000000}}}}],"entity":{{"id":"payments","kind":"service","locations":[{{"file":"compose.yml","line":5}}],"since":{{"commit":"c1","time":1000000000}}}}}}}}
{{"type":"entity","data":{{"edges":[],"entity":{{"id":"ledger-db","kind":"service","locations":[{{"file":"compose.yml","line":10}}]}}}}}}
{{"type":"entity","data":{{"edges":[],"entity":{{"id":"mailer","kind":"service","locations":[{{"file":"compose.yml","line":12}}]}}}}}}
X
;;
--timeline) case "$2" in
  *payments*|checkout) echo '{{"type":"event","data":{{{commit},"service":"payments","time":1}}}}' ;;
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
--introduced)
  echo '{{"type":"introduced","data":{{"commits":2,"files_at_head":1,"first_added":{{"author":"a","date":"2026-11-20","line_number":11,"path":"services/payments/pool.py","sha":"{FIRST}","subject":"payments: add the ledger pool","text":"log.error()","time":{added}}},"last_removed":null}}}}' ;;
--suspects)
  echo '{{"type":"suspect","data":{{"rank":1,"suspect":{{"changes":[{{"files":["services/payments/pool.py"],"sha":"{SHA}","subject":"payments: shrink ledger pool","time":{at}}}],"distance":1.0,"explains":["checkout"],"score":2.727272727272727,"service":"payments"}}}}}}'
  echo '{{"type":"suspect","data":{{"rank":2,"suspect":{{"changes":[],"distance":0.0,"explains":["checkout"],"score":1.0,"service":"checkout"}}}}}}' ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            at = T - 720,
            added = T - 3 * 86_400
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
    assert_eq!(calls(&root).lines().filter(|l| l.starts_with("--timeline")).collect::<Vec<_>>(), ["--timeline checkout,payments,ledger-db,mailer --since 30d . --json"]);
}

#[test]
fn an_ogrep_that_takes_one_service_at_a_time_is_asked_for_each() {
    let root = workspace("commits-old", &format!("{CATALOG}{CODE}"), &[("shop/.old", "")]);
    let out = harvest(&root);
    assert!(out.iter().all(|o| o.degraded.is_empty()), "{out:?}");
    assert_eq!(values(&root, "SELECT ?r WHERE { ?e a evt:Commit ; evt:revision ?r }"), [SHA]);
    let mut asked: Vec<String> = calls(&root).lines().filter(|l| l.starts_with("--timeline")).map(str::to_owned).collect();
    asked.sort();
    assert_eq!(asked, ["--timeline checkout --since 30d . --json", "--timeline checkout,payments,ledger-db,mailer --since 30d . --json", "--timeline ledger-db --since 30d . --json", "--timeline mailer --since 30d . --json", "--timeline payments --since 30d . --json"]);
}

#[test]
fn a_dependency_added_to_a_declared_service_is_dated_and_one_that_came_with_it_is_not() {
    let root = workspace("dated", CATALOG, &[]);
    let out = harvest(&root);
    assert!(out.iter().all(|o| o.degraded.is_empty()), "{out:?}");
    let dated = "SELECT ?x WHERE { ?d a ops:Dependency ; ops:caller ?c ; ops:callee ?e ; ops:validFrom ?t ; ops:addedIn ?s . BIND(CONCAT(STR(?c), ' ', STR(?e), ' ', STR(?t), ' ', ?s) AS ?x) }";
    assert_eq!(values(&root, dated), ["https://ograph.dev/id/app/checkout https://ograph.dev/id/app/mailer 2004-11-09T11:33:20Z c2"]);
    assert_eq!(violations(&root), 0);

    let old = workspace("undated", CATALOG, &[("shop/.undated", "")]);
    let out = harvest(&old);
    let catalog = out.iter().find(|o| o.graph.contains("/catalog/")).unwrap();
    assert_eq!(catalog.degraded, ["shop: dependencies carry no date, so each holds at every time; dating them needs an ogrep release after 0.3.0"]);
    assert!(values(&old, dated).is_empty());
    assert_eq!(values(&old, "SELECT ?to WHERE { id:app\\/checkout ops:dependsOn ?to }"), ["https://ograph.dev/id/app/mailer", "https://ograph.dev/id/app/payments"]);
}

/// An incident on checkout that opened between 2001, when payments came with checkout, and
/// 2004, when checkout -> mailer was added, with an alert on each callee before it.
#[test]
fn every_view_of_the_ranking_follows_a_dependency_only_from_its_date() {
    let opened = 1_050_000_000;
    let alert = |app: &str, at: i64| format!(r#"{{"labels":{{"alertname":"Down","service":"{app}","severity":"critical"}},"startsAt":{at}}}"#);
    let alerts = format!("[{},{}]", alert("mailer", opened - 300), alert("payments", opened - 200));
    let root = workspace("dated-views", &format!("{CATALOG}[[source]]\nkind = \"alerts\"\npath = \"alerts.json\"\n"), &[("alerts.json", &alerts), ("shop/.undated", "")]);
    std::fs::write(root.join("incidents.json"), format!(r#"[{{"id":"INC-1","title":"Checkout failing","service":"checkout","opened":{opened},"severity":"SEV1","status":"open"}}]"#)).unwrap();
    harvest(&root);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let undated_at = ograph_core::term::format_millis(ograph_core::store::now_ms());
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::remove_file(root.join("shop/.undated")).unwrap();
    harvest(&root);

    let ws = Workspace::open(&root).unwrap();
    let ranked = |snap: &ograph_core::store::Snapshot| {
        let ids = ograph_core::correlate::Ids::new(snap).unwrap();
        let a = ograph_core::query::chains(snap, &ids, ograph_core::query::lookup(snap, "INC-1").unwrap(), &ws.config.correlation, &mut Default::default()).unwrap();
        (a.apps.iter().map(|r| r.app.clone()).collect::<Vec<_>>(), a.withheld.iter().map(|w| format!("{} -> {} from {}", w.caller, w.callee, w.valid_from)).collect::<Vec<_>>())
    };
    assert_eq!(ranked(&ws.store.snapshot().unwrap()), (vec!["payments".to_owned()], vec!["checkout -> mailer from 2004-11-09T11:33:20Z".to_owned()]));
    assert_eq!(ranked(&ws.known_at(&undated_at).unwrap().0), (vec!["mailer".to_owned(), "payments".to_owned()], vec![]));

    let names = |r: &[ograph_core::query::RankedApp]| r.iter().map(|a| a.app.clone()).collect::<Vec<_>>();
    let t = ograph_core::workspace::when_ms(&undated_at).unwrap();
    let shift = ograph_core::query::rank_shift(&ws.store.snapshot().unwrap(), t, "INC-1", &ws.config.correlation, 5).unwrap();
    assert_eq!((names(&shift.then), names(&shift.now)), (vec!["mailer".to_owned(), "payments".to_owned()], vec!["payments".to_owned()]));
    assert_eq!(shift.causes.iter().map(|c| c.graph.as_str()).collect::<Vec<_>>(), ["g:harvest/catalog/shop"]);

    let used = ograph_core::query::uses(&ws.store.snapshot().unwrap(), &ws.ontology, &ws.config.correlation, "alerts.json").unwrap();
    assert_eq!(used.chains.iter().map(|c| (c.change, c.rank)).collect::<Vec<_>>(), [("gone", Some(1))]);
    let snap = ws.store.snapshot().unwrap();
    let ids = ograph_core::correlate::Ids::new(&snap).unwrap();
    let now = ograph_core::query::chains(&snap, &ids, ograph_core::query::lookup(&snap, "INC-1").unwrap(), &ws.config.correlation, &mut Default::default()).unwrap();
    assert_eq!(used.chains[0].chain, ograph_core::vocab::compact(&now.apps[0].chain.as_ref().unwrap().id));
}

#[test]
fn withheld_lists_only_the_dependencies_the_depth_would_follow() {
    let root = workspace("dated-depth", CATALOG, &[]);
    std::fs::write(root.join("incidents.json"), r#"[{"id":"INC-1","service":"checkout","opened":1050000000}]"#).unwrap();
    harvest(&root);
    let ws = Workspace::open(&root).unwrap();
    let snap = ws.store.snapshot().unwrap();
    let withheld = |depth| ograph_core::query::before(&snap, "INC-1", 600, depth, 1800).unwrap().withheld.iter().map(|w| format!("{} -> {}", w.caller, w.callee)).collect::<Vec<_>>();
    assert_eq!(withheld(0), Vec::<String>::new());
    assert_eq!(withheld(1), ["checkout -> mailer"]);
}

#[test]
fn a_call_ogrep_infers_is_a_dependency_unless_the_catalog_says_declared_only() {
    let depends = |root: &Path| values(root, "SELECT ?to WHERE { id:app\\/checkout ops:dependsOn ?to }");
    let root = workspace("inferred", CATALOG, &[]);
    harvest(&root);
    assert_eq!(depends(&root), ["https://ograph.dev/id/app/mailer", "https://ograph.dev/id/app/payments"]);
    assert_eq!(values(&root, "SELECT ?a WHERE { id:app\\/checkout ops:alias ?a }"), ["checkout-external"]);
    let declared = workspace("declared-only", "[[source]]\nkind = \"catalog\"\npath = \"shop\"\ninferred = false\n", &[]);
    harvest(&declared);
    assert_eq!(depends(&declared), ["https://ograph.dev/id/app/payments"]);
}

/// Fifteen quiet minutes of payments, then `lines` error lines of a message it never wrote.
fn first_lines(lines: i64) -> String {
    let mut out = String::new();
    for i in 0..30 {
        out += &format!("{{\"ts\":{},\"service\":\"payments\",\"level\":\"info\",\"message\":\"charge ok id={i} took 80ms\"}}\n", T - 1000 + i * 30);
    }
    for i in 0..lines {
        out += &format!("{{\"ts\":{},\"service\":\"payments\",\"level\":\"error\",\"message\":\"ledger-db connection pool exhausted after 3000ms waiting=6{i}\"}}\n", T - 100 + i);
    }
    out
}

const LOGS: &str = "[[source]]\nkind = \"logs\"\npath = \"logs.jsonl\"\n";

#[test]
fn the_first_line_of_a_new_error_message_is_an_event_with_its_line_of_code() {
    let root = workspace("log-new", &format!("{CATALOG}{CODE}{LOGS}"), &[("logs.jsonl", &first_lines(2))]);
    harvest(&root);
    let about = |p: &str| values(&root, &format!("SELECT ?v WHERE {{ ?e a evt:LogError ; evt:on id:app\\/payments ; evt:signal \"LOG_NEW\" ; {p} ?v }}"));
    assert_eq!(about("evt:template"), ["ledger-db connection pool exhausted after <*> <*>"]);
    assert_eq!(about("evt:count"), ["2"]);
    assert_eq!(about("evt:at"), [ograph_core::term::format_millis((T - 100) * 1000)]);
    assert_eq!(about("evt:codeLocation"), ["shop/services/payments/pool.py:11"]);
    assert_eq!(values(&root, "SELECT ?e WHERE { ?e a evt:LogError }").len(), 1);
    assert_eq!(violations(&root), 0);
}

#[test]
fn a_burst_of_a_new_message_is_one_event_and_the_detector_can_be_turned_off() {
    let signals = |root: &Path| values(root, "SELECT ?s WHERE { ?e a evt:LogError ; evt:signal ?s }");
    let burst = workspace("log-new-burst", &format!("{CATALOG}{LOGS}"), &[("logs.jsonl", &first_lines(5))]);
    harvest(&burst);
    assert_eq!(signals(&burst), ["LOG_ERROR"]);
    let off = workspace("log-new-off", &format!("{CATALOG}{LOGS}"), &[("logs.jsonl", &first_lines(2))]);
    let config = std::fs::read_to_string(off.join(".ograph/config.toml")).unwrap();
    std::fs::write(off.join(".ograph/config.toml"), format!("{config}[detect]\nlog_new_after_s = 0\n")).unwrap();
    harvest(&off);
    assert!(signals(&off).is_empty());
    // The fixture of the other tests starts ten minutes before its burst: too little history
    // to call a message new.
    let short = workspace("log-new-short", &format!("{CATALOG}{LOGS}"), &[("logs.jsonl", &logs())]);
    harvest(&short);
    assert_eq!(signals(&short), ["LOG_ERROR"]);
}

const DEPLOYS: &str = "[[source]]\nkind = \"deployments\"\npath = \"deployments.jsonl\"\n";

fn deployments() -> String {
    format!("{{\"service\":\"payments\",\"version\":\"v1\",\"at\":{}}}\n{{\"service\":\"payments\",\"version\":\"v2\",\"at\":{}}}\n{{\"service\":\"checkout\",\"version\":\"v7\",\"at\":{}}}\n", T - 7200, T - 900, T - 800)
}

#[test]
fn a_deployment_lists_the_files_changed_since_the_version_before() {
    let root = workspace("deploy-files", &format!("{CATALOG}{CODE}{DEPLOYS}{EVERYTHING}"), &[("deployments.jsonl", &deployments()), ("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    let out = harvest(&root);
    assert!(out.iter().all(|o| o.degraded.is_empty()), "{out:?}");
    let pool = "shop/services/payments/pool.py";
    assert_eq!(values(&root, "SELECT ?v WHERE { ?e a evt:Deployment ; evt:changedFile ?f ; evt:version ?v }"), ["v2"]);
    assert_eq!(values(&root, "SELECT ?f WHERE { ?e a evt:Deployment ; evt:changedFile ?f }"), [pool]);
    // One call: the first deployment of a service has no version before it.
    assert_eq!(calls(&root).lines().filter(|l| l.contains("--until")).collect::<Vec<_>>(), ["--timeline payments --since v1 --until v2 . --json"]);
    assert_eq!(violations(&root), 0);
    let ws = Workspace::open(&root).unwrap();
    let (answer, _) = ograph_core::query::code(&ws.store.snapshot().unwrap(), "INC-1", &ws.config.correlation).unwrap();
    assert_eq!(
        serde_json::to_value(&answer.deployments).unwrap(),
        serde_json::json!([{"event": format!("id:ev/deploy-payments-v2-{}", (T - 900) * 1000), "app": "payments", "at": ograph_core::term::format_millis((T - 900) * 1000), "minutes_before": 15.0, "version": "v2", "files": [pool], "touches": [format!("{pool}:11")]}])
    );
}

#[test]
fn deployments_keep_no_files_without_a_code_source_or_with_an_ogrep_that_reads_no_revisions() {
    let plain = workspace("deploy-plain", &format!("{CATALOG}{DEPLOYS}"), &[("deployments.jsonl", &deployments())]);
    let out = harvest(&plain);
    assert!(out.iter().all(|o| o.degraded.is_empty()), "{out:?}");
    assert_eq!(calls(&plain), "--files .\n--entities . --json --dated\n");
    let old = workspace("deploy-old", &format!("{CATALOG}{CODE}{DEPLOYS}"), &[("deployments.jsonl", &deployments()), ("shop/.old", "")]);
    let out = harvest(&old);
    let o = out.iter().find(|o| o.graph.ends_with("/deployments/deployments.jsonl")).unwrap();
    assert_eq!(o.degraded, ["deployments.jsonl: 1 of 1 deployments could not be compared with the version before, so they list no changed files: this ogrep cannot compare two revisions; it needs ogrep 0.3 or newer"]);
    assert_eq!(values(&old, "SELECT ?e WHERE { ?e a evt:Deployment }").len(), 3);
    assert!(values(&old, "SELECT ?f WHERE { ?e evt:changedFile ?f . ?e a evt:Deployment }").is_empty());
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
    assert_eq!(calls(&root), "--files .\n--entities . --json --dated\n");
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
    // The message of the located log line entered the code three days before the incident.
    assert_eq!(answer["locations"][2]["introduced"], serde_json::json!({"revision": FIRST, "at": ograph_core::term::format_millis((T - 3 * 86_400) * 1000), "days_before": 3.0, "summary": "payments: add the ledger pool", "path": "shop/services/payments/pool.py"}));
    assert_eq!(answer["locations"][0].get("introduced"), None);
    let opened = ograph_core::term::format_millis(T * 1000);
    assert!(calls(&root).contains(&format!("--introduced -F -e ledger-db connection pool exhausted after --until {opened} . --json\n")), "{}", calls(&root));
    let top = ws.code(&ws.store.snapshot().unwrap(), "INC-1", 1).unwrap();
    assert_eq!(top.files.len(), 1);
}

fn suspects(root: &Path, target: &str, since: Option<&str>) -> (serde_json::Value, Vec<String>) {
    let ws = Workspace::open(root).unwrap();
    let (found, degraded) = ws.suspects(&ws.store.snapshot().unwrap(), target, since, &ws.config.correlation).unwrap();
    (serde_json::to_value(found).unwrap(), degraded)
}

fn expected_suspects() -> serde_json::Value {
    let change = serde_json::json!({"revision": SHA, "summary": "payments: shrink ledger pool", "at": ograph_core::term::format_millis((T - 720) * 1000), "files": ["shop/services/payments/pool.py"]});
    serde_json::json!([
        {"rank": 1, "app": "payments", "score": 2.73, "distance": 1.0, "explains": ["checkout"], "changes": [change]},
        {"rank": 2, "app": "checkout", "score": 1.0, "distance": 0.0, "explains": ["checkout"], "changes": []},
    ])
}

#[test]
fn suspects_of_an_incident_are_asked_for_its_application_since_the_day_before() {
    let incidents = format!(r#"[{{"id":"INC-1","service":"checkout","opened":{T}}},{{"id":"INC-2","service":"payments","opened":{}}},{{"id":"INC-3","service":"ledger-db","opened":{}}}]"#, T + 600, T - 7200);
    let root = workspace("suspects-incident", CATALOG, &[("incidents.json", &incidents)]);
    harvest(&root);
    assert_eq!(suspects(&root, "INC-1", None), (expected_suspects(), vec![]));
    // INC-2 opened inside the correlation window of INC-1, so its application fails with it;
    // INC-3 opened two hours earlier.
    let day_before = ograph_core::term::format_millis((T - 86_400) * 1000)[..10].to_owned();
    let opened = ograph_core::term::format_millis(T * 1000);
    assert_eq!(calls(&root).lines().last(), Some(format!("--suspects checkout,payments --since {day_before} --until {opened} . --json").as_str()));
}

#[test]
fn an_ogrep_that_cannot_stop_at_the_incident_is_asked_again_and_the_answer_says_so() {
    let root = workspace("suspects-old", CATALOG, &[("shop/.old", "")]);
    harvest(&root);
    let opened = ograph_core::term::format_millis(T * 1000);
    assert_eq!(suspects(&root, "INC-1", None), (expected_suspects(), vec![format!("shop: this ogrep cannot stop at {opened}, so commits made after it count too")]));
    let day_before = ograph_core::term::format_millis((T - 86_400) * 1000)[..10].to_owned();
    assert_eq!(calls(&root).lines().last(), Some(format!("--suspects checkout --since {day_before} . --json").as_str()));
}

#[test]
fn suspects_of_named_applications_look_back_a_day_unless_told() {
    let root = workspace("suspects-apps", CATALOG, &[("aliases.jsonl", "{\"app\":\"payments\",\"alias\":\"pay\"}\n")]);
    let config = std::fs::read_to_string(root.join(".ograph/config.toml")).unwrap();
    std::fs::write(root.join(".ograph/config.toml"), format!("{config}[[source]]\nkind = \"ownership\"\npath = \"aliases.jsonl\"\n")).unwrap();
    harvest(&root);
    assert_eq!(suspects(&root, "checkout, pay", None).0, expected_suspects());
    assert_eq!(calls(&root).lines().last(), Some("--suspects checkout,payments --since 24h . --json"));
    suspects(&root, "not-in-the-catalog", Some("3d"));
    assert_eq!(calls(&root).lines().last(), Some("--suspects not-in-the-catalog --since 3d . --json"));
}

#[test]
fn suspects_come_from_the_first_repository_ogrep_answers_for() {
    let sources = format!("[[source]]\nkind = \"code\"\npath = \"broken\"\n{CATALOG}");
    let root = workspace("suspects-degraded", &sources, &[("broken/.fail", "")]);
    harvest(&root);
    assert_eq!(suspects(&root, "INC-1", None), (expected_suspects(), vec!["broken: ogrep --suspects failed: boom".to_owned()]));
}

#[test]
fn chains_falls_back_to_suspects_only_when_nothing_ranks_and_code_is_configured() {
    let fallback = |root: &Path| {
        let ws = Workspace::open(root).unwrap();
        let snap = ws.store.snapshot().unwrap();
        let ids = ograph_core::correlate::Ids::new(&snap).unwrap();
        let analysis = ograph_core::query::chains(&snap, &ids, ograph_core::query::lookup(&snap, "INC-1").unwrap(), &ws.config.correlation, &mut Default::default()).unwrap();
        let (found, _) = ws.fallback_suspects(&snap, &analysis, "INC-1", &ws.config.correlation);
        (analysis.apps.len(), serde_json::to_value(ograph_core::query::Chains { analysis, suspects: found, shift: None }).unwrap())
    };
    // A commit twelve minutes before is a candidate, but alone it is too weak to rank.
    let quiet = workspace("fallback", &format!("{CATALOG}{CODE}"), &[]);
    harvest(&quiet);
    let (ranked, answer) = fallback(&quiet);
    assert_eq!((ranked, &answer["suspects"], &answer["candidates"]), (0, &expected_suspects(), &serde_json::json!(1)));

    let no_code = workspace("fallback-no-code", CATALOG, &[]);
    harvest(&no_code);
    let (ranked, answer) = fallback(&no_code);
    assert_eq!((ranked, answer.get("suspects")), (0, None));
    assert_eq!(calls(&no_code), "--files .\n--entities . --json --dated\n");

    let loud = workspace("fallback-ranked", &format!("{CATALOG}{CODE}{EVERYTHING}"), &[("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    harvest(&loud);
    let (ranked, answer) = fallback(&loud);
    assert!(ranked > 0);
    assert_eq!(answer.get("suspects"), None);
    assert!(!calls(&loud).contains("--suspects"));
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
fn a_missing_repository_is_noted_once_and_the_next_one_still_answers() {
    let sources = format!("{CATALOG}[[source]]\nkind = \"code\"\npath = \"missing\"\n{CODE}{EVERYTHING}");
    let root = workspace("code-missing", &sources, &[("logs.jsonl", &logs()), ("errors.jsonl", &errors())]);
    harvest(&root);
    let ws = Workspace::open(&root).unwrap();
    let answer = ws.code(&ws.store.snapshot().unwrap(), "INC-1", 5).unwrap();
    assert_eq!(answer.degraded.len(), 1, "{:?}", answer.degraded);
    assert!(answer.degraded[0].starts_with("missing: no repository at "), "{:?}", answer.degraded);
    assert_eq!((answer.files.len(), answer.writeups.len()), (2, 1));
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
