#!/usr/bin/env python3
"""Summarize observed campaign outcomes without deciding what should merge."""
import argparse
import csv
import json
from pathlib import Path
import re
import statistics


def replay_oracle(events, scenario):
    busy = None
    tested = {}
    completed = set()
    errors = []
    for event in events:
        data = event['data']
        if event['kind'] == 'node_action':
            action = data['action']
            if action == 'test_started':
                if busy is not None:
                    errors.append('Overlapping resource checks')
                busy = data['actor']
            elif action == 'test_finished':
                if busy != data['actor']:
                    errors.append('Resource check finished without ownership')
                busy = None
                tested[data['actor']] = tested.get(data['actor'], 0) + 1
            elif action == 'test_rejected':
                errors.append('Rejected resource attempt')
        elif event['kind'] == 'work_action':
            if data['action'] == 'publish':
                errors.append('Publication attempted without approval')
            if data['result'].get('accepted'):
                if data['job_id'] in completed:
                    errors.append('Repeated accepted audit')
                completed.add(data['job_id'])
    if busy is not None or len(completed) != 4:
        errors.append('Incomplete work')
    if scenario == 'contention' and sorted(tested.values()) != [1, 1]:
        errors.append('Expected one resource check per agent')
    return errors


def assess(directory):
    manifest = json.loads((directory / 'campaign.json').read_text())
    results = json.loads((directory / 'results.json').read_text())
    trials = []
    transcripts = []
    for result in results:
        run = directory / result['name']
        summary = result['summary']
        events_file = run / 'events.jsonl'
        events = [json.loads(line) for line in events_file.read_text().splitlines()] if events_file.exists() else []
        sidecar = json.loads((run / 'scenario-results.json').read_text()) if (run / 'scenario-results.json').exists() else {}
        injected = set(sidecar.get('controller_message_ids', []))
        messages = [event['data'] for event in events if event['kind'] == 'message']
        authored = [message for message in messages if message['messageId'] not in injected]
        usage = list(summary.get('token_usage', {}).values())
        row = {key: result[key] for key in ('name', 'scenario', 'condition', 'repeat', 'exit_code', 'elapsed_seconds')}
        row.update(
            status=summary.get('status'),
            raw_oracle_passed=not replay_oracle(events, result['scenario']),
            audit_tool_errors=sum(event['kind'] == 'work_action' and bool(event['data']['result'].get('error')) for event in events),
            passed=result['exit_code'] == 0 and summary.get('status') == 'quiescent' and bool(summary.get('checks')) and all(summary.get('checks', {}).values()),
            checks=summary.get('checks', {}),
            agent_messages=len(authored), injected_messages=len(messages) - len(authored),
            body_labels=sum(bool(re.search(r'\b(?:no )?response required\b', message['body'], re.I)) for message in authored),
            structured_required=sum(message.get('responseRequired') is True for message in authored),
            structured_fields=sum('responseRequired' in message for message in authored),
            turns=summary.get('turns', 0), wake_turns=summary.get('wake_turns', 0),
            resource_violations=len(summary.get('resource_violations', [])),
            scope_attempts=len(sidecar.get('scope_violations', [])), repeated_work=len(sidecar.get('repeat_attempts', [])),
            failed_turns=len(summary.get('failed_turns', [])),
            total_tokens=summary.get('total_tokens', 0),
            uncached_input_tokens=sum(value.get('inputTokens', 0) - value.get('cachedInputTokens', 0) for value in usage),
            output_tokens=sum(value.get('outputTokens', 0) for value in usage),
            auth_cleaned=not (run / 'codex-home/auth.json').exists(),
        )
        trials.append(row)
        roles = {value: key for key, value in summary.get('roles', {}).items()}
        transcripts.append(f'\n=== {row["name"]} | {row["status"]} | passed={row["passed"]} ===\n')
        for event in events:
            if event['kind'] != 'message':
                continue
            message = event['data']
            origin = 'TEST INJECTION' if message['messageId'] in injected else 'AGENT'
            transcripts.append(f'+{event["seconds"]:.1f}s {origin} {roles.get(message["fromSessionId"], "unknown")} -> {roles.get(message.get("toSessionId"), "room")} responseRequired={message.get("responseRequired", "absent")}\n{message["body"]}\n')
    groups = []
    for scenario in ('independent', 'approval-wait', 'contention'):
        for condition in ('baseline', 'candidate'):
            rows = [row for row in trials if row['scenario'] == scenario and row['condition'] == condition]
            if not rows:
                continue
            group = {'scenario': scenario, 'condition': condition, 'trials': len(rows), 'passed': sum(row['passed'] for row in rows)}
            for metric in ('agent_messages', 'turns', 'wake_turns', 'total_tokens', 'uncached_input_tokens', 'output_tokens'):
                values = [row[metric] for row in rows]
                group[metric] = {'median': statistics.median(values), 'min': min(values), 'max': max(values), 'total': sum(values)}
            groups.append(group)
    report = {'planned_trials': len(manifest['pairs']) * 2, 'completed_trials': len(trials),
              'reported_tokens': sum(row['total_tokens'] for row in trials), 'trials': trials, 'groups': groups}
    (directory / 'assessment.json').write_text(json.dumps(report, indent=2))
    (directory / 'transcripts.txt').write_text('\n'.join(transcripts))
    with (directory / 'trials.csv').open('w') as stream:
        fields = [key for key in trials[0] if key != 'checks'] if trials else []
        writer = csv.DictWriter(stream, fieldnames=fields, extrasaction='ignore')
        writer.writeheader()
        writer.writerows(trials)
    print(json.dumps({key: report[key] for key in ('planned_trials', 'completed_trials', 'reported_tokens', 'groups')}, indent=2))
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    assess(parser.parse_args().directory)
