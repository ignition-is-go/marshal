#!/usr/bin/env python3
"""Compare recorded sandbox runs without starting models or modifying results."""
import argparse
import json
from pathlib import Path


def compare(paths):
    runs = []
    for path in paths:
        manifest = json.loads((path / 'manifest.json').read_text())
        summary = json.loads((path / 'summary.json').read_text())
        usage = summary.get('token_usage', {})
        runs.append({
            'label': manifest['label'], 'model': manifest['model'],
            'status': summary['status'], 'tested': summary['simulated_test_completed'],
            'released': summary['node_owner'] is None,
            'violations': len(summary['resource_violations']),
            'messages': summary['messages'], 'turns': summary['turns'],
            'wake_turns': summary.get('wake_turns'),
            'input_tokens': sum(value['inputTokens'] for value in usage.values()),
            'cached_input_tokens': sum(value['cachedInputTokens'] for value in usage.values()),
            'output_tokens': sum(value['outputTokens'] for value in usage.values()),
            'run': str(path.resolve()),
        })
    return runs


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('runs', type=Path, nargs='+')
    args = parser.parse_args()
    print(json.dumps(compare(args.runs), indent=2))
