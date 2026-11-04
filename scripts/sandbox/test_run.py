import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import tomllib
import unittest

from run import Run, configure_home, hook_hash


class ResourceTests(unittest.TestCase):
    def test_observer_deduplicates_actual_camel_case_wire_messages(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Run(SimpleNamespace(output=Path(directory) / 'run'))
            result = {'contents': [{'text': json.dumps({'messages': [{'messageId': 'one', 'body': 'No response required'}]})}]}
            run.record_messages('sender', result)
            run.record_messages('recipient', result)
            self.assertEqual(len(run.messages), 1)
            self.assertEqual([event['data']['observedAsSession'] for event in run.events], ['sender'])

    def test_conflicting_use_is_observed_and_handoff_can_complete(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Run(SimpleNamespace(output=Path(directory) / 'run'))
            run.owner = 'keeper'
            result = run.node_action({'threadId': 'tester', 'arguments': {'action': 'test'}})
            self.assertIn('error', result)
            self.assertEqual(run.owner, 'keeper')
            self.assertFalse(run.tested)
            self.assertEqual(len(run.violations), 1)
            run.node_action({'threadId': 'keeper', 'arguments': {'action': 'release'}})
            run.node_action({'threadId': 'tester', 'arguments': {'action': 'test'}})
            run.node_action({'threadId': 'tester', 'arguments': {'action': 'release'}})
            self.assertTrue(run.tested)
            self.assertIsNone(run.owner)


class LifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def test_cleanup_stops_process_and_removes_auth_link(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Run(SimpleNamespace(output=Path(directory) / 'run'))
            home = run.output / 'codex-home'
            home.mkdir()
            source = Path(directory) / 'auth-fixture'
            source.write_text('fixture')
            (home / 'auth.json').symlink_to(source)
            process = await run.spawn('child', [sys.executable, '-c', 'import time; time.sleep(30)'], dict(os.environ), run.output)
            await run.close()
            self.assertIsNotNone(process.returncode)
            self.assertFalse((home / 'auth.json').exists())
            self.assertEqual(source.read_text(), 'fixture')


@unittest.skipUnless(os.environ.get('MARSHAL_TEST_SHIM'), 'Set MARSHAL_TEST_SHIM to the built binary')
class ConfigContractTests(unittest.TestCase):
    def test_private_routing_survives_filtered_child_environment_and_trust_matches_rust(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory) / 'codex'
            subprocess.run([os.environ['MARSHAL_TEST_SHIM'], 'codex-setup', '--no-install', '--codex-home', str(home), '--daemon', 'ws://127.0.0.1:45678'], check=True, capture_output=True)
            original = tomllib.loads((home / 'config.toml').read_text())
            for event, label in [('SessionStart', 'session_start'), ('SessionEnd', 'session_end'), ('UserPromptSubmit', 'user_prompt_submit'), ('PreToolUse', 'pre_tool_use'), ('PostToolUse', 'post_tool_use')]:
                group = original['hooks'][event][0]
                hook = group['hooks'][0]
                key = f'{home / "config.toml"}:{label}:0:0'
                self.assertEqual(original['hooks']['state'][key]['trusted_hash'], hook_hash({'event': label, 'matcher': group.get('matcher'), 'command': hook['command'], 'timeout': hook.get('timeout', 600)}))
            private = Path(directory) / 'private'
            configure_home(home, private, 'http://127.0.0.1:45679')
            config = tomllib.loads((home / 'config.toml').read_text())
            self.assertEqual(config['mcp_servers']['marshal']['env']['XDG_CONFIG_HOME'], str(private))
            self.assertEqual(config['mcp_servers']['marshal']['env']['MARSHAL_DAEMON_ADDRESS'], 'ws://127.0.0.1:45678')
            self.assertIn('http://127.0.0.1:45679', config['hooks']['SessionStart'][0]['hooks'][0]['command'])
            self.assertNotEqual(original['hooks']['state'], config['hooks']['state'])
            self.assertFalse(config['features']['apps'])
            self.assertFalse(config['features']['shell_tool'])
            self.assertFalse(config['features']['multi_agent'])


if __name__ == '__main__':
    unittest.main()
