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
import clean_failed_restore
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


class FailedRestoreCleanupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name).resolve()
        self.target = self.root / "target"
        self.target.mkdir()
        (self.target / "partial-artifact").write_text("incomplete")
        (self.root / "Cargo.toml").write_text("source")
        (self.root / "src").mkdir()
        (self.root / "src/lib.rs").write_text("source code")

    def clean(self, target=None, build=None):
        metadata = {"target_directory": str(target if target is not None else self.target)}
        if build is not None:
            metadata["build_directory"] = str(build)
        with patch.object(clean_failed_restore.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(metadata))):
            clean_failed_restore.clean_failed_restore(self.root)

    def test_discards_partial_outputs_without_touching_source_or_sibling(self):
        sibling = self.root / "unrelated-target"
        sibling.mkdir()
        (sibling / "keep").write_text("other job")
        self.clean(build=self.target / "build")
        self.assertFalse(self.target.exists())
        self.assertEqual((self.root / "Cargo.toml").read_text(), "source")
        self.assertEqual((sibling / "keep").read_text(), "other job")

    def test_all_paths_are_validated_before_any_cleanup(self):
        for unsafe in [self.root, self.root / "src", self.root.parent / "shared-target"]:
            with self.subTest(unsafe=unsafe), self.assertRaises(ValueError):
                self.clean(build=unsafe)
            self.assertEqual((self.target / "partial-artifact").read_text(), "incomplete")
            self.assertEqual((self.root / "src/lib.rs").read_text(), "source code")

    def test_relative_target_is_rejected(self):
        with self.assertRaises(ValueError):
            self.clean(target=Path("target"))
        self.assertTrue((self.target / "partial-artifact").exists())

    @unittest.skipIf(os.name == "nt", "Symlink fixture requires Unix")
    def test_symlink_target_is_rejected_without_following_it(self):
        with tempfile.TemporaryDirectory() as outside:
            other = Path(outside)
            (other / "keep").write_text("shared")
            link = self.target / "linked-build"
            link.symlink_to(other, target_is_directory=True)
            with self.assertRaises(ValueError):
                self.clean(build=link)
            self.assertEqual((other / "keep").read_text(), "shared")
            self.assertTrue((self.target / "partial-artifact").exists())

    def test_restore_failure_recovers_before_unchanged_build_gates(self):
        import yaml
        workflow = yaml.safe_load((Path(check_workflows.ROOT) / ".github/workflows/ci.yml").read_text())
        for name in ["check", "browser-auth", "windows-cross-check"]:
            steps = workflow["jobs"][name]["steps"]
            restore = next(i for i, step in enumerate(steps) if step.get("id") == "compiler-snapshot")
            self.assertIs(steps[restore]["continue-on-error"], True)
            recovery = steps[restore + 1]
            self.assertEqual(recovery["if"], "steps.compiler-snapshot.outcome == 'failure'")
            self.assertEqual(recovery["run"], "python3 scripts/ci/clean_failed_restore.py")
            self.assertNotIn("continue-on-error", recovery)
        check_runs = [step.get("run") for step in workflow["jobs"]["check"]["steps"]]
        for gate in ["cargo flux run check", "cargo flux run lint", "cargo flux run test"]:
            self.assertIn(gate, check_runs)
            step = next(step for step in workflow["jobs"]["check"]["steps"] if step.get("run") == gate)
            self.assertNotIn("continue-on-error", step)


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


@unittest.skipUnless(os.name == "posix", "requires the native Linux runner")
class NativeTestRunnerTests(unittest.TestCase):
    def test_direct_status_is_authoritative_and_diagnostics_are_bounded(self):
        runner = Path(__file__).with_name("native-test-runner.sh").resolve()
        for direct, diagnostic, tools in [
            (0, 0, "all"), (23, 0, "all"), (23, 31, "all"),
            (23, 0, "missing-strace"), (23, 0, "missing-timeout"),
            (23, 0, "timeout"),
        ]:
            with self.subTest(direct=direct, diagnostic=diagnostic, tools=tools):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    binary = root / "tmux_adapter-fixture"
                    bindir = root / "bin"
                    bindir.mkdir()
                    def executable(path, body):
                        path.write_text("#!/bin/sh\n" + body)
                        path.chmod(0o700)
                    executable(binary, 'printf "%s\\n" "$@" >> "$CALLS"\n'
                               'if [ "${DIAGNOSTIC:-}" = 1 ]; then exit "$DIAG_STATUS"; fi\n'
                               'exit "$DIRECT_STATUS"\n')
                    for tool in ("cat", "mkdir"):
                        (bindir / tool).symlink_to(shutil.which(tool))
                    if tools != "missing-strace":
                        executable(bindir / "strace", 'printf "trace\\n" >> "$EVENTS"\n'
                                   'while [ "$1" != -o ]; do shift; done\nshift 2\n'
                                   'DIAGNOSTIC=1; export DIAGNOSTIC\nexec "$@"\n')
                    if tools != "missing-timeout":
                        executable(bindir / "timeout", 'printf "%s\\n" "$*" >> "$EVENTS"\n'
                                   'if [ "$TIMEOUT_MODE" = timeout ]; then exit 124; fi\n'
                                   'shift 2\nexec "$@"\n')
                    env = dict(os.environ, PATH=str(bindir), RUNNER_TEMP=str(root),
                               CALLS=str(root / "calls"), EVENTS=str(root / "events"),
                               DIRECT_STATUS=str(direct), DIAG_STATUS=str(diagnostic),
                               TIMEOUT_MODE=tools)
                    result = subprocess.run(["/bin/bash", str(runner), str(binary), "literal argument", "--exact"],
                                            env=env, capture_output=True, text=True, timeout=5)
                    self.assertEqual(result.returncode, direct, result.stderr)
                    calls = (root / "calls").read_text().splitlines()
                    expected = 2 if direct and tools == "all" else 1
                    self.assertEqual(calls, ["literal argument", "--exact"] * expected)
                    events = (root / "events").read_text() if (root / "events").exists() else ""
                    if direct == 0:
                        self.assertEqual(events, "")
                        self.assertFalse((root / "native-tmux-processes").exists())
                    elif tools in ("all", "timeout"):
                        self.assertIn("--kill-after=5s 60s strace", events)


if __name__ == "__main__":
    unittest.main()
