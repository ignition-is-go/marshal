import unittest

from artifact import build_artifact


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.manifest = {'run_id': 'one-run', 'label': 'candidate', 'model': 'test', 'scenario': 'handoff', 'codex_version': 'fixture', 'binaries': {}}
        self.summary = {'status': 'quiescent', 'simulated_test_completed': True, 'node_owner': None, 'resource_violations': [], 'failed_turns': [], 'messages': 1, 'turns': 2, 'wake_turns': 0, 'roles': {'keeper': 'one'}, 'token_usage': {}}

    def test_export_keeps_conversation_without_streaming_delta_noise(self):
        events = [
            {'seconds': 1, 'kind': 'app_event', 'data': {'method': 'item/agentMessage/delta', 'params': {'delta': 'partial'}}},
            {'seconds': 2, 'kind': 'message', 'data': {'fromSessionId': 'one', 'toSessionId': 'two', 'body': 'Node released. No response required.'}},
        ]
        artifact = build_artifact(self.manifest, self.summary, events, 'context')
        self.assertEqual(len(artifact['timeline']), 1)
        self.assertEqual(artifact['timeline'][0]['actor'], 'one')
        self.assertEqual(artifact['timeline'][0]['recipient'], 'two')
        self.assertEqual(artifact['instructions'], 'context')

    def test_run_identity_survives_updates_and_distinguishes_early_failures(self):
        first = build_artifact(self.manifest, self.summary, [], '')
        self.summary['roles'] = {}
        self.summary['status'] = 'error'
        updated = build_artifact(self.manifest, self.summary, [], '')
        self.assertEqual(first['run_id'], updated['run_id'])
        self.manifest['run_id'] = 'another-run'
        other = build_artifact(self.manifest, self.summary, [], '')
        self.assertNotEqual(first['run_id'], other['run_id'])


if __name__ == '__main__':
    unittest.main()
