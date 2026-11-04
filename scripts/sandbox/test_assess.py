import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest

from assess import assess, replay_oracle


class AssessmentTests(unittest.TestCase):
    def test_raw_oracle_detects_overlap_independently_of_summary(self):
        events = [
            {'kind': 'node_action', 'data': {'action': 'test_started', 'actor': 'one'}},
            {'kind': 'node_action', 'data': {'action': 'test_started', 'actor': 'two'}},
        ]
        self.assertIn('Overlapping resource checks', replay_oracle(events, 'contention'))

    def test_early_failure_without_events_remains_in_report(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'campaign.json').write_text(json.dumps({'pairs': [['independent', 1]]}))
            (root / 'results.json').write_text(json.dumps([{'name': 'failed', 'scenario': 'independent', 'condition': 'candidate', 'repeat': 1, 'exit_code': 2, 'elapsed_seconds': 1, 'summary': {}}]))
            with contextlib.redirect_stdout(io.StringIO()):
                report = assess(root)
            self.assertEqual(report['completed_trials'], 1)
            self.assertFalse(report['trials'][0]['passed'])

    def test_injections_do_not_count_as_agent_messages_and_failed_checks_remain_failed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'campaign.json').write_text(json.dumps({'pairs': [['independent', 1]]}))
            summary = {'status': 'quiescent', 'checks': {'No unnecessary peer messages': False},
                       'turns': 2, 'wake_turns': 0, 'total_tokens': 100, 'roles': {'keeper': 'one', 'tester': 'two'}}
            (root / 'results.json').write_text(json.dumps([{'name': 'trial', 'scenario': 'independent', 'condition': 'candidate', 'repeat': 1, 'exit_code': 1, 'elapsed_seconds': 40, 'summary': summary}]))
            trial = root / 'trial'
            trial.mkdir()
            (trial / 'scenario-results.json').write_text(json.dumps({'controller_message_ids': ['injection']}))
            events = [
                {'seconds': 1, 'kind': 'message', 'data': {'messageId': 'injection', 'fromSessionId': 'one', 'toSessionId': 'two', 'body': 'Response required: test'}},
                {'seconds': 2, 'kind': 'message', 'data': {'messageId': 'actual', 'fromSessionId': 'two', 'toSessionId': 'one', 'body': 'Ready?', 'responseRequired': True}},
            ]
            (trial / 'events.jsonl').write_text('\n'.join(json.dumps(event) for event in events))
            with contextlib.redirect_stdout(io.StringIO()):
                report = assess(root)
            result = report['trials'][0]
            self.assertFalse(result['passed'])
            self.assertEqual(result['agent_messages'], 1)
            self.assertEqual(result['injected_messages'], 1)
            self.assertEqual(result['body_labels'], 0)
            self.assertEqual(result['structured_required'], 1)
