#!/usr/bin/env python3
"""Gate for valid time on dependency edges: a dependency added after an incident does not change
how ograph ranks that incident, and still counts for incidents after it.

A scripted git repository `shop` (Docker Compose): commit 1 on day 0 declares web -> api, with a
fraud service that nothing depends on; commit 2 on day 20 adds web -> fraud. Incident A opens on
web on day 10, incident B on day 30. Before each, api alerts 8 minutes before and fraud 2 minutes
before, so fraud ranks first whenever web -> fraud is followed.

Workspaces, each harvested from scratch:
  then    the repository checked out at commit 1, with the dating ogrep
  dated   the repository at commit 2, with the dating ogrep
  undated the repository at commit 2, with an ogrep that cannot date (--old-ogrep), this build
  base    the same as undated, with the build before this lane (--base-bin)

Pass: `chains INC-A --json` of dated equals that of then, apart from the `withheld` field that
names the dependency it left out and the store row numbers of signals (`id`, see below); undated ranks fraud first for A; dated ranks fraud first for B;
`before INC-A` of dated leaves fraud out of the scope and `before INC-B` keeps it; `blast fraud --at
INC-A` lists no web and `blast fraud` does; undated says in its harvest that the dependencies carry
no date, and its answers equal base's apart from store row numbers (the harvest note is one more
term in the store).

  edge_dates_gate.py --bin target/release/ograph --base-bin OLD --ogrep NEW_OGREP --old-ogrep OLD_OGREP
                     --work target/edge-dates-gate
"""
import argparse, json, os, pathlib, shutil, subprocess, sys

DAY = 86400
T0 = 1_767_225_600  # 2026-01-01T00:00:00Z
A, B = T0 + 10 * DAY, T0 + 30 * DAY

def compose(deps):
    lines = "".join(f"      - {d}\n" for d in deps)
    return f"services:\n  web:\n    image: shop/web\n    depends_on:\n{lines}  api:\n    image: shop/api\n  fraud:\n    image: shop/fraud\n"

def git(repo, *args, when=None):
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
    if when is not None:
        env["GIT_AUTHOR_DATE"] = env["GIT_COMMITTER_DATE"] = f"@{when} +0000"
    return subprocess.run(["git", *args], cwd=repo, env=env, check=True, capture_output=True, text=True).stdout.strip()

def alert(app, name, at):
    return {"labels": {"alertname": name, "service": app, "severity": "critical"}, "startsAt": at}

def workspace(binp, ogrep, w, repo):
    shutil.rmtree(w, ignore_errors=True); w.mkdir(parents=True)
    env = dict(os.environ, OGRAPH_OGREP=ogrep)
    subprocess.run([binp, "init"], cwd=w, env=env, check=True, capture_output=True)
    (w / ".ograph/config.toml").write_text("".join(f'[[source]]\nkind = "{k}"\npath = "{p}"\n' for k, p in
        [("catalog", str(repo)), ("incidents", "incidents.json"), ("alerts", "alerts.json")]) + "[correlation]\nknown_pairs = []\n")
    (w / "incidents.json").write_text(json.dumps([{"id": i, "service": "web", "opened": t, "severity": "SEV1", "status": "resolved"} for i, t in (("INC-A", A), ("INC-B", B))]))
    (w / "alerts.json").write_text(json.dumps([a for t in (A, B) for a in (alert("api", "ErrorRate", t - 900), alert("fraud", "ScoreTimeouts", t - 120))]))
    p = subprocess.run([binp, "harvest"], cwd=w, env=env, capture_output=True, text=True)
    assert p.returncode == 0, p.stderr
    def ask(*args):
        q = subprocess.run([binp, *args], cwd=w, env=env, capture_output=True, text=True)
        return q.stdout
    return p.stdout + p.stderr, ask

def main():
    ap = argparse.ArgumentParser()
    for k in ("--bin", "--base-bin", "--ogrep", "--old-ogrep", "--work"):
        ap.add_argument(k, required=True)
    a = ap.parse_args()
    binp, base = str(pathlib.Path(a.bin).resolve()), str(pathlib.Path(a.base_bin).resolve())
    work = pathlib.Path(a.work).resolve(); shutil.rmtree(work, ignore_errors=True)
    repo = work / "shop"; repo.mkdir(parents=True)
    git(repo, "init", "-q", "-b", "main")
    (repo / "compose.yaml").write_text(compose(["api"])); git(repo, "add", "."); git(repo, "commit", "-qm", "web calls api", when=T0)
    first = git(repo, "rev-parse", "HEAD")
    (repo / "compose.yaml").write_text(compose(["api", "fraud"])); git(repo, "add", "."); git(repo, "commit", "-qm", "web calls fraud", when=T0 + 20 * DAY)
    then_repo = work / "shop-then"
    git(repo, "worktree", "add", "-q", "--detach", str(then_repo), first)

    _, then = workspace(binp, a.ogrep, work / "then", then_repo)
    dated_log, dated = workspace(binp, a.ogrep, work / "dated", repo)
    undated_log, undated = workspace(binp, a.old_ogrep, work / "undated", repo)
    _, old = workspace(base, a.old_ogrep, work / "base", repo)

    def chains(ask, inc):
        return json.loads(ask("chains", inc, "--json"))
    def rank1(ask, inc):
        apps = chains(ask, inc)["apps"]
        return apps[0]["app"] if apps else None
    checks = []
    def check(name, ok, detail=""):
        checks.append(ok); print(f"{'PASS' if ok else 'FAIL'}  {name}{': ' + detail if detail else ''}")

    print("dated harvest:\n" + "".join("  " + l + "\n" for l in dated_log.splitlines()))
    print("undated harvest:\n" + "".join("  " + l + "\n" for l in undated_log.splitlines()))
    print("chains INC-A, dated:\n" + "".join("  " + l + "\n" for l in dated("chains", "INC-A").splitlines()))
    print("chains INC-A, undated:\n" + "".join("  " + l + "\n" for l in undated("chains", "INC-A").splitlines()))
    print("chains INC-B, dated:\n" + "".join("  " + l + "\n" for l in dated("chains", "INC-B").splitlines()))

    ca, ta = chains(dated, "INC-A"), chains(then, "INC-A")
    withheld = ca.pop("withheld", [])
    # A signal's `id` is its row number in the store, which counts the terms interned before it:
    # the dated store holds five more. Its `iri` names it.
    def rows(x):
        if isinstance(x, dict):
            return {k: rows(v) for k, v in x.items() if not (k == "id" and isinstance(v, int))}
        return [rows(v) for v in x] if isinstance(x, list) else x
    differ = [k for k in ("id",) if json.dumps(ca, sort_keys=True) != json.dumps(ta, sort_keys=True)]
    check("chains INC-A with dated edges equals chains INC-A at commit 1, apart from `withheld` and store row numbers",
          json.dumps(rows(ca), sort_keys=True) == json.dumps(rows(ta), sort_keys=True),
          f"rank 1 {rank1(dated, 'INC-A')} and {rank1(then, 'INC-A')}; byte for byte {'different only in store row numbers' if differ else 'identical'}")
    check("chains INC-A names the dependency it did not follow", [(w["caller"], w["callee"]) for w in withheld] == [("web", "fraud")], json.dumps(withheld))
    check("chains INC-A without dates ranks fraud first", rank1(undated, "INC-A") == "fraud", str(rank1(undated, "INC-A")))
    check("chains INC-B with dated edges still ranks fraud first", rank1(dated, "INC-B") == "fraud", str(rank1(dated, "INC-B")))
    scope = lambda ask, inc: sorted(a for a, _ in json.loads(ask("before", inc, "--json"))["scope"])
    check("before INC-A leaves fraud out of the scope", scope(dated, "INC-A") == ["api", "web"], str(scope(dated, "INC-A")))
    check("before INC-B keeps fraud in the scope", scope(dated, "INC-B") == ["api", "fraud", "web"], str(scope(dated, "INC-B")))
    blast = lambda *args: [d for d, _ in json.loads(dated("blast", "fraud", *args, "--json"))["dependents"]]
    check("blast fraud --at INC-A lists no web", blast("--at", "INC-A") == [], str(blast("--at", "INC-A")))
    check("blast fraud lists web", blast() == ["web"], str(blast()))
    check("an ogrep that cannot date: the harvest says so", "dependencies carry no date" in undated_log)
    queries = [["chains", i, "--json"] for i in ("INC-A", "INC-B")] + [["before", i, "--json"] for i in ("INC-A", "INC-B")] + [["blast", "fraud", "--json"], ["chains", "INC-A"], ["before", "INC-A"]]
    def norm(q, text):
        return json.dumps(rows(json.loads(text)), sort_keys=True) if "--json" in q and text else text
    same = [q for q in queries if norm(q, undated(*q)) == norm(q, old(*q))]
    exact = sum(undated(*q) == old(*q) for q in queries)
    check("an ogrep that cannot date: answers equal the build before this lane, apart from store row numbers", len(same) == len(queries),
          f"{len(same)} of {len(queries)} identical apart from store row numbers, {exact} byte for byte" + "".join(f"; differs: {' '.join(q)}" for q in queries if q not in same))
    print(f"\n{'GATE PASSED' if all(checks) else 'GATE FAILED'}: {sum(checks)} of {len(checks)} checks")
    sys.exit(0 if all(checks) else 1)

if __name__ == "__main__":
    main()
