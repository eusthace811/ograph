#!/usr/bin/env python3
"""Export RCAEval RE2-OB cases into ograph workspaces with opaque ids.

The case folder name holds the answer (root-cause service and fault), so ograph never sees it:
each case becomes `OUT/c-<hash>/` holding the telemetry in ograph's input formats and a
`.ograph/config.toml`, while the labels go to `OUT/labels.json`, which only the scorer reads.

The incident is opened on the user-facing `frontend` `--detect-lag` seconds after the
injection, as an alert-driven incident would be. Nothing else about the fault is exported.

Usage: rcaeval_export.py --data data/rcaeval/RE2-OB --out data/rcaeval-cases [--detect-lag 300]
Needs pyarrow on PYTHONPATH.
"""
import argparse, csv, hashlib, json, pathlib
import pyarrow.parquet as pq

FIXTURE = pathlib.Path(__file__).resolve().parents[2] / "data" / "fixtures" / "online-boutique"
OWNERSHIP = [
    {"app": "frontend", "alias": "frontendservice"},
    {"app": "frontend", "process": "storefront"},
    {"app": "checkoutservice", "process": "checkout"},
    {"app": "paymentservice", "process": "payments"},
    {"app": "emailservice", "process": "order notifications"},
    {"app": "shippingservice", "process": "fulfilment"},
    {"app": "paymentservice", "vendor": "CardRail"},
    {"app": "emailservice", "vendor": "MailRelay"},
    {"app": "currencyservice", "vendor": "FX Feed"},
]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--detect-lag", type=int, default=300)
    ap.add_argument("--no-traces", action="store_true")
    a = ap.parse_args()
    out = pathlib.Path(a.out); out.mkdir(parents=True, exist_ok=True)
    labels = {}
    for case in sorted(pathlib.Path(a.data).glob("re2ob_*")):
        _, svc, fault, rep = case.name.split("_")
        cid = "c-" + hashlib.sha256(case.name.encode()).hexdigest()[:10]
        inject = int((case / "inject_time.txt").read_text().strip())
        labels[cid] = {"root": svc, "fault": fault, "rep": int(rep), "inject": inject}
        d = out / cid; (d / ".ograph").mkdir(parents=True, exist_ok=True)
        m = pq.read_table(case / "metrics.parquet")
        cols = m.column_names
        with open(d / "metrics.csv", "w", newline="") as f:
            w = csv.writer(f); w.writerow(cols)
            for row in zip(*[m.column(c).to_pylist() for c in cols]):
                w.writerow(["" if v is None else v for v in row])
        lg = pq.read_table(case / "logs.parquet")
        with open(d / "logs.jsonl", "w") as f:
            for ts, svcn, msg in zip(lg.column("timestamp").to_pylist(), lg.column("container_name").to_pylist(), lg.column("message").to_pylist()):
                f.write(json.dumps({"ts": ts, "service": svcn, "message": msg}) + "\n")
        sources = [
            ("catalog", str(FIXTURE)), ("ownership", "ownership.jsonl"), ("incidents", "incidents.json"),
            ("logs", "logs.jsonl"), ("metrics", "metrics.csv"),
        ]
        tp = case / "traces.parquet"
        if tp.exists() and not a.no_traces:
            t = pq.read_table(tp, columns=["traceID", "spanID", "parentSpanID", "serviceName", "startTimeMillis", "duration", "statusCode"])
            with open(d / "traces.jsonl", "w") as f:
                for tr, sp, pa, sv, st, du, sc in zip(*[t.column(c).to_pylist() for c in t.column_names]):
                    f.write(json.dumps({"trace_id": tr, "span_id": sp, "parent_span_id": pa, "service": sv, "start_ms": st, "duration_us": du, "status": sc}) + "\n")
            sources.append(("traces", "traces.jsonl"))
        with open(d / "ownership.jsonl", "w") as f:
            for r in OWNERSHIP: f.write(json.dumps(r) + "\n")
        inc = {"id": f"INC-{cid[2:8].upper()}", "title": "Storefront degraded", "service": "frontend",
               "opened": inject + a.detect_lag, "severity": "SEV2", "status": "open"}
        (d / "incidents.json").write_text(json.dumps([inc]))
        cfg = "".join(f'[[source]]\nkind = "{k}"\npath = "{p}"\n\n' for k, p in sources)
        (d / ".ograph" / "config.toml").write_text(cfg + "[correlation]\nwindow_s = 1800\n")
        labels[cid]["incident"] = inc["id"]; labels[cid]["traces"] = any(k == "traces" for k, _ in sources)
        print(cid, len(sources), "sources")
    (out / "labels.json").write_text(json.dumps(labels, indent=1))

if __name__ == "__main__":
    main()
