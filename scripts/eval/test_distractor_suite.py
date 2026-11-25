#!/usr/bin/env python3
"""Tests for examples/distractors/generate.py: byte-identical output for a seed, labels by
construction, nothing in a case that gives its label away, and the committed SHA256SUMS.

Usage: python3 scripts/eval/test_distractor_suite.py"""
import collections, hashlib, json, pathlib, re, subprocess, sys, tempfile, unittest

REPO = pathlib.Path(__file__).resolve().parents[2]
GEN = REPO / "examples" / "distractors" / "generate.py"


def generate(out, seed="2026"):
    subprocess.run([sys.executable, str(GEN), "--seed", seed, "--out", str(out)], check=True, capture_output=True)


def digests(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(root.rglob("*")) if p.is_file() and ".git" not in p.relative_to(root).parts}


class Suite(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.a, cls.b = pathlib.Path(cls.tmp.name) / "a", pathlib.Path(cls.tmp.name) / "b"
        generate(cls.a); generate(cls.b)
        cls.labels = json.loads((cls.a / "labels.json").read_text())

    @classmethod
    def tearDownClass(cls): cls.tmp.cleanup()

    def test_same_seed_same_bytes(self):
        self.assertEqual(digests(self.a), digests(self.b))

    def test_other_seed_other_cases(self):
        c = pathlib.Path(self.tmp.name) / "c"
        generate(c, seed="7")
        self.assertNotEqual(digests(self.a), digests(c))

    def test_committed_manifest(self):
        want = {}
        for line in (GEN.parent / "SHA256SUMS").read_text().splitlines():
            h, name = line.split(None, 1)
            want[name.removeprefix("./")] = h
        self.assertEqual(digests(self.a), want)

    def test_types_topologies_and_cells(self):
        by = collections.defaultdict(list)
        for lab in self.labels.values(): by[lab["distractor"]].append(lab)
        self.assertEqual(sorted(by), list("abcde"))
        for k, labs in by.items():
            self.assertGreaterEqual(len(labs), 6, k)
            self.assertGreaterEqual(len({l["topology"] for l in labs}), 3, k)
        cells = [(l["root"], l["fault"]) for l in self.labels.values()]
        self.assertEqual(len(cells), len(set(cells)))

    def test_labels_by_construction(self):
        for cid, lab in self.labels.items():
            d = self.a / cid
            catalog = d.joinpath("catalog", "catalog-info.yaml").read_text()
            apps = re.findall(r"^  name: (\S+)$", catalog, re.M)
            self.assertIn(lab["root"], apps); self.assertIn(lab["distractor_app"], apps)
            self.assertNotEqual(lab["root"], lab["distractor_app"])
            self.assertNotIn("service", lab["root"])
            incidents = json.loads(d.joinpath("incidents.json").read_text())
            self.assertEqual(incidents[-1]["id"], lab["incident"]); self.assertEqual(incidents[-1]["status"], "open")
            logs = [json.loads(l) for l in d.joinpath("logs.jsonl").read_text().splitlines()]
            first_error = min(l["ts"] for l in logs if "failed" in l["message"] or l["service"] == lab["root"] and l["ts"] >= lab["inject"] and "handled" not in l["message"])
            self.assertGreaterEqual(first_error, lab["inject"] - 1, cid)
            if lab["distractor"] == "b":
                deploys = [json.loads(l) for l in d.joinpath("deployments.jsonl").read_text().splitlines()]
                self.assertTrue(any(x["service"] == lab["distractor_app"] and 0 < lab["inject"] - x["at"] <= 240 for x in deploys))
                self.assertNotIn(lab["distractor_app"], lab["callers"] + [lab["root"]])
            if lab["distractor"] == "c":
                alerts = sorted(json.loads(d.joinpath("alerts.json").read_text())["alerts"], key=lambda a: a["startsAt"])
                recent = [a for a in alerts if a["startsAt"] >= lab["inject"] - 3600]
                self.assertEqual(recent[0]["labels"]["service"], lab["distractor_app"])
                self.assertIn(lab["distractor_app"], lab["callers"])
            if lab["distractor"] == "e":
                past = [i for i in incidents[:-1] if i["service"] == incidents[-1]["service"]]
                self.assertTrue(past and all(lab["distractor_app"] in i["mitigation"] for i in past))

    def test_nothing_gives_the_label_away(self):
        for cid in self.labels:
            for p in (self.a / cid).rglob("*"):
                if not p.is_file() or ".git" in p.parts: continue
                data = p.read_bytes()
                self.assertIsNone(re.search(rb'"(?:root|inject|fault|distractor\w*)"\s*:', data), p)
                self.assertNotIn(b"rcaeval", data.lower())
                self.assertIsNone(re.search(r"label|question|generate\.py", p.name, re.I), p)


if __name__ == "__main__":
    unittest.main()
