from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import check_workflows
import qualify_compiler_cache
import report_resources


class WorkflowValidationTests(unittest.TestCase):
    def test_moving_refs_are_rejected_and_immutable_refs_are_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "test.yml"
            workflow.write_text("jobs: {check: {steps: [{uses: actions/checkout@v4}]}}\n")
            self.assertEqual(len(check_workflows.unpinned_actions(workflow)), 1)
            workflow.write_text("jobs: {check: {steps: [{uses: actions/checkout@" + "a" * 40 + "}, {uses: ./local-action}]}}\n")
            self.assertEqual(check_workflows.unpinned_actions(workflow), [])
            workflow.write_text("jobs: {check: {steps: [{uses: docker://example/image@sha256:" + "b" * 64 + "}]}}\n")
            self.assertEqual(check_workflows.unpinned_actions(workflow), [])
            workflow.write_text("jobs: {check: {uses: owner/repo/.github/workflows/check.yml@" + "c" * 40 + ", with: {uses: input-data}}}\n")
            self.assertEqual(check_workflows.unpinned_actions(workflow), [])

    def test_quoted_flow_and_aliased_uses_keys_cannot_bypass_pinning(self):
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "test.yml"
            for content in [
                'jobs:\n  check:\n    steps:\n      - "uses": actions/checkout@v4\n',
                'jobs: {check: {steps: [{uses: actions/checkout@v4}]}}\n',
                'jobs: {check: {steps: [&action {uses: actions/checkout@v4}, *action]}}\n',
                'jobs: {check: {uses: owner/repo/.github/workflows/check.yml@main}}\n',
            ]:
                workflow.write_text(content)
                self.assertTrue(check_workflows.unpinned_actions(workflow), content)

    def test_missing_actionlint_fails_explicitly(self):
        result = subprocess.run(
            [sys.executable, str(Path(check_workflows.__file__)), "--actionlint", "/missing/actionlint"],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("actionlint is required", result.stderr)

    def test_actionlint_rejects_invalid_workflow_and_accepts_valid_workflow(self):
        actionlint = os.environ.get("ACTIONLINT") or shutil.which("actionlint")
        if not actionlint:
            self.skipTest("Install actionlint or set ACTIONLINT for the real validator test")
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "test.yml"
            prefix = "name: Fixture\non: push\njobs:\n  check:\n    runs-on: ubuntu-latest\n    steps:\n"
            for body, expected in [("      - run: echo ok\n", 0), ("      - uses: actions/checkout@v4\n", 1), ("      - run: ${{ unknown.property }}\n", 1)]:
                workflow.write_text(prefix + body)
                result = subprocess.run(
                    [sys.executable, str(Path(check_workflows.__file__)), "--actionlint", actionlint, str(workflow)],
                    capture_output=True, text=True, check=False,
                )
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)

    def test_environment_uses_is_data_not_an_action(self):
        actionlint = os.environ.get("ACTIONLINT") or shutil.which("actionlint")
        if not actionlint:
            self.skipTest("Install actionlint or set ACTIONLINT for the real validator test")
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "test.yml"
            workflow.write_text("name: Fixture\non: push\njobs:\n  check:\n    runs-on: ubuntu-latest\n    env: {uses: job-data}\n    steps:\n      - run: echo ok\n        env: {uses: step-data}\n")
            self.assertEqual(check_workflows.unpinned_actions(workflow), [])
            result = subprocess.run(
                [sys.executable, str(Path(check_workflows.__file__)), "--actionlint", actionlint, str(workflow)],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


class ResourceReportingTests(unittest.TestCase):
    def test_absent_resource_probes_are_reported_without_failure(self):
        with patch.object(report_resources, "memory_bytes", side_effect=OSError), patch.object(report_resources.shutil, "disk_usage", side_effect=OSError):
            result = report_resources.collect()
        self.assertIsNone(result["memory_bytes"])
        self.assertIsNone(result["disk_free_bytes"])
        self.assertEqual(set(result), {"logical_cpus", "memory_bytes", "disk_free_bytes"})

    def test_unwritable_summary_does_not_fail_or_dump_environment(self):
        output = io.StringIO()
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": directory, "SECRET_FIXTURE": "never-print-this"}), redirect_stdout(output):
            report_resources.main()
        self.assertNotIn("never-print-this", output.getvalue())
        self.assertIn("logical_cpus", json.loads(output.getvalue()))


@unittest.skipIf(os.name == "nt", "Executable subprocess fixtures run in the Linux CI job")
class CompilerCacheProbeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.rustc = self.executable("rustc", "from pathlib import Path\nimport sys\np = Path(sys.argv[-1])\np.write_text('#!/bin/sh\\nprintf marshal-cache-probe')\np.chmod(0o755)\n")

    def executable(self, name, source):
        path = self.root / name
        path.write_text(f"#!{sys.executable}\n" + source)
        path.chmod(0o755)
        return str(path)

    def probe(self, cache, timeout=2):
        return qualify_compiler_cache.qualify(cache, self.rustc, self.root / "outputs", timeout)

    def test_success_uses_separate_compiler_outputs_and_does_not_claim_production_qualification(self):
        cache = self.executable("cache", "import subprocess,sys\nif sys.argv[1:] == ['--start-server']: sys.exit(0)\nsys.exit(subprocess.run(sys.argv[1:]).returncode)\n")
        report, status = self.probe(cache)
        self.assertEqual(status, 0)
        self.assertTrue(report["output_matches"])
        self.assertFalse(report["production_qualified"])
        self.assertTrue((self.root / "outputs/direct/probe").exists())
        self.assertTrue((self.root / "outputs/cached/probe").exists())

    def test_missing_cache_still_records_direct_compilation(self):
        report, status = self.probe(str(self.root / "absent"))
        self.assertEqual(status, 1)
        self.assertEqual(report["direct"]["outcome"], "ok")
        self.assertEqual(report["startup"]["outcome"], "unavailable")

    def test_startup_failure_is_not_reported_as_a_cache_compile(self):
        cache = self.executable("cache", "import sys\nsys.exit(7)\n")
        report, status = self.probe(cache)
        self.assertEqual(status, 1)
        self.assertEqual(report["startup"]["exit_code"], 7)
        self.assertNotIn("cached", report)

    def test_runtime_failure_does_not_fallback_into_cached_output(self):
        cache = self.executable("cache", "import sys\nsys.exit(0 if sys.argv[1:] == ['--start-server'] else 8)\n")
        report, status = self.probe(cache)
        self.assertEqual(status, 1)
        self.assertEqual(report["cached"]["exit_code"], 8)
        self.assertFalse((self.root / "outputs/cached/probe").exists())
        self.assertTrue((self.root / "outputs/direct/probe").exists())

    def test_runtime_timeout_retains_outputs_without_retrying(self):
        cache = self.executable("cache", "import sys,time\nif sys.argv[1:] != ['--start-server']: time.sleep(10)\n")
        report, status = self.probe(cache, timeout=0.5)
        self.assertEqual(status, 1)
        self.assertEqual(report["cached"]["outcome"], "timeout")
        self.assertTrue((self.root / "outputs/cached").exists())
        self.assertFalse((self.root / "outputs/cached/probe").exists())

    def test_startup_timeout_is_bounded(self):
        cache = self.executable("cache", "import time\ntime.sleep(10)\n")
        report, status = self.probe(cache, timeout=0.5)
        self.assertEqual(status, 1)
        self.assertEqual(report["startup"]["outcome"], "timeout")

    def test_detached_writer_survives_client_timeout_without_colliding_with_direct_output(self):
        child = "import pathlib,sys,time;time.sleep(1);pathlib.Path(sys.argv[1]).write_text('late-cache-output')"
        cache = self.executable("cache", f"import subprocess,sys,time\nif sys.argv[1:] == ['--start-server']: sys.exit(0)\nsubprocess.Popen([sys.executable, '-c', {child!r}, sys.argv[-1]], start_new_session=True)\ntime.sleep(10)\n")
        report, status = self.probe(cache, timeout=0.5)
        self.assertEqual(status, 1)
        self.assertEqual(report["cached"]["outcome"], "timeout")
        time.sleep(1)
        self.assertEqual((self.root / "outputs/cached/probe").read_text(), "late-cache-output")
        self.assertIn("marshal-cache-probe", (self.root / "outputs/direct/probe").read_text())

    def test_real_compiler_failure_status_is_preserved(self):
        self.rustc = self.executable("broken-rustc", "import sys\nsys.exit(9)\n")
        report, status = self.probe(str(self.root / "absent"))
        self.assertEqual(status, 9)
        self.assertEqual(report["direct"]["exit_code"], 9)
        self.assertNotIn("startup", report)


if __name__ == "__main__":
    unittest.main()
