#!/usr/bin/env python3
from pathlib import Path
import unittest


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"


class ReleaseDispatchContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text()

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


if __name__ == "__main__":
    unittest.main()
