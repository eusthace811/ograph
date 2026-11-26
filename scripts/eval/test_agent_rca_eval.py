#!/usr/bin/env python3
"""Tests for agent_rca_eval.py that need no model: scoring, sampling, the leak detector on
planted leaks, and the harness end to end with stand-in arms.

Usage: python3 scripts/eval/test_agent_rca_eval.py [--bin target/release/ograph]"""
import contextlib, io, json, os, pathlib, shutil, subprocess, sys, tempfile, types, unittest

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
            first = h.holdout_cases(d, 3, 13, {c.id for c in four})
            rest = h.holdout_cases(d, 3, 13, {c.id for c in four}, rest=True)
            self.assertEqual((len(first), len(rest)), (3, 5))
            self.assertEqual(sorted(c.id for c in first + rest + four), sorted(labels))
            self.assertEqual({c.suite for c in rest}, {"rcaeval-heldout-2"})


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

    def test_an_out_directory_in_use_by_another_run_is_refused(self):
        import fcntl
        (self.root / "out").mkdir()
        with open(self.root / "out" / ".lock", "w") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(SystemExit) as e: self.harness()
        self.assertIn("another agent_rca_eval run is writing", str(e.exception))
        self.assertEqual(self.calls, [], "nothing runs")

    def test_rows_of_another_binary_are_not_resumed(self):
        self.harness()
        other = self.root / "other-ograph"; shutil.copy(BIN, other); other.write_bytes(other.read_bytes() + b"\0")
        out = io.StringIO()
        with self.assertRaises(SystemExit) as e, contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            h.main(["--bin", str(other), "--cases", str(self.root / "cases"), "--sample", "2", "--runs", "2", "--out", str(self.root / "out"),
                    "--work", str(self.root / "work"), "--arms", "cart,chains"])
        self.assertIn("the rows of arm cart, chains in", str(e.exception))
        self.assertEqual(len(self.calls), 4, "no run of the other binary")
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            h.main(["--bin", str(other), "--cases", str(self.root / "cases"), "--sample", "2", "--runs", "1", "--out", str(self.root / "out"),
                    "--work", str(self.root / "work"), "--arms", "absent"])
        self.assertEqual(len(self.calls), 4, "an arm no other binary wrote may share the directory")

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

    def test_the_claude_runner_maps_each_arm_and_reads_its_stream(self):
        fake = self.root / "fakebin"; fake.mkdir()
        init = {"type": "system", "subtype": "init", "tools": ["Bash", "Read"], "mcp_servers": [], "skills": [], "slash_commands": [],
                "plugins": [{"name": "cc-plugin-privacy", "source": "cc-plugin-privacy@builtin"}], "model": "claude-haiku-5-5"}
        stream = [init,
                  {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ograph chains INC-1"}}]}},
                  {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "rank 1 cart"}]}]}},
                  {"type": "assistant", "message": {"content": [{"type": "text", "text": "A hypothesis.\nROOT: cart"}]}},
                  {"type": "result", "subtype": "success", "is_error": False, "result": "A hypothesis.\nROOT: cart", "total_cost_usd": 0.0125,
                   "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 0}}]
        (fake / "stream.jsonl").write_text("".join(json.dumps(e) + "\n" for e in stream))
        (fake / "claude").write_text(f"#!/bin/sh\nprintf '%s\\0' \"$@\" > {self.root}/argv\ncat > {self.root}/stdin\nenv > {self.root}/env\ncat {fake}/stream.jsonl\n")
        (fake / "claude").chmod(0o755)
        path = os.environ["PATH"]
        os.environ["PATH"] = f"{fake}{os.pathsep}{path}"
        os.environ["CLAUDECODE"] = "1"
        try: code, text, rows = self.harness("--arms", "skill,chains", "--runner", "claude", "--runs", "1", "--parallel", "1")
        finally: os.environ["PATH"] = path; os.environ.pop("CLAUDECODE")
        skill = [r for r in rows if r["arm"] == "skill"]
        self.assertEqual([(r["status"], r["root"], r["cost"], r["tokens"], r["tools"], r["model"], r["runner"]) for r in skill],
                         [("pass", "cart", 0.0125, 115, {"bash": 1}, "claude-haiku-5-5", "claude"), ("fail", "cart", 0.0125, 115, {"bash": 1}, "claude-haiku-5-5", "claude")])
        argv = (self.root / "argv").read_text().split("\0")
        for flag in ("--setting-sources", "--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence", "--max-budget-usd"):
            self.assertIn(flag, argv)
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "")
        self.assertEqual(argv[argv.index("--tools") + 1], "Bash,Read")
        skill_text = argv[argv.index("--append-system-prompt") + 1]
        self.assertIn("<available_skills>", skill_text)
        self.assertIn(f"<location>{self.root / 'work' / 'skills' / 'ograph' / 'SKILL.md'}</location>", skill_text)
        self.assertIn("Incident INC-2 is open.", (self.root / "stdin").read_text(), "the prompt goes on stdin")
        self.assertNotIn("CLAUDECODE=", (self.root / "env").read_text(), "the eval agent does not inherit this session's Claude Code variables")
        self.assertIn("model claude-haiku-5-5 on claude", text)
        # A run whose init shows anything beyond its arm is voided.
        init["skills"] = ["ograph"]; init["plugins"].append({"name": "x", "source": "x@user-marketplace"})
        (fake / "stream.jsonl").write_text("".join(json.dumps(e) + "\n" for e in stream))
        os.environ["PATH"] = f"{fake}{os.pathsep}{path}"
        try: code, text, rows = self.harness("--arms", "skill,chains", "--runner", "claude", "--runs", "1", "--fresh")
        finally: os.environ["PATH"] = path
        self.assertEqual({(r["status"], r["why"]) for r in rows if r["arm"] == "skill"}, {("leak", "isolation: skills ['ograph']; plugins ['x@user-marketplace']")})

    def test_claude_stream_names_mcp_tools_as_pi_does(self):
        out = "\n".join(json.dumps(e) for e in [
            {"type": "system", "subtype": "init", "tools": ["mcp__ograph__chains"], "mcp_servers": [{"name": "ograph", "status": "connected"}]},
            {"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "a", "name": "mcp__ograph__chains", "input": {"incident": "INC-1"}}]}},
            {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "a", "content": "RCAEval cart", "is_error": True}]}},
            {"type": "result", "result": "ROOT: cart", "total_cost_usd": 0.5, "usage": {}}])
        events, text, tokens, cost, tools, init, result = h.claude_digest(out)
        self.assertEqual(tools, {"ograph_chains": 1})
        self.assertEqual(events[1], {"type": "tool_execution_end", "toolName": "ograph_chains", "result": {"content": [{"type": "text", "text": "RCAEval cart"}]}, "isError": True})
        self.assertEqual((text, cost), ("ROOT: cart", 0.5))
        self.assertEqual(h.isolation_breach(init, ["mcp__ograph__chains"], True), [])
        self.assertEqual(h.isolation_breach(init, ["mcp__ograph__chains", "Bash"], True), ["tools ['mcp__ograph__chains']"])
        self.assertEqual(h.isolation_breach({**init, "memory_paths": {"auto": "/m"}}, ["mcp__ograph__chains"], True), ["memory {'auto': '/m'}"])
        self.assertEqual(h.isolation_breach({**init, "slash_commands": ["mcp__ograph__investigate"]}, ["mcp__ograph__chains"], True), [])
        self.assertEqual(h.isolation_breach({**init, "slash_commands": ["mcp__ograph__investigate", "review"]}, ["mcp__ograph__chains"], True), ["slash_commands ['review']"])
        ctx = types.SimpleNamespace(secret=[pathlib.Path("/repo")])
        self.assertEqual(h.leaks_in_transcript(types.SimpleNamespace(**vars(ctx), bin=pathlib.Path("/w/bin/ograph"), nobin=pathlib.Path("/w/nobin"), work=pathlib.Path("/w"), guides={"": None},
                                                                       skill_of=lambda g: pathlib.Path("/w/skills/ograph")), pathlib.Path("/w/runs/r/ops"), events, h.ARMS["mcp"]),
                         ["ograph_chains: output contains 'rcaeval'"])

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
