"""Tests for scripts/toolkit-capabilities.py.

    python3 -m unittest scripts/test_toolkit_capabilities.py

The fixture is a real `poundlock-toolkit recommend --format json` summary made by
Toolkit 0.5.0 from its fake-backend runs: one coding qualification, an
investigation pair and an OpenAI-boundary tool-schema qualification.
"""

import copy
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "docs/reference/fixtures/toolkit-recommendation-0.5.0.json"
spec = importlib.util.spec_from_file_location(
    "toolkit_capabilities", ROOT / "scripts/toolkit-capabilities.py"
)
converter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(converter)


class ConverterTests(unittest.TestCase):
    def setUp(self):
        self.summary = json.loads(FIXTURE.read_text())

    def by_name(self, summary=None):
        block = converter.capability_block(summary or self.summary)
        return {c["name"]: c for c in block}

    def test_each_toolkit_verdict_becomes_one_capability(self):
        found = self.by_name()
        self.assertEqual(
            set(found), {"code_repair_verified", "sealed_investigation", "plan_tool_schema"}
        )
        for capability in found.values():
            self.assertEqual(capability["status"], "qualified")
            self.assertEqual(capability["source"], "poundlock_toolkit")
            self.assertEqual(capability["toolkit_version"], "0.5.0")

    def test_provenance_is_carried_not_invented(self):
        rows = {r["run_id"]: r for r in self.summary["results"]}
        coding = self.by_name()["code_repair_verified"]
        [run] = coding["run_ids"]
        self.assertEqual(coding["result_sha256"], rows[run]["result_sha256"])
        self.assertEqual(coding["identity_sha256"], rows[run]["identity_sha256"])
        pair = self.by_name()["sealed_investigation"]
        self.assertEqual(len(pair["run_ids"]), 2)
        self.assertEqual(
            pair["result_sha256"],
            converter.paired_digest(*(rows[r]["result_sha256"] for r in pair["run_ids"])),
        )

    def test_nothing_sent_about_thinking_is_the_model_default(self):
        for capability in self.by_name().values():
            self.assertEqual(capability["measured_settings"]["reasoning"], "model_default")

    def test_an_explicit_off_is_off(self):
        summary = copy.deepcopy(self.summary)
        for row in summary["results"]:
            if row["settings"]["reasoning_sent"] == "omitted":
                row["settings"]["reasoning_sent"] = "disabled"
        coding = self.by_name(summary)["code_repair_verified"]
        self.assertEqual(coding["measured_settings"]["reasoning"], "off")

    def test_an_explicit_effort_is_refused(self):
        summary = copy.deepcopy(self.summary)
        summary["results"][0]["settings"]["reasoning_sent"] = "high"
        with self.assertRaisesRegex(ValueError, "reasoning effort 'high'"):
            converter.capability_block(summary)

    def test_a_tool_schema_run_with_two_thinking_behaviours_is_refused(self):
        summary = copy.deepcopy(self.summary)
        row = next(r for r in summary["results"] if r["tool_schema"])
        row["tool_schema"]["boundaries"].append(
            {**row["tool_schema"]["boundaries"][0], "boundary": "ollama", "reasoning_sent": "disabled"}
        )
        with self.assertRaisesRegex(ValueError, "one boundary per run"):
            converter.capability_block(summary)

    def test_an_incomplete_tool_schema_run_is_insufficient(self):
        summary = copy.deepcopy(self.summary)
        row = next(r for r in summary["results"] if r["tool_schema"])
        row["tool_schema"]["boundaries"][0]["attempted"] -= 1
        row["tool_schema"]["boundaries"][0]["passed"] -= 1
        self.assertEqual(self.by_name(summary)["plan_tool_schema"]["status"], "insufficient")

    def test_a_smoke_tool_schema_run_is_smoke(self):
        summary = copy.deepcopy(self.summary)
        next(r for r in summary["results"] if r["tool_schema"])["tool_schema"]["mode"] = "smoke"
        self.assertEqual(self.by_name(summary)["plan_tool_schema"]["status"], "smoke")

    def test_the_weaker_investigation_variant_decides(self):
        summary = copy.deepcopy(self.summary)
        pair = summary["investigation_pairs"][0]
        pair.update(qualified=False)
        pair["findable"].update(level="insufficient evidence", passed=0)
        capability = self.by_name(summary)["sealed_investigation"]
        self.assertEqual(capability["status"], "insufficient")
        self.assertEqual(capability["passed"], 0)
        self.assertEqual(capability["attempted"], 3)

    def test_a_smoke_level_maps_to_smoke(self):
        summary = copy.deepcopy(self.summary)
        row = summary["results"][0]
        coding = next(w for w in row["workloads"] if w["workload"] == "coding")
        coding.update(level="observed smoke pass", passed=1, attempted=1)
        self.assertEqual(self.by_name(summary)["code_repair_verified"]["status"], "smoke")

    def test_anything_but_the_versioned_summary_is_refused(self):
        for value in ({}, {"schema": "urn:poundlock-toolkit:qualification:1"}, []):
            with self.assertRaisesRegex(ValueError, "recommend --format json"):
                converter.capability_block(value)


if __name__ == "__main__":
    unittest.main()
