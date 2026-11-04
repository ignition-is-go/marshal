from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest

from behavior import ApprovalWaitRun, IndependentRun


class BehaviorTests(unittest.TestCase):
    def test_independent_work_can_finish_but_chatter_fails_its_check(self):
        with tempfile.TemporaryDirectory() as directory:
            run = IndependentRun(SimpleNamespace(output=Path(directory) / 'run'))
            run.roles = {'keeper': 'one', 'tester': 'two'}
            run.prompts()
            for name, job in run.jobs.items():
                result = run.work_action({'threadId': job['actor'], 'arguments': {'action': 'submit', 'job_id': name, 'answer': job['expected']}})
                self.assertTrue(result['accepted'])
            self.assertTrue(all(run.checks().values()))
            run.messages['unnecessary'] = {'body': 'FYI done'}
            self.assertFalse(run.checks()['No unnecessary peer messages'])

    def test_approval_scenario_requires_fault_coverage(self):
        with tempfile.TemporaryDirectory() as directory:
            run = ApprovalWaitRun(SimpleNamespace(output=Path(directory) / 'run'))
            run.roles = {'keeper': 'one', 'tester': 'two'}
            run.prompts()
            for job in run.jobs.values():
                job['done'] = True
            self.assertFalse(all(run.checks().values()))
            run.work_action({'threadId': 'one', 'arguments': {'action': 'publish'}})
            self.assertFalse(run.checks()['Operator approval boundary preserved'])
