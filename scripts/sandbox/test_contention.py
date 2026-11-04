import asyncio
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest

from contention import ContentionRun, answer_schema


class ContentionTests(unittest.IsolatedAsyncioTestCase):
    async def test_answer_schema_distinguishes_route_count_from_route_names(self):
        schema = answer_schema({'unique_routes': 3, 'duplicated_routes': ['aux']})
        self.assertEqual(schema['properties']['unique_routes'], {'type': 'integer'})
        self.assertEqual(schema['properties']['duplicated_routes'], {'type': 'array', 'items': {'type': 'string'}})
        self.assertNotIn('aux', str(schema))

    async def test_reservation_exposure_does_not_require_an_owner_update(self):
        with tempfile.TemporaryDirectory() as directory:
            run = ContentionRun(SimpleNamespace(output=Path(directory) / 'run'))
            run.roles = {'keeper': 'one', 'tester': 'two'}
            run.owner = 'one'
            run.node_action({'threadId': 'two', 'arguments': {'action': 'inspect'}})
            self.assertTrue(run.checks()['Contention exercised'])
            self.assertFalse(run.messages)

    async def test_waiting_test_does_not_hide_resource_state(self):
        with tempfile.TemporaryDirectory() as directory:
            run = ContentionRun(SimpleNamespace(output=Path(directory) / 'run'))
            run.roles = {'keeper': 'one', 'tester': 'two'}
            run.prompts()
            run.jobs['keeper-prep']['done'] = True
            run.test_seconds = 0.02
            task = asyncio.create_task(run.node_test({'threadId': 'one', 'arguments': {'profile': 'warm'}}))
            await asyncio.sleep(0)
            observed = run.node_action({'threadId': 'two', 'arguments': {'action': 'inspect'}})
            self.assertEqual(observed['owner'], 'one')
            self.assertTrue(observed['busy'])
            self.assertFalse(task.done())
            await task
            self.assertEqual(run.node_tests, {'one': 1})

    async def test_scope_attempt_and_missing_fault_coverage_cannot_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            run = ContentionRun(SimpleNamespace(output=Path(directory) / 'run'))
            run.roles = {'keeper': 'one', 'tester': 'two'}
            run.prompts()
            result = run.work_action({'threadId': 'one', 'arguments': {'action': 'publish'}})
            self.assertIn('error', result)
            self.assertEqual(len(run.scope_violations), 1)
            for job in run.jobs.values():
                job['done'] = True
            run.tested = True
            run.owner = None
            self.assertFalse(run.goal_met())
            self.assertFalse(run.checks()['Late duplicate exercised'])
