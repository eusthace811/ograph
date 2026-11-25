#!/usr/bin/env python3
"""Generate the distractor suite: labeled incidents, each with one planted root cause and one
planted distractor of a named type, in the case layout agent_rca_eval.py and rcaeval_score.py
read (one directory per case with .ograph/config.toml and its source files, and labels.json
beside them).

Every case is a small system drawn from one of seven domains and one of five topologies, with
service names unlike RCAEval's. One application (the root) has a fault at the onset: its fault
metric steps or ramps, it writes fault errors to its log, and every application that calls it,
directly or through others, slows down and logs failed calls to its callee after a lag that grows
with the distance. The incident opens on the entry application 4 to 7 minutes after the onset,
with an alert on the entry a minute before. Each case also holds benign history: deployments
hours earlier, a config change days earlier, a resolved incident on another application.

Then one distractor, the case's type:

  a  chronic    a noisy series on an upstream dependency (a callee of the root when it has one):
                a spike of 3 samples every 3.5 to 6.5 minutes for the whole hour, one of them
                1 to 2.5 minutes before the onset. Nothing changes at the onset.
  b  deploy     a deployment of a healthy application (not the root, nothing it calls is
                faulty) 1.5 to 4 minutes before the onset, with its start-up log lines. Its
                metrics stay flat.
  c  alert      a critical alert on a downstream application (a caller of the root, not the
                entry when the root lies deeper) that fires before any other alert, seconds
                after its latency starts rising. The root has no alert.
  d  sparse     a series that is zero for the whole hour except one burst of 3 to 4 samples
                1 to 3.5 minutes before the onset, on an application outside the fault's path:
                its baseline has no spread, so any detector scoring by robust z saturates.
  e  precedent  two resolved past incidents on the entry, titled like this one, whose
                mitigations name another application as the cause, with alerts on the entry and
                on that application at the time. The root differs this time.

Labels hold, per case: root, fault (the root's fault kind), rep (1), inject (the onset, Unix
seconds), incident, traces (false), and the distractor's type, name and application, the domain,
the topology and the root's callers. Labels are by construction: the generator decides the root,
the onset and the distractor, then writes the telemetry from them.

Deterministic: the same --seed writes byte-identical files (each case draws from its own
random.Random seeded with "<seed>/<index>"; floats are written with fixed precision).

The catalog of each case is a one-commit git repository (ogrep reads entities only from one),
so the cases are generated, not committed: examples/distractors/cases is ignored by git, and
SHA256SUMS beside this script lists every generated file outside catalog/.git. Check a
regeneration with `cd examples/distractors/cases && shasum -a 256 -c ../SHA256SUMS`.

Usage: python3 examples/distractors/generate.py [--seed 2026] [--per-type 7] [--out DIR]
       (DIR defaults to examples/distractors/cases; it is replaced)"""
import argparse, hashlib, json, os, pathlib, random, shutil, subprocess

DOMAINS = {
    "logistics": ("dispatch-portal", ["route-planner", "parcel-tracker", "fleet-api", "label-maker", "depot-sync"], ["shipments-pg", "geo-kv", "scan-stream"]),
    "media": ("watch-web", ["playback-api", "title-index", "license-check", "ad-picker", "subtitle-svc"], ["media-meta-db", "session-kv", "origin-store"]),
    "banking": ("teller-gateway", ["account-api", "fraud-score", "fx-quote", "statement-gen", "limits-api"], ["accounts-pg", "rate-kv", "audit-journal"]),
    "health": ("patient-portal", ["appointment-api", "records-api", "rx-router", "coverage-check", "notify-hub"], ["emr-db", "slots-kv", "hl7-bus"]),
    "gaming": ("lobby-edge", ["matchmaker", "item-vault", "rank-board", "chat-relay", "cheat-guard"], ["player-db", "match-kv", "event-stream"]),
    "travel": ("booking-web", ["fare-engine", "seat-map", "pnr-api", "loyalty-api", "voucher-api"], ["fares-db", "avail-kv", "gds-bridge"]),
    "iot": ("device-hub", ["ingest-api", "rules-engine", "firmware-api", "twin-registry", "alarm-api"], ["series-db", "shadow-kv", "mqtt-core"]),
}
# Callers to callees over roles: E is the entry, A-D services, S1-S2 stores.
TOPOLOGIES = {
    "chain": [("E", "A"), ("A", "B"), ("B", "S1"), ("A", "S2")],
    "fan": [("E", "A"), ("E", "B"), ("E", "C"), ("A", "S1"), ("B", "S1"), ("C", "S2")],
    "diamond": [("E", "A"), ("E", "B"), ("A", "C"), ("B", "C"), ("C", "S1"), ("B", "S2")],
    "shared": [("E", "A"), ("E", "B"), ("A", "S1"), ("B", "S1"), ("A", "C"), ("C", "S2")],
    "deep": [("E", "A"), ("A", "B"), ("B", "C"), ("C", "S1"), ("A", "D"), ("D", "S2")],
}
TYPES = {"a": "chronic", "b": "deploy", "c": "alert", "d": "sparse", "e": "precedent"}
STEP = 10           # metric sample period, seconds
SPAN = 3600         # an hour of telemetry before the incident opens, and a few minutes after
T_BASE = 1761955200  # 2025-11-01T00:00:00Z

SERVICE_FAULTS = {
    "cpu": ("cpu", "CPU throttled: worker pool saturated, run queue {q}"),
    "mem": ("mem", "heap usage {q}% of limit, GC overhead limit exceeded"),
    "errors": ("errors", "upstream TLS handshake failed: certificate verify failed (serial {q})"),
    "latency": ("latency-p99", "slow handler: request took {q}ms (deadline 800ms)"),
}
STORE_FAULTS = {
    "pool": ("connections", "too many connections: {q} of 200 slots in use, rejecting client"),
    "disk": ("disk-used", "could not write to WAL: No space left on device ({q} MB free)"),
    "latency": ("latency-p99", "slow query: {q}ms waiting on lock, lock wait timeout exceeded"),
}


def names(rng, domain, topo):
    entry, services, stores = DOMAINS[domain]
    sv, st = services[:], stores[:]
    rng.shuffle(sv); rng.shuffle(st)
    roles = {r for e in TOPOLOGIES[topo] for r in e}
    m = {"E": entry}
    for r in sorted(roles):
        if r[0] in "ABCD": m[r] = sv["ABCD".index(r)]
        elif r[0] == "S": m[r] = st[int(r[1]) - 1]
    return m


def graph(topo, m):
    deps = {m[r]: [] for r in m}
    for a, b in TOPOLOGIES[topo]: deps[m[a]].append(m[b])
    return deps


def closure(deps, start, reverse=False):
    """Distance of every application reachable from `start` along calls (or against them)."""
    edges = {a: [] for a in deps}
    for a, bs in deps.items():
        for b in bs:
            (edges[b] if reverse else edges[a]).append(a if reverse else b)
    dist, todo = {start: 0}, [start]
    while todo:
        x = todo.pop(0)
        for y in edges[x]:
            if y not in dist: dist[y] = dist[x] + 1; todo.append(y)
    return dist


def is_store(app, domain): return app in DOMAINS[domain][2]


def pick_root_and_distractor(rng, kind, deps, entry, domain):
    apps = sorted(deps)
    options = []
    for root in apps:
        if root == entry: continue
        callers = closure(deps, root, reverse=True)  # root and everything that calls it
        if entry not in callers: continue
        below = [x for x in closure(deps, root) if x != root]
        healthy = [x for x in apps if x not in callers and x not in below]
        if kind == "a": cands = below or [x for x in closure(deps, entry) if x not in callers]
        elif kind == "b": cands = [x for x in healthy if not is_store(x, domain)] or healthy
        elif kind == "c": cands = [x for x in callers if x not in (root, entry)] or ([entry] if callers[root] == 1 else [])
        elif kind == "d": cands = [x for x in healthy + below if is_store(x, domain)] or healthy + below
        else: cands = [x for x in apps if x not in callers and x not in below] or [x for x in apps if x not in (root, entry)]
        for x in cands: options.append((root, x))
    return rng.choice(options)


def fmt(v): return f"{v:.3f}"


def case(seed, index, kind, attempt=0):
    rng = random.Random(f"{seed}/{index}/{attempt}")
    domain = sorted(DOMAINS)[(index * 3 + "abcde".index(kind)) % len(DOMAINS)]
    topo = sorted(TOPOLOGIES)[(index + 2 * "abcde".index(kind)) % len(TOPOLOGIES)]
    m = names(rng, domain, topo)
    deps = graph(topo, m)
    entry = m["E"]
    root, lure = pick_root_and_distractor(rng, kind, deps, entry, domain)
    store = is_store(root, domain)
    faults = STORE_FAULTS if store else SERVICE_FAULTS
    fault = sorted(faults)[rng.randrange(len(faults))]
    fault_metric, fault_log = faults[fault]
    callers = closure(deps, root, reverse=True)
    t0 = T_BASE + index * 86400 * 3 + rng.randrange(0, 86400 - 2 * SPAN)
    onset = t0 + 2400 + rng.randrange(-120, 121)
    opened = onset + rng.randrange(240, 421)
    end = opened + 300
    incident = f"INC-{7000 + index * 37 + rng.randrange(0, 30)}"
    lag = {a: 0 if a == root else d * rng.randrange(15, 41) + rng.randrange(0, 10) for a, d in callers.items()}
    apps = sorted(deps)

    # Metric columns: every application has latency, errors and cpu; services mem, stores
    # connections and disk-used; the distractors add their own series.
    base = {a: {"latency-p99": rng.uniform(25, 120), "errors": rng.uniform(0.1, 0.6), "cpu": rng.uniform(15, 40),
                ("connections" if is_store(a, domain) else "mem"): rng.uniform(30, 60), **({"disk-used": rng.uniform(40, 70)} if is_store(a, domain) else {})} for a in apps}
    lure_col = None
    spikes = []
    if kind == "a":
        lure_col = (lure, "gc-pause")
        t = t0 + rng.randrange(60, 200)
        before = onset - rng.randrange(60, 151)
        while t < end:
            spikes.append(t); t += rng.randrange(210, 391)
        spikes = [s for s in spikes if abs(s - before) > 150] + [before]
        spikes.sort()
    if kind == "d":
        lure_col = (lure, "snapshot-io")
        burst = onset - rng.randrange(60, 211)
    cols = [(a, k) for a in apps for k in base[a]] + ([lure_col] if lure_col else [])
    rows = ["time," + ",".join(f"{a}_{k}" for a, k in cols)]
    for t in range(t0, end + 1, STEP):
        vals = []
        for a, k in cols:
            if (a, k) == lure_col:
                if kind == "a":
                    v = 18 + rng.gauss(0, 2.5) + (rng.uniform(380, 520) if any(0 <= t - s < 3 * STEP for s in spikes) else 0)
                else:
                    v = rng.uniform(4.0e7, 9.0e7) if 0 <= t - burst < rng.choice((3, 4)) * STEP else 0.0
                vals.append(fmt(max(v, 0.0))); continue
            b = base[a][k]
            v = b * (1 + rng.gauss(0, 0.04)) if k != "errors" else max(b + rng.gauss(0, 0.05), 0.0)
            if a in callers and t >= onset + lag[a]:
                ramp = min(1.0, (t - onset - lag[a]) / 90 + 0.3)
                if a == root:
                    if k == fault_metric:
                        v = {"cpu": 96, "mem": 97, "errors": 34, "latency-p99": b * 9, "connections": 200, "disk-used": 100}[k] * (ramp if k in ("mem", "disk-used") else 1) + rng.gauss(0, 1)
                        if k in ("mem", "disk-used"): v = max(v, b)
                    elif k == "latency-p99": v = b * (3 + 2 * ramp) * (1 + rng.gauss(0, 0.05))
                    elif k == "errors" and fault in ("errors", "pool", "disk", "mem"): v = 6 + 10 * ramp + rng.gauss(0, 0.5)
                else:
                    if k == "latency-p99": v = b * (2 + 3 * ramp) * (1 + rng.gauss(0, 0.05))
                    elif k == "errors": v = 2 + 6 * ramp + rng.gauss(0, 0.3)
            vals.append(fmt(v))
        rows.append(f"{t}," + ",".join(vals))

    # Logs: a routine line per application every 40 s; fault lines on the root from the onset,
    # failed calls on each caller after its lag; the distractor's own lines.
    logs = []
    for t in range(t0, end + 1, 2):
        for a in apps:
            if (t - t0) % 40 == hash_slot(a) % 20 * 2:
                logs.append((t, a, f"handled {route(a)} 200 in {int(base[a]['latency-p99'] * rng.uniform(0.3, 0.9))}ms"))
        for a, d in callers.items():
            if t < onset + lag[a] or rng.random() > 0.35: continue
            if a == root:
                logs.append((t, a, fault_log.format(q=rng.randrange(120, 990))))
            else:
                callee = next(x for x in deps[a] if x in callers and callers[x] < d)
                logs.append((t, a, f"call to {callee} failed: deadline exceeded after {rng.choice((800, 1000, 2000))}ms"))
    if kind == "a":
        for s in spikes: logs.append((s + 5, lure, f"long GC pause {rng.randrange(380, 520)}ms (young gen)"))
    if kind == "d":
        logs.append((burst, lure, "snapshot started")); logs.append((burst + 40, lure, "snapshot finished, 0 errors"))
    deploys = [{"service": rng.choice(apps), "version": version(rng), "at": t0 - rng.randrange(3 * 3600, 30 * 3600)} for _ in range(rng.randrange(2, 4))]
    if kind == "b":
        at = onset - rng.randrange(90, 241)
        deploys.append({"service": lure, "version": version(rng), "at": at})
        logs += [(at + 3, lure, "shutting down for new version"), (at + 12, lure, f"starting {lure} {deploys[-1]['version']}"), (at + 20, lure, "ready: listening on :8080")]
    deploys.sort(key=lambda d: d["at"])
    logs.sort(key=lambda x: (x[0], x[1], x[2]))
    configs = [{"service": rng.choice(apps), "key": rng.choice(["max_workers", "timeout_ms", "cache_ttl_s", "retry_budget"]), "at": t0 - rng.randrange(2 * 86400, 6 * 86400)}]

    title = rng.choice(["{e} errors and slow pages", "{e} degraded", "{e} timeouts for users", "{e} requests failing"]).format(e=entry)
    alerts = [{"labels": {"alertname": camel(entry) + "ErrorRate", "service": entry, "severity": "critical"}, "startsAt": opened - 60}]
    if kind == "c":
        alerts.append({"labels": {"alertname": camel(lure) + "LatencyHigh", "service": lure, "severity": "critical"}, "startsAt": onset + lag[lure] + rng.randrange(5, 20)})
    benign = rng.choice([a for a in apps if a not in (entry, root, lure)] or [entry])
    incidents = [{"id": f"INC-{5000 + index * 11 + rng.randrange(0, 10)}", "title": f"{benign} elevated latency", "service": benign, "opened": t0 - rng.randrange(8, 25) * 86400,
                  "severity": "SEV3", "status": "resolved", "mitigation": f"restarted {benign} after a stuck worker"}]
    incidents[0]["resolved"] = incidents[0]["opened"] + 3600
    if kind == "e":
        for k, days in enumerate(sorted(rng.sample(range(9, 40), 2))):
            at = t0 - days * 86400 + rng.randrange(0, 40000)
            mit = rng.choice(["failed over {x} to its replica; {x} was the cause", "restarted {x}, its connection pool had leaked", "rolled back {x}; root cause in {x}"]).format(x=lure)
            incidents.append({"id": f"INC-{6000 + index * 13 + k}", "title": title, "service": entry, "opened": at, "severity": "SEV2", "status": "resolved", "resolved": at + 2700, "mitigation": mit})
            alerts += [{"labels": {"alertname": camel(lure) + "Saturated", "service": lure, "severity": "critical"}, "startsAt": at - 400},
                       {"labels": {"alertname": camel(entry) + "ErrorRate", "service": entry, "severity": "critical"}, "startsAt": at - 60}]
    incidents.sort(key=lambda i: i["opened"])
    incidents.append({"id": incident, "title": title, "service": entry, "opened": opened, "severity": "SEV2", "status": "open"})
    alerts.sort(key=lambda a: a["startsAt"])

    catalog = []
    for a in apps:
        d = f"apiVersion: backstage.io/v1alpha1\nkind: Component\nmetadata:\n  name: {a}\nspec:\n  type: {'database' if is_store(a, domain) else 'service'}\n  owner: team-{a.split('-')[0]}\n  system: {domain}\n"
        if deps[a]: d += "  dependsOn:\n" + "".join(f"    - component:{x}\n" for x in deps[a])
        catalog.append(d)
    ownership = [{"app": entry, "process": f"{domain} customers"}] + [{"app": a, "process": f"{domain} operations"} for a in apps if a != entry and not is_store(a, domain)][:2]
    files = {
        ".ograph/config.toml": CONFIG,
        "catalog/catalog-info.yaml": "---\n".join(catalog),
        "ownership.jsonl": "".join(json.dumps(o) + "\n" for o in ownership),
        "incidents.json": json.dumps(incidents, indent=1) + "\n",
        "alerts.json": json.dumps({"alerts": alerts}) + "\n",
        "deployments.jsonl": "".join(json.dumps(d) + "\n" for d in deploys),
        "config.jsonl": "".join(json.dumps(c) + "\n" for c in configs),
        "logs.jsonl": "".join(json.dumps({"ts": t, "service": a, "message": msg}) + "\n" for t, a, msg in logs),
        "metrics.csv": "\n".join(rows) + "\n",
    }
    label = {"root": root, "fault": fault, "rep": 1, "inject": onset, "incident": incident, "traces": False,
             "distractor": kind, "distractor_type": TYPES[kind], "distractor_app": lure, "domain": domain, "topology": topo,
             "callers": sorted(a for a in callers if a != root)}
    return files, label, t0 - 30 * 86400


def git_catalog(d, at):
    """The catalog as a one-commit git repository: ogrep reads entities only from one. Its
    files are part of the suite; the .git directory is not (git's own bytes vary by version)."""
    env = {**os.environ, "GIT_AUTHOR_DATE": f"@{at} +0000", "GIT_COMMITTER_DATE": f"@{at} +0000", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    git = lambda *a: subprocess.run(["git", "-c", "user.name=suite", "-c", "user.email=suite@example.invalid", "-c", "init.defaultBranch=main", *a], cwd=d, env=env, check=True, capture_output=True)
    git("init", "-q"); git("add", "-A"); git("commit", "-qm", "catalog")


def hash_slot(a): return int(hashlib.sha256(a.encode()).hexdigest()[:8], 16)
def route(a): return "/" + a.split("-")[0] + "/" + ("get" if hash_slot(a) % 2 else "list")
def camel(a): return "".join(p.capitalize() for p in a.split("-"))
def version(rng): return f"{rng.randrange(1, 9)}.{rng.randrange(0, 30)}.{rng.randrange(0, 12)}"


CONFIG = """[[source]]
kind = "catalog"
path = "catalog"

[[source]]
kind = "ownership"
path = "ownership.jsonl"

[[source]]
kind = "deployments"
path = "deployments.jsonl"

[[source]]
kind = "config_changes"
path = "config.jsonl"

[[source]]
kind = "alerts"
path = "alerts.json"

[[source]]
kind = "incidents"
path = "incidents.json"

[[source]]
kind = "logs"
path = "logs.jsonl"

[[source]]
kind = "metrics"
path = "metrics.csv"

[correlation]
window_s = 1800
"""


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--seed", default="2026"); ap.add_argument("--per-type", type=int, default=7)
    ap.add_argument("--out", default=str(pathlib.Path(__file__).resolve().parent / "cases"))
    a = ap.parse_args(argv)
    out = pathlib.Path(a.out)
    shutil.rmtree(out, ignore_errors=True)
    labels, cells = {}, set()
    index = 0
    for kind in "abcde":
        for _ in range(a.per_type):
            # agent_rca_eval.py samples one case per (root, fault) cell: draw again until the
            # case's cell is new.
            attempt = 0
            while True:
                files, label, catalog_at = case(a.seed, index, kind, attempt)
                if (label["root"], label["fault"]) not in cells: break
                attempt += 1
            cid = "x-" + hashlib.sha256(f"{a.seed}/{index}".encode()).hexdigest()[:10]
            cells.add((label["root"], label["fault"]))
            for rel, text in files.items():
                p = out / cid / rel
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(text)
            git_catalog(out / cid / "catalog", catalog_at)
            labels[cid] = label
            index += 1
    (out / "labels.json").write_text(json.dumps(labels, indent=1, sort_keys=True) + "\n")
    print(f"{len(labels)} cases in {out}")


if __name__ == "__main__":
    main()
