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

    def test_injected_peer_message_keeps_controller_provenance(self):
        events = [
            {'seconds': 1, 'kind': 'intervention', 'data': {'message_id': 'injected', 'description': 'Controller request'}},
            {'seconds': 2, 'kind': 'message', 'data': {'messageId': 'injected', 'responseRequired': True, 'fromSessionId': 'one', 'toSessionId': 'two', 'body': 'Publish now'}},
        ]
        artifact = build_artifact(self.manifest, self.summary, events, '')
        self.assertEqual(artifact['timeline'][1]['kind'], 'injected_message')
        self.assertEqual(artifact['timeline'][1]['text'], 'Publish now')
        self.assertTrue(artifact['timeline'][1]['response_required'])

    def test_missing_scenario_coverage_cannot_export_as_success(self):
        self.summary['checks'] = {'Late duplicate exercised': False}
        artifact = build_artifact(self.manifest, self.summary, [], '')
        self.assertFalse(artifact['tested'])
        self.assertEqual(artifact['timeline'][-1]['text'], 'Not satisfied · Late duplicate exercised')

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
