#!/usr/bin/env python3
from pathlib import Path
import unittest
import tomllib


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"


class ReleaseDispatchContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text()

    def test_registry_packages_do_not_depend_on_private_git_packages(self) -> None:
        root = WORKFLOW.parents[2]
        workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["dependencies"]
        for manifest in (root / "crates").glob("*/Cargo.toml"):
            data = tomllib.loads(manifest.read_text())
            package = data["package"]
            if "publish" not in package.get("metadata", {}).get("flux", {}).get("tasks", []):
                continue
            self.assertIsNot(package.get("publish"), False, str(manifest))
            for name, spec in data.get("dependencies", {}).items():
                if isinstance(spec, dict) and spec.get("workspace"):
                    spec = workspace[name]
                if isinstance(spec, dict) and "git" in spec:
                    self.fail(f"{package['name']} cannot publish Git dependency {name} to crates.io")

    def test_dispatch_waits_for_every_release_producer(self) -> None:
        self.assertIn("needs: [release, publish-packages, build-macos]", self.workflow)
        for job in ("release", "publish-packages", "build-macos"):
            self.assertIn(f"needs.{job}.result == 'success'", self.workflow)

    def test_dispatch_token_is_scoped_to_pulse_deploy(self) -> None:
        self.assertIn("app-id: ${{ vars.RELEASE_APP_ID }}", self.workflow)
        self.assertIn("private-key: ${{ secrets.RELEASE_APP_PRIVATE_KEY }}", self.workflow)
        self.assertNotIn("RELEASE_DISPATCH_APP", self.workflow)
        self.assertIn("repositories: pulse-deploy", self.workflow)
        self.assertIn("permission-contents: write", self.workflow)
        self.assertNotIn("permission-actions: write", self.workflow)

    def test_payload_carries_the_versioned_immutable_contract(self) -> None:
        for field in (
            "schema", "delivery_id", "repository", "tag", "release_sha",
            "source_run_id", "source_run_attempt", "published_at",
        ):
            self.assertIn(f"{field}:", self.workflow)
        self.assertIn('event_type: "marshal-release.v1"', self.workflow)

    def test_notification_job_installs_its_own_api_dependencies(self) -> None:
        notify = self.workflow.split("  notify-pulse-deploy:", 1)[1]
        self.assertIn("apt-get install", notify)
        self.assertIn("gh jq", notify)
        self.assertLess(notify.index("apt-get install"), notify.index("Mint pulse-deploy"))

    def test_source_metadata_uses_source_repository_token(self) -> None:
        notify = self.workflow.split("  notify-pulse-deploy:", 1)[1]
        self.assertIn("SOURCE_GH_TOKEN: ${{ github.token }}", notify)
        self.assertIn('published_at=$(GH_TOKEN="$SOURCE_GH_TOKEN" gh api', notify)
        self.assertIn("GH_TOKEN: ${{ steps.app-token.outputs.token }}", notify)


if __name__ == "__main__":
    unittest.main()
