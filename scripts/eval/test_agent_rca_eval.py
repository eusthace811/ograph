#!/usr/bin/env python3
"""Tests for agent_rca_eval.py that need no model: scoring, sampling, the leak detector on
planted leaks, and the harness end to end with stand-in arms.

Usage: python3 scripts/eval/test_agent_rca_eval.py [--bin target/release/ograph]"""
import contextlib, io, json, os, pathlib, subprocess, sys, tempfile, types, unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import agent_rca_eval as h

BIN = pathlib.Path(sys.argv.pop(sys.argv.index("--bin") + 1) if "--bin" in sys.argv else h.REPO / "target" / "release" / "ograph")
if "--bin" in sys.argv: sys.argv.remove("--bin")


def bash(command): return {"type": "tool_execution_start", "toolName": "bash", "args": {"command": command}}
def output(tool, text): return {"type": "tool_execution_end", "toolName": tool, "result": {"content": [{"type": "text", "text": text}]}}


class Scoring(unittest.TestCase):
    def test_root_line(self):
        self.assertEqual(h.parse_root("I think it is cart.\nROOT: currencyservice"), "currencyservice")
        self.assertEqual(h.parse_root("ROOT: frontend\nmore\n**ROOT:** `Ledger-DB`."), "ledger-db")
        self.assertEqual(h.parse_root("ROOT: id:app/payments_api (a hypothesis)"), "payments_api")
        self.assertEqual(h.parse_root("The root cause is emailservice."), "")

    def test_verdict(self):
        self.assertEqual(h.verdict(["pass", "pass", "pass"]), "stable_pass")
        self.assertEqual(h.verdict(["pass", "fail", "pass"]), "flaky")
        self.assertEqual(h.verdict(["fail", "fail"]), "stable_fail")
        self.assertEqual(h.verdict(["pass", "error", "pass"]), "error")
        self.assertEqual(h.verdict(["pass", "leak"]), "error")
        self.assertEqual(h.verdict([]), "missing")

    def test_sign_test(self):
        self.assertEqual(h.sign_test(5, 0), 0.0625)
        self.assertEqual(h.sign_test(3, 3), 1.0)
        self.assertEqual(h.sign_test(0, 0), 1.0)

    def test_hypothesis(self):
        self.assertTrue(h.HYPOTHESIS.search("This is a hypothesis until a person confirms it."))
        self.assertTrue(h.HYPOTHESIS.search("Most likely cause, not yet confirmed."))
        self.assertFalse(h.HYPOTHESIS.search("The root cause is emailservice."))

    def test_sample_is_one_per_cell_and_a_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            d = pathlib.Path(d)
            labels = {f"c-{s}{f}{r}": {"root": s, "fault": f, "rep": r, "incident": f"INC-{s}{f}{r}".upper()} for s in "ab" for f in "xy" for r in (1, 2, 3)}
            (d / "labels.json").write_text(json.dumps(labels))
            four, two = h.rcaeval_cases(d, 4, 12), h.rcaeval_cases(d, 2, 12)
            self.assertEqual(sorted((labels[c.id]["root"], labels[c.id]["fault"]) for c in four), [("a", "x"), ("a", "y"), ("b", "x"), ("b", "y")])
            self.assertEqual([c.id for c in two], [c.id for c in four][:2])
            self.assertEqual([c.id for c in four], [c.id for c in h.rcaeval_cases(d, 4, 12)])


class Leaks(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(os.path.realpath(self.tmp.name))
        self.cases = root / "data" / "cases"; self.cases.mkdir(parents=True)
        self.ctx = h.Ctx(types.SimpleNamespace(work=str(root / "work"), cases=str(self.cases), bin="unused"))
        self.ws = self.ctx.work / "runs" / "ab12" / "ops"; self.ws.mkdir(parents=True)
        (self.ws / "metrics.csv").write_text("time,checkoutservice_cpu\n1,0.5\n")
        (self.ws / "incidents.json").write_text('[{"id": "INC-1", "service": "frontend"}]')

    def tearDown(self): self.tmp.cleanup()

    def test_clean_workspace(self):
        self.assertEqual(h.leaks_in_dir(self.ctx, self.ws), [])

    def test_planted_files(self):
        (self.ws / "labels.json").write_text('{"c-1": {"root": "checkoutservice", "fault": "cpu", "inject": 5}}')
        self.assertEqual(h.leaks_in_dir(self.ctx, self.ws), ["labels.json: file name", "labels.json: label-shaped JSON keys"])
        (self.ws / "labels.json").unlink()
        (self.ws / "notes.txt").write_text(f"exported from {self.cases}/c-1 of RCAEval")
        self.assertEqual(h.leaks_in_dir(self.ctx, self.ws), ["notes.txt: contains 'rcaeval'", f"notes.txt: contains '{str(self.cases).lower()}'"])
        (self.ws / "notes.txt").unlink()
        (self.ws / "answer").symlink_to(self.cases)
        self.assertEqual(h.leaks_in_dir(self.ctx, self.ws), [f"answer: symlink to {self.cases}"])

    def test_clean_transcript(self):
        events = [bash(f"ograph chains INC-1 --json && awk -F, '$2 > 1/2' {self.ws}/metrics.csv && python3 /tmp/x.py > /dev/null"),
                  {"type": "tool_execution_start", "toolName": "ograph_sparql", "args": {"query": "SELECT ?a WHERE { ?a ops:dependsOn+ id:app\\/ledger-db . <https://ograph.dev/x> ?p ?o }"}},
                  bash(f"cat {self.ctx.skill}/references/sparql.md"), output("bash", '{"root": "x", "apps": []}')]
        self.assertEqual(h.leaks_in_transcript(self.ctx, self.ws, events, h.ARMS["skill"]), [])

    def test_planted_transcripts(self):
        leaks = lambda events, arm="skill": h.leaks_in_transcript(self.ctx, self.ws, events, h.ARMS[arm])
        self.assertEqual(leaks([bash(f"cat {self.cases}/labels.json")]),
                         [f"bash: asks for '{str(self.cases).lower()}'", "bash: asks for 'labels.json'", f"bash: reaches {self.cases}/labels.json"])
        self.assertEqual(leaks([bash("ls ../../")]), ["bash: climbs out with .."])
        self.assertEqual(leaks([bash("ls ~/.cargo")]), ["bash: reaches the home directory"])
        self.assertEqual(leaks([bash(f"ls {self.ctx.work}/runs")]), [f"bash: reaches {self.ctx.work}/runs"])
        self.assertEqual(leaks([bash("find /Volumes -name '*.json'")]), ["bash: reaches /Volumes"])
        self.assertEqual(leaks([output("bash", "RCAEval RE2-OB checkoutservice_cpu_1")]), ["bash: output contains 'rcaeval'"])
        self.assertEqual(leaks([bash("ograph chains INC-1")], "raw"), ["bash: uses ograph"])
        self.assertEqual(leaks([bash("ls")], "mcp"), ["bash: not an ograph tool"])


@unittest.skipUnless(BIN.exists(), f"{BIN} is not built")
class EndToEnd(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(os.path.realpath(self.tmp.name))
        cases = self.root / "cases"
        labels = {}
        catalog = self.root / "fixtures" / "shop"; catalog.mkdir(parents=True)
        (catalog / "catalog-info.yaml").write_text("---\n".join(
            f"apiVersion: backstage.io/v1alpha1\nkind: Component\nmetadata:\n  name: {n}\nspec:\n  type: service\n  owner: team\n" for n in ("web", "cart", "mail")))
        for cmd in (["init", "-q"], ["add", "-A"], ["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-qm", "catalog"]):
            subprocess.run(["git", *cmd], cwd=catalog, check=True)
        for cid, root in (("c-1", "cart"), ("c-2", "mail")):
            (cases / cid / ".ograph").mkdir(parents=True)
            (cases / cid / ".ograph" / "config.toml").write_text(f'[[source]]\nkind = "catalog"\npath = "{catalog}"\n[[source]]\nkind = "incidents"\npath = "incidents.json"\n')
            inc = f"INC-{cid[-1]}"
            (cases / cid / "incidents.json").write_text(json.dumps([{"id": inc, "title": "down", "service": "web", "opened": 1700000000, "severity": "SEV2", "status": "open"}]))
            labels[cid] = {"root": root, "fault": "cpu", "rep": 1, "incident": inc}
        (cases / "labels.json").write_text(json.dumps(labels))
        self.saved = dict(h.ARMS)
        self.calls = []
        def cart(ctx, case, ws):
            self.calls.append(case.id)
            return h.Answer("", "cart", text="A hypothesis.\nROOT: cart", tool_calls=2, events=[bash("ograph chains " + case.incident)])
        h.ARMS["cart"] = h.Arm("cart", "always answers cart", True, cart)
        h.ARMS["absent"] = h.Arm("absent", "cannot run here", True, cart, lambda ctx: "its backend is not installed")

    def tearDown(self):
        h.ARMS.clear(); h.ARMS.update(self.saved); self.tmp.cleanup()

    def harness(self, *more):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            code = h.main(["--bin", str(BIN), "--cases", str(self.root / "cases"), "--sample", "2", "--runs", "2", "--out", str(self.root / "out"),
                           "--work", str(self.root / "work"), "--arms", "cart,chains,absent", *more])
        rows = [json.loads(l) for l in (self.root / "out" / "runs.jsonl").read_text().splitlines()]
        return code, out.getvalue(), rows

    def test_scores_skips_and_resumes(self):
        code, text, rows = self.harness()
        self.assertEqual(code, 0)
        self.assertEqual(sorted((r["case"], r["arm"], r["run"], r["status"], r["root"]) for r in rows), [
            ("rcaeval/c-1/INC-1", "cart", 1, "pass", "cart"), ("rcaeval/c-1/INC-1", "cart", 2, "pass", "cart"),
            ("rcaeval/c-1/INC-1", "chains", 1, "fail", ""), ("rcaeval/c-1/INC-1", "chains", 2, "fail", ""),
            ("rcaeval/c-2/INC-2", "cart", 1, "fail", "cart"), ("rcaeval/c-2/INC-2", "cart", 2, "fail", "cart"),
            ("rcaeval/c-2/INC-2", "chains", 1, "fail", ""), ("rcaeval/c-2/INC-2", "chains", 2, "fail", "")])
        self.assertIn("skipped arm absent: its backend is not installed", text)
        self.assertIn("| cart | 2/4 (0.50) | 1 | 0 | 1 | 0 | 4/4 | 2 |", text)
        self.assertIn("cart against chains: better on 1 cases, worse on 0, equal on 1 (sign test p = 1.000)", text)
        self.assertIn("leak check: 0 of 8 runs voided; 0 errors", text)
        self.assertIn("cart wrong answers: cart x2", text)
        self.assertIn("cart departs from chains rank 1 on 4 of 4 runs: right on 2 of them, and rank 1 was right on 0", text)
        self.assertEqual(len(self.calls), 4)
        self.harness()
        self.assertEqual(len(self.calls), 4, "finished runs are not repeated")
        self.assertFalse(list((self.root / "work" / "runs").iterdir()), "each run's workspace is removed")

    def test_a_usage_limit_stops_the_model_arms_after_one_call(self):
        fake = self.root / "fakebin"; fake.mkdir()
        (fake / "pi").write_text(f"#!/bin/sh\necho call >> {self.root}/pi-calls\necho 'The user has reached their usage limit.'\n")
        (fake / "pi").chmod(0o755)
        path = os.environ["PATH"]
        os.environ["PATH"] = f"{fake}{os.pathsep}{path}"
        try: code, text, rows = self.harness("--arms", "raw,chains", "--parallel", "1")
        finally: os.environ["PATH"] = path
        self.assertEqual((self.root / "pi-calls").read_text(), "call\n")
        self.assertEqual(sorted((r["arm"], r["status"]) for r in rows), [("chains", "fail")] * 4 + [("raw", "error")] * 4)
        self.assertIn("not run: the provider's usage limit is reached", text)
        code, text, rows = self.harness("--arms", "raw,chains", "--report-only")
        self.assertIn("| raw | 0/0 (0.00) | 0 | 0 | 0 | 2 |", text)

    def test_strict_fails_on_a_missing_arm(self):
        with self.assertRaisesRegex(SystemExit, "arm absent cannot run: its backend is not installed"): self.harness("--strict")

    def test_a_planted_label_file_voids_every_run_before_any_arm_sees_it(self):
        (self.root / "cases" / "c-1" / "labels.json").write_text((self.root / "cases" / "labels.json").read_text())
        code, text, rows = self.harness()
        voided = [r for r in rows if r["case"] == "rcaeval/c-1/INC-1"]
        self.assertEqual({r["status"] for r in voided}, {"leak"})
        self.assertTrue(all(r["why"].startswith("workspace: labels.json: file name") for r in voided))
        self.assertEqual(sorted(self.calls), ["c-2", "c-2"])
        self.assertIn("leak check: 4 of 8 runs voided", text)

    def test_a_run_that_reads_the_labels_is_voided(self):
        labels = self.root / "cases" / "labels.json"
        h.ARMS["cart"] = h.Arm("cart", "cheats", True, lambda ctx, case, ws: h.Answer("", case.root, text=f"ROOT: {case.root}", events=[bash(f"cat {labels}")]))
        code, text, rows = self.harness()
        self.assertEqual({r["status"] for r in rows if r["arm"] == "cart"}, {"leak"})
        self.assertIn("| cart | 0/0 (0.00) | 0 | 0 | 0 | 2 |", text)


class Guidance(unittest.TestCase):
    def test_a_guided_arm_reads_the_skill_of_its_revision(self):
        with tempfile.TemporaryDirectory() as d:
            work = pathlib.Path(os.path.realpath(d)) / "work"
            ctx = h.Ctx(types.SimpleNamespace(work=str(work), cases=None, bin=str(BIN), guidance="old=5179213"))
            ctx.setup()
            old = subprocess.run(["git", "-C", str(h.REPO), "show", "5179213:skills/ograph/SKILL.md"], capture_output=True, text=True, check=True).stdout
            self.assertEqual((ctx.skill_of("old") / "SKILL.md").read_text(), old)
            self.assertEqual((ctx.agent_of("old") / "SKILL.md").read_text(), subprocess.run(["git", "-C", str(h.REPO), "show", "5179213:scripts/agent/SKILL.md"], capture_output=True, text=True, check=True).stdout)
            self.assertEqual(ctx.skill_of("old"), work / "guide" / "old" / "skills" / "ograph")
            arm = h.arm_named("skill@old")
            self.assertEqual((arm.name, arm.guide), ("skill@old", "old"))
            self.assertEqual(h.arm_named("skill@new").unavailable(ctx) if h.shutil.which("pi") else "no --guidance new=REV", "no --guidance new=REV")
            self.assertIsNone(h.arm_named("chains@old"))
            ws = work / "runs" / "r" / "ops"; ws.mkdir(parents=True)
            self.assertEqual(h.leaks_in_transcript(ctx, ws, [bash(f"cat {ctx.skill_of('old')}/references/investigation.md")], arm), [])
            self.assertEqual(h.leaks_in_transcript(ctx, ws, [bash(f"cat {work}/guide/old/scripts/agent/SKILL.md")], arm), [f"bash: reaches {work}/guide/old/scripts/agent/SKILL.md"])

    def test_a_work_directory_inside_the_repository_is_refused(self):
        with self.assertRaisesRegex(SystemExit, "is inside"):
            h.Ctx(types.SimpleNamespace(work=str(h.REPO / "target" / "agent-work"), cases=None, bin="unused"))


if __name__ == "__main__":
    unittest.main()
