#!/usr/bin/env python3
"""Tests for misled_by.py and replay_calls.py that need no binary and no model.

Usage: python3 scripts/eval/test_misled_by.py"""
import json, pathlib, sys, unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import misled_by as m
import replay_calls as rc


def run(root, *calls):
    return {"key": "k", "arm": "mcp", "case": "rcaeval/c/INC-1", "run": 1, "root": root, "label": "right", "status": "fail",
            "calls": [{"tool": t, "args": a, "output": o} for t, a, o in calls]}


BEFORE = json.dumps({"app": "web", "events": [{"app": "cache", "signal": "METRIC_ANOMALY", "severity": 0.05},
                                               {"app": "cache", "signal": "METRIC_ANOMALY", "severity": 1.0}, {"app": "right", "signal": "LOG_ERROR", "severity": 1.0}]})
CHAINS = json.dumps({"app": "web", "apps": [{"app": "right", "score": 1.0}, {"app": "cache", "score": 0.7, "chain": {"root": {"kind": "METRIC_ANOMALY", "severity": 1.0}}}]})


class Attribute(unittest.TestCase):
    def test_first_mcp_output_with_an_event_on_the_app(self):
        r = run("cache", ("ograph_blast", {"app": "cache"}, '{"app":"cache","dependents":[["web",1]]}'),
                ("ograph_chains", {"incident": "INC-1"}, CHAINS), ("ograph_before", {"incident": "INC-1"}, BEFORE))
        self.assertEqual(m.attribute(r), (2, "chains", "chain rank 2 score 0.70, root METRIC_ANOMALY severity 1.0"))

    def test_strongest_signal_of_the_first_output(self):
        self.assertEqual(m.attribute(run("cache", ("ograph_before", {}, BEFORE)))[2], "METRIC_ANOMALY severity 1.0")

    def test_scope_only_is_not_a_candidate(self):
        scope = json.dumps({"app": "web", "scope": [["web", 0], ["cache", 2]], "events": [{"app": "right", "signal": "LOG_ERROR", "severity": 1.0}]})
        self.assertIsNone(m.attribute(run("cache", ("ograph_before", {}, scope))))

    def test_cite_check_echo_is_not_a_candidate(self):
        self.assertIsNone(m.attribute(run("cache", ("ograph_cite_check", {"text": "cache"}, '[{"cited":"cache","status":"present"}]'))))

    def test_cli_text_in_one_bash_call(self):
        out = "id:inc/inc-1 on web at 2024; scope: web(0) cache(2)\n  -  3.3 min  cache  METRIC_ANOMALY  sev 0.12  id:ev/x\n  2. cache   score 0.85\n"
        self.assertEqual(m.attribute(run("cache", ("bash", {"command": "ograph before INC-1; ograph chains INC-1"}, out))), (1, "before", "METRIC_ANOMALY severity 0.12"))

    def test_suspects_rank_is_not_chains(self):
        out = "  1. web   score 1.00  distance 0  explains web\n  2. cache  score 0.91  distance 1  explains web\n"
        self.assertEqual(m.attribute(run("cache", ("bash", {"command": "ograph suspects INC-1"}, out)))[1], "suspects")

    def test_json_cut_short(self):
        out = '{"incident":"id:inc/inc-1","app":"web","events":[{"at":"t","app":"cache","signal":"METRIC_ANOMALY","severity":1.0},{"at":"t2","ap'
        self.assertEqual(m.attribute(run("cache", ("bash", {"command": "ograph --json before INC-1 | head -c 300"}, out)))[1], "before")

    def test_own_analysis_needs_a_fault(self):
        r = run("cache", ("bash", {"command": "cat catalog/catalog-info.yaml; head -1 metrics.csv"}, "  name: cache\ntime,web_cpu,cache_cpu,cache_mem,web_mem,a_x,b_x,c_x,d_x,e_x\n"),
                ("bash", {"command": "grep cache logs.jsonl | head -2"}, '{"ts": 1, "service": "cache", "message": "GET ok"}\n{"ts": 2, "service": "cache", "message": "connection refused by peer"}'))
        self.assertEqual(m.attribute(r)[:2], (2, "own"))

    def test_skill_text_is_not_a_candidate(self):
        r = run("cache", ("read", {"file_path": "/w/skills/ograph/SKILL.md"}, "a cache anomaly may mislead"))
        self.assertIsNone(m.attribute(r))


class Transcript(unittest.TestCase):
    TEXT = "\n".join(["# rcaeval/c/INC-1 arm skill run 1: fail, answered cache, labeled right", "",
                      '$ bash {"command": "a"}', '$ bash {"command": "b"}', "out a", "", "out b line 1", "out b line 2", "",
                      "Now I check the cache.", "", '$ ograph_chains {"incident": "INC-1"}', "x" * 3000, "... [512 more characters]", "", "ROOT: cache"])

    def test_parse_batches_and_cut(self):
        calls = rc.parse(self.TEXT)
        self.assertEqual([c["tool"] for c in calls], ["bash", "bash", "ograph_chains"])
        self.assertEqual(rc.kept_output(calls[0]["region"], calls[0]["nth"]), ("out a", False))
        self.assertEqual(rc.kept_output(calls[1]["region"], calls[1]["nth"]), ("out b line 1\nout b line 2", False))
        kept, cut = rc.kept_output(calls[2]["region"], calls[2]["nth"])
        self.assertTrue(cut and kept.endswith("[512 more characters]"))

    def test_agreement_masks_harvest_times(self):
        full = '{"harvested_at":"2026-11-25T20:11:08.049Z","x":1}' + "y" * 4000
        kept = '{"harvested_at":"2026-11-25T18:11:07.525Z","x":1}' + "y" * (3000 - 49) + "\n... [9 more characters]"
        self.assertEqual(rc.agreement(kept, kept, full), "exact")
        self.assertEqual(rc.agreement("something else", "something else", full), "mismatch")


if __name__ == "__main__":
    unittest.main()
