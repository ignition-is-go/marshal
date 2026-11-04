#!/usr/bin/env python3
"""Export a sandbox run as a compact, versioned artifact for the Marshal UI."""
import argparse
import hashlib
import json
from pathlib import Path


def build_artifact(manifest, summary, events, instructions):
    usage = list(summary.get('token_usage', {}).values())
    identity = json.dumps({'roles': summary['roles'], 'binaries': manifest['binaries']}, sort_keys=True)
    names = {session: role for role, session in summary['roles'].items()}
    timeline = []
    for event in events:
        data = event['data']
        row = {'seconds': event['seconds'], 'kind': event['kind'], 'actor': None, 'recipient': None, 'text': ''}
        if event['kind'] == 'message':
            row.update(actor=data['fromSessionId'], recipient=data.get('toSessionId', data.get('toRoomId')), text=data['body'])
        elif event['kind'] == 'node_action':
            result = data['result']
            owner = result.get('owner')
            owner_label = names.get(owner, owner) if owner else 'unowned'
            state = 'test completed' if result.get('test_completed') else 'test pending'
            text = f"{data['action'].capitalize()} node-7 · owner: {owner_label} · {state}"
            if result.get('error'):
                text += f" · {result['error']}"
            row.update(actor=data['actor'], text=text)
        elif event['kind'] == 'work_action':
            result = data['result']
            status = result.get('error') or ('accepted' if result.get('accepted') else 'inspected')
            row.update(actor=data['actor'], text=f"{data['action'].capitalize()} {data.get('job_id') or 'assigned jobs'} · {status}")
        elif event['kind'] == 'intervention':
            row.update(recipient=data.get('recipient'), text=data['description'])
        elif event['kind'] in ('error', 'finished', 'interrupted'):
            row['text'] = json.dumps(data)
        elif event['kind'] == 'app_event':
            method = data.get('method')
            params = data.get('params', {})
            row['actor'] = params.get('threadId')
            if method in ('turn/started', 'turn/completed'):
                row.update(kind=method.replace('/', '_'), text=json.dumps(params['turn']))
            elif method == 'item/completed':
                item = params['item']
                if item['type'] == 'agentMessage':
                    row.update(kind='agent_message', text=item['text'])
                elif item['type'] == 'userMessage':
                    row.update(kind='user_message', text='\n'.join(part['text'] for part in item['content'] if part['type'] == 'text'))
                elif item['type'] in ('mcpToolCall', 'dynamicToolCall'):
                    row.update(kind='tool_call', text=json.dumps(item))
                else:
                    continue
            else:
                continue
        else:
            continue
        timeline.append(row)
    for name, passed in summary.get('checks', {}).items():
        timeline.append({
            'seconds': timeline[-1]['seconds'] if timeline else 0,
            'kind': 'scenario_check', 'actor': None, 'recipient': None,
            'text': f"{'Passed' if passed else 'Not satisfied'} · {name}",
        })
    return {
        'schema_version': 1, 'run_id': manifest.get('run_id') or hashlib.sha256(identity.encode()).hexdigest(),
        'label': manifest['label'], 'model': manifest['model'], 'scenario': manifest['scenario'],
        'status': summary['status'], 'tested': summary['simulated_test_completed'] and all(summary.get('checks', {}).values()),
        'released': summary['node_owner'] is None,
        'resource_violations': len(summary['resource_violations']), 'failed_turns': len(summary['failed_turns']),
        'messages': summary['messages'], 'turns': summary['turns'], 'wake_turns': summary.get('wake_turns', 0),
        'input_tokens': sum(value['inputTokens'] for value in usage),
        'cached_input_tokens': sum(value['cachedInputTokens'] for value in usage),
        'output_tokens': sum(value['outputTokens'] for value in usage),
        'codex_version': manifest['codex_version'], 'binaries': manifest['binaries'],
        'roles': summary['roles'], 'instructions': instructions, 'timeline': timeline,
    }


def export(directory):
    artifact = build_artifact(
        json.loads((directory / 'manifest.json').read_text()),
        json.loads((directory / 'summary.json').read_text()),
        [json.loads(line) for line in (directory / 'events.jsonl').read_text().splitlines()],
        (directory / 'codex-home' / 'AGENTS.md').read_text(),
    )
    path = directory / 'sandbox-run.json'
    path.write_text(json.dumps(artifact, indent=2))
    return path


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    args = parser.parse_args()
    print(export(args.directory))
