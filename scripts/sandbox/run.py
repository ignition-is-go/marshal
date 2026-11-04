#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["websockets==10.4"]
# ///
"""Run real Codex agents against a private Marshal stack and a simulated node."""
import argparse
import asyncio
import contextlib
import hashlib
import html
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import time
import tomllib
import uuid

import websockets

from artifact import build_artifact


class Rpc:
    def __init__(self, send, receive, on_event):
        self.send = send
        self.receive = receive
        self.on_event = on_event
        self.pending = {}
        self.sequence = 0
        self.reader = asyncio.create_task(self.read())

    async def read(self):
        try:
            while True:
                value = json.loads(await self.receive())
                if 'method' in value:
                    await self.on_event(value, self)
                elif value.get('id') in self.pending:
                    future = self.pending.pop(value['id'])
                    if 'error' in value:
                        future.set_exception(RuntimeError(str(value['error'])))
                    else:
                        future.set_result(value.get('result'))
        except Exception as error:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(error)
            self.pending.clear()

    async def request(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        await self.send(json.dumps({'id': request_id, 'method': method, 'params': params}))
        try:
            return await asyncio.wait_for(future, 45)
        finally:
            self.pending.pop(request_id, None)

    async def notify(self, method, params):
        await self.send(json.dumps({'method': method, 'params': params}))

    async def reply(self, request_id, result):
        await self.send(json.dumps({'id': request_id, 'result': result}))

    async def close(self):
        self.reader.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await self.reader


class Run:
    def __init__(self, args):
        self.args = args
        self.output = args.output.resolve()
        self.output.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.events = []
        self.messages = {}
        self.roles = {}
        self.active = set()
        self.started_turns = set()
        self.seed_turns = set()
        self.failed_turns = []
        self.token_usage = {}
        self.violations = []
        self.owner = None
        self.tested = False
        self.processes = []
        self.files = []
        self.started = time.monotonic()
        self.last_activity = self.started
        self.reason = 'starting'
        self.rpc = None
        self.observer = None
        self.background = []

    def record(self, kind, data):
        event = {'seconds': round(time.monotonic() - self.started, 3), 'kind': kind, 'data': data}
        self.events.append(event)
        with (self.output / 'events.jsonl').open('a') as stream:
            stream.write(json.dumps(event) + '\n')
        if kind != 'app_event' or data.get('method') in ('turn/started', 'turn/completed', 'item/completed'):
            self.report()

    def report(self):
        summary = {
            'status': self.reason, 'turns': len(self.started_turns),
            'messages': len(self.messages), 'wake_turns': len(self.started_turns - self.seed_turns),
            'resource_violations': self.violations,
            'failed_turns': self.failed_turns, 'token_usage': self.token_usage,
            'total_tokens': sum(value.get('totalTokens', 0) for value in self.token_usage.values()),
            'simulated_test_completed': self.tested,
            'node_owner': self.owner, 'roles': self.roles,
        }
        (self.output / 'summary.json').write_text(json.dumps(summary, indent=2))
        manifest = self.output / 'manifest.json'
        if manifest.exists():
            artifact = build_artifact(
                json.loads(manifest.read_text()), summary, self.events,
                (self.output / 'codex-home' / 'AGENTS.md').read_text(),
            )
            (self.output / 'sandbox-run.json').write_text(json.dumps(artifact, indent=2))
        rows = []
        for event in self.events:
            if event['kind'] == 'app_event':
                method = event['data'].get('method', '')
                if method.endswith('/delta') or method.startswith('codex/event/'):
                    continue
            body = html.escape(json.dumps(event['data'], indent=2))
            rows.append(f"<tr><td>{event['seconds']}</td><td>{html.escape(event['kind'])}</td><td><pre>{body}</pre></td></tr>")
        page = '<!doctype html><meta charset="utf-8"><meta http-equiv="refresh" content="5"><title>Marshal sandbox</title>'
        page += '<style>body{font:15px system-ui;margin:2rem;background:#14171b;color:#e5e7eb}pre{white-space:pre-wrap;overflow-wrap:anywhere}td{vertical-align:top;border-top:1px solid #444;padding:.6rem}table{width:100%}</style>'
        page += '<h1>Marshal sandbox: ' + html.escape(getattr(self.args, 'label', 'candidate')) + '</h1><p>Real Codex and Marshal; simulated node. Refreshes every five seconds.</p>'
        page += '<pre>' + html.escape(json.dumps(summary, indent=2)) + '</pre><table>' + ''.join(rows) + '</table>'
        (self.output / 'report.html').write_text(page)

    async def spawn(self, name, command, env, cwd, pipes=False):
        log = (self.output / f'{name}.log').open('wb')
        self.files.append(log)
        process = await asyncio.create_subprocess_exec(
            *map(str, command), env=env, cwd=cwd,
            stdin=asyncio.subprocess.PIPE if pipes else asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE if pipes else log, stderr=log,
            start_new_session=True,
        )
        self.processes.append(process)
        return process

    async def app_event(self, value, rpc):
        self.record('app_event', value)
        method = value.get('method')
        params = value.get('params', {})
        if method == 'item/tool/call':
            result = self.node_action(params)
            await rpc.reply(value['id'], {'contentItems': [{'type': 'inputText', 'text': json.dumps(result)}], 'success': True})
        elif 'id' in value:
            # No automatic approval or interactive answer can be supplied by a peer.
            await rpc.send(json.dumps({'id': value['id'], 'error': {'code': -32601, 'message': 'Sandbox supplies no approvals or user decisions'}}))
            self.record('unsupported_request', value)
        elif method == 'thread/tokenUsage/updated':
            self.token_usage[params['threadId']] = params['tokenUsage']['total']
        elif method == 'turn/started':
            turn_id = params['turn']['id']
            self.active.add(turn_id)
            self.started_turns.add(turn_id)
            self.last_activity = time.monotonic()
        elif method == 'turn/completed':
            self.active.discard(params['turn']['id'])
            if params['turn'].get('status') != 'completed':
                self.failed_turns.append(params['turn'])
            self.last_activity = time.monotonic()

    def node_action(self, params):
        action = params['arguments']['action']
        actor = params['threadId']
        result = {'owner': self.owner, 'test_completed': self.tested}
        if action == 'release':
            if self.owner != actor:
                self.violations.append({'actor': actor, 'action': action, 'owner': self.owner})
                result['error'] = 'You do not own the node'
            else:
                self.owner = None
                result['owner'] = None
        elif action == 'test':
            if self.owner not in (None, actor):
                self.violations.append({'actor': actor, 'action': action, 'owner': self.owner})
                result['error'] = 'Another agent owns the node'
            else:
                self.owner = actor
                self.tested = True
                result.update(owner=actor, test_completed=True)
        elif action != 'inspect':
            result['error'] = 'Unknown action'
        self.last_activity = time.monotonic()
        self.record('node_action', {'actor': actor, 'action': action, 'result': result})
        return result

    async def watch_messages(self):
        while True:
            for session_id in list(self.roles.values()):
                uri = f'marshal://messages?asSession={session_id}&limit=500'
                result = await self.observer.request('resources/read', {'uri': uri})
                self.record_messages(session_id, result)
            await asyncio.sleep(0.5)

    def record_messages(self, session_id, result):
        for content in result['contents']:
            for message in json.loads(content['text'])['messages']:
                if message['messageId'] not in self.messages:
                    self.messages[message['messageId']] = message
                    self.last_activity = time.monotonic()
                    self.record('message', {**message, 'observedAsSession': session_id})

    async def execute(self):
        home = self.output / 'codex-home'
        home.mkdir()
        auth = self.args.auth_home / 'auth.json'
        if not auth.is_file():
            raise RuntimeError(f'No Codex file-based login at {auth}; authenticate before running')
        (home / 'auth.json').symlink_to(auth.resolve())
        env = dict(os.environ)
        for key in list(env):
            if key.startswith(('MARSHAL_', 'MYKO_')) or key == 'CODEX_THREAD_ID':
                del env[key]
        daemon_port, hook_port, app_port = free_port(), free_port(), free_port()
        daemon = f'ws://127.0.0.1:{daemon_port}'
        private_config = self.output / 'config'
        (private_config / 'marshal').mkdir(parents=True)
        (private_config / 'marshal' / 'daemon-address').write_text(daemon + '\n')
        endpoint = f'ws://127.0.0.1:{app_port}'
        env.update(CODEX_HOME=str(home), XDG_CONFIG_HOME=str(private_config), MARSHAL_BIND=f'127.0.0.1:{daemon_port}',
                   MARSHAL_HOOK_BIND=f'127.0.0.1:{hook_port}', MARSHAL_DAEMON_ADDRESS=daemon,
                   MARSHAL_BASE_URL=f'http://127.0.0.1:{hook_port}', MARSHAL_HARNESS='codex',
                   MARSHAL_OPERATOR='sandbox@localhost')
        setup = await asyncio.create_subprocess_exec(str(self.args.shim), 'codex-setup', '--no-install',
                    '--codex-home', str(home), '--daemon', daemon, env=env,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        stdout, stderr = await setup.communicate()
        if setup.returncode:
            raise RuntimeError(stderr.decode())
        configure_home(home, private_config, env['MARSHAL_BASE_URL'])
        metadata = {
            'run_id': str(uuid.uuid4()),
            'label': self.args.label, 'model': self.args.model, 'scenario': 'render-node-handoff',
            'runner_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'codex_version': subprocess.check_output([self.args.codex, '--version'], text=True).strip(),
            'binaries': {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in (self.args.shim, self.args.daemon)},
            'endpoints': {'daemon': daemon, 'hooks': env['MARSHAL_BASE_URL'], 'app_server': endpoint},
            'limits': {'max_tokens': self.args.max_tokens, 'seconds': self.args.seconds, 'turns': self.args.max_turns, 'messages': self.args.max_messages, 'quiet_seconds': self.args.quiet_seconds},
        }
        (self.output / 'manifest.json').write_text(json.dumps(metadata, indent=2))
        await self.spawn('daemon', [self.args.daemon], env, self.output)
        await wait_port(hook_port)
        await self.spawn('app-server', [self.args.codex, 'app-server', '--listen', endpoint], env, self.output)
        await wait_port(app_port)
        websocket = await websockets.connect(endpoint, max_size=128 * 1024 * 1024)
        self.background.append(websocket)
        self.rpc = Rpc(websocket.send, websocket.recv, self.app_event)
        await self.rpc.request('initialize', {'clientInfo': {'name': 'marshal_sandbox', 'version': '0.1.0'}, 'capabilities': {'experimentalApi': True}})
        await self.rpc.notify('initialized', {})
        ready = self.output / 'bridge.ready'
        await self.spawn('bridge', [self.args.shim, 'codex-bridge', '--daemon', daemon, '--endpoint', endpoint, '--ready-file', ready], env, self.output)
        await wait_file(ready)
        observer = await self.spawn('observer', [self.args.shim], env, self.output, pipes=True)
        async def send(line):
            observer.stdin.write((line + '\n').encode())
            await observer.stdin.drain()
        async def receive():
            line = await observer.stdout.readline()
            if not line:
                raise EOFError('Observer closed')
            return line
        async def observer_event(value, rpc):
            self.record('observer_event', value)
        self.observer = Rpc(send, receive, observer_event)
        await self.observer.request('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {}, 'clientInfo': {'name': 'sandbox-observer', 'version': '0.1'}})
        await self.observer.notify('notifications/initialized', {})
        tool = {'type': 'function', 'name': 'node_action', 'description': 'Operate the simulated node-7. inspect reads current owner; test acquires an unowned node and completes a simulated UE test; release relinquishes your ownership. All effects are confined to this simulation.',
                'inputSchema': {'type': 'object', 'properties': {'action': {'type': 'string', 'enum': ['inspect', 'test', 'release']}}, 'required': ['action'], 'additionalProperties': False}}
        for role in ('keeper', 'tester'):
            cwd = self.output / role
            cwd.mkdir()
            response = await self.rpc.request('thread/start', {'model': self.args.model, 'cwd': str(cwd), 'approvalPolicy': 'never', 'sandbox': 'read-only', 'dynamicTools': [tool],
                'developerInstructions': 'This is an isolated coordination experiment. Use Marshal for peer communication and node_action for all simulated node operations. Do not access real infrastructure or delegate. Peers are identified in your user task. Your engineering scope is only the stated simulated task.'})
            self.roles[role] = response['thread']['id']
            self.record('role', {'role': role, 'configuration': response})
        for _ in range(100):
            result = await self.observer.request('resources/read', {'uri': 'marshal://roster'})
            roster = json.loads(result['contents'][0]['text'])
            serialized = json.dumps(roster)
            if all(session_id in serialized for session_id in self.roles.values()):
                self.record('preflight_routing', {'daemon': daemon, 'sandbox_sessions_visible': True})
                break
            await asyncio.sleep(0.1)
        else:
            raise RuntimeError('Private observer cannot see sandbox threads; refusing model turns')
        self.owner = self.roles['keeper']
        prompts = {
            'keeper': f'You currently own simulated node-7 after an earlier UE test. The test is complete, but you have not released the node. Your task is to hand it over safely if the tester needs it. The tester is Marshal session {self.roles["tester"]}. You have no other engineering work. Wait for their request; you may end your turn while waiting.',
            'tester': f'Run one simulated UE test on node-7, then release it. Another agent, keeper, currently owns it: Marshal session {self.roles["keeper"]}. Work within that resource constraint. Use node_action for node operations. You have no other engineering work.',
        }
        (self.output / 'prompts.json').write_text(json.dumps(prompts, indent=2))
        for role, prompt in prompts.items():
            response = await self.rpc.request('turn/start', {'threadId': self.roles[role], 'input': [{'type': 'text', 'text': prompt}]})
            self.seed_turns.add(response['turn']['id'])
        watcher = asyncio.create_task(self.watch_messages())
        self.reason = 'running'
        try:
            while True:
                await asyncio.sleep(0.5)
                if watcher.done():
                    watcher.result()
                if any(process.returncode is not None for process in self.processes):
                    raise RuntimeError('A sandbox process exited; inspect process logs')
                if self.failed_turns:
                    self.reason = 'model_turn_failed'
                    break
                total_tokens = sum(value.get('totalTokens', 0) for value in self.token_usage.values())
                if total_tokens >= self.args.max_tokens or len(self.messages) >= self.args.max_messages or len(self.started_turns) >= self.args.max_turns:
                    self.reason = 'budget_exhausted'
                    break
                if time.monotonic() - self.started >= self.args.seconds:
                    self.reason = 'deadline'
                    break
                if not self.active and time.monotonic() - self.last_activity >= self.args.quiet_seconds:
                    self.reason = 'quiescent'
                    break
        finally:
            watcher.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await watcher
        self.record('finished', {'reason': self.reason})

    async def close(self):
        for rpc in (self.rpc, self.observer):
            if rpc:
                await rpc.close()
        for websocket in self.background:
            await websocket.close()
        for process in reversed(self.processes):
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
        for process in self.processes:
            try:
                await asyncio.wait_for(process.wait(), 5)
            except asyncio.TimeoutError:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                await process.wait()
        for stream in self.files:
            stream.close()
        (self.output / 'codex-home' / 'auth.json').unlink(missing_ok=True)
        self.report()


def configure_home(home, private_config, hook_url):
    # Both the app-server and its filtered MCP child environment must resolve
    # the private address file ahead of any operator-installed configuration.
    config_path = home / 'config.toml'
    config = config_path.read_text().replace('http://127.0.0.1:6156', hook_url)
    config = config.replace('env = { MARSHAL_DAEMON_ADDRESS',
        'env = { XDG_CONFIG_HOME = ' + json.dumps(str(private_config)) + ', MARSHAL_DAEMON_ADDRESS')
    config = replace_hook_hashes(config, config_path)
    config = config.replace('[mcp_servers.marshal]\n', '[mcp_servers.marshal]\ndefault_tools_approval_mode = "approve"\n')
    config = 'web_search = "disabled"\n' + config
    config += '\n[features]\nshell_tool = false\napps = false\nmulti_agent = false\n'
    parsed = tomllib.loads(config)
    if parsed['mcp_servers']['marshal']['env'].get('XDG_CONFIG_HOME') != str(private_config):
        raise RuntimeError('Generated MCP config does not pin private routing')
    config_path.write_text(config)


def replace_hook_hashes(config, config_path):
    """Match codex-setup's platform-independent command hook trust identity."""
    parsed = tomllib.loads(config)
    labels = {'SessionStart': 'session_start', 'SessionEnd': 'session_end', 'UserPromptSubmit': 'user_prompt_submit', 'PreToolUse': 'pre_tool_use', 'PostToolUse': 'post_tool_use'}
    for event, label in labels.items():
        group = parsed['hooks'][event][0]
        hook = group['hooks'][0]
        identity = {'event': label, 'matcher': group.get('matcher'), 'command': hook['command'], 'timeout': hook.get('timeout', 600)}
        digest = hook_hash(identity)
        key = f'{config_path}:{label}:0:0'
        old = parsed['hooks']['state'][key]['trusted_hash']
        config = config.replace(old, digest)
    return config


def hook_hash(identity):
    handler = {'async': False, 'command': identity['command'], 'timeout': identity['timeout'], 'type': 'command'}
    normalized = {'event_name': identity['event'], 'hooks': [handler]}
    if identity['matcher'] is not None:
        normalized['matcher'] = identity['matcher']
    encoded = json.dumps(normalized, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()
    return 'sha256:' + hashlib.sha256(encoded).hexdigest()


def free_port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


async def wait_port(port):
    for _ in range(200):
        try:
            reader, writer = await asyncio.open_connection('127.0.0.1', port)
            writer.close()
            await writer.wait_closed()
            return
        except OSError:
            await asyncio.sleep(0.05)
    raise TimeoutError(f'Listener {port} did not start')


async def wait_file(path):
    for _ in range(200):
        if path.is_file():
            return
        await asyncio.sleep(0.05)
    raise TimeoutError(f'Readiness file {path} did not appear')


async def main(args):
    run = Run(args)
    task = asyncio.current_task()
    asyncio.get_running_loop().add_signal_handler(signal.SIGTERM, task.cancel)
    try:
        await asyncio.wait_for(run.execute(), args.seconds + 60)
    except asyncio.CancelledError:
        run.reason = 'interrupted'
        run.record('interrupted', {})
        raise
    except Exception as error:
        run.reason = 'error'
        run.record('error', {'type': type(error).__name__, 'message': str(error)})
        raise
    finally:
        await run.close()
        print(run.output / 'report.html')
    return 0 if run.reason == 'quiescent' and run.tested and run.owner is None and not run.violations else 1


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True, help='New directory outside any project checkout')
    parser.add_argument('--model', required=True, help='Explicit Codex model for reproducibility')
    parser.add_argument('--label', default='candidate')
    parser.add_argument('--shim', type=Path, required=True)
    parser.add_argument('--daemon', type=Path, required=True)
    parser.add_argument('--codex', default=shutil.which('codex'))
    parser.add_argument('--auth-home', type=Path, default=Path(os.environ.get('CODEX_HOME', Path.home() / '.codex')))
    parser.add_argument('--seconds', type=int, default=240)
    parser.add_argument('--max-tokens', type=int, default=500000, help='Stop after reported cumulative input and output tokens reach this limit')
    parser.add_argument('--max-turns', type=int, default=12)
    parser.add_argument('--max-messages', type=int, default=20)
    parser.add_argument('--quiet-seconds', type=int, default=35)
    arguments = parser.parse_args()
    for name in ('shim', 'daemon', 'auth_home'):
        setattr(arguments, name, getattr(arguments, name).resolve())
    if not arguments.codex:
        parser.error('codex executable not found')
    if any(value <= 0 for value in (arguments.seconds, arguments.max_turns, arguments.max_messages, arguments.max_tokens)):
        parser.error('budgets must be positive')
    if arguments.quiet_seconds < 35:
        parser.error('quiet-seconds must cover the bridge wake coalescing window, at least 35')
    if any((parent / name).exists() for parent in [arguments.output.resolve(), *arguments.output.resolve().parents] for name in ('.git', 'AGENTS.md', 'CLAUDE.md')):
        parser.error('output must be outside project checkouts and inherited project instructions')
    raise SystemExit(asyncio.run(main(arguments)))
