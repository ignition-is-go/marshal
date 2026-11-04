#!/usr/bin/env python3
"""Run a fixed, paired baseline/candidate campaign without outcome-based retries."""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import sys
import time


async def campaign(args):
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    runner = args.output / 'runner'
    runner.mkdir()
    for source in Path(__file__).parent.glob('*.py'):
        shutil.copy2(source, runner / source.name)
    pairs = [(scenario, repeat) for scenario in ('independent', 'approval-wait', 'contention') for repeat in range(1, args.repeats + 1)]
    random.Random(20261104).shuffle(pairs)
    manifest = {
        'model': args.model, 'baseline_commit': args.baseline_commit, 'candidate_commit': args.candidate_commit,
        'repeats_per_condition_per_scenario': args.repeats, 'pairs': pairs,
        'concurrent_trials': 2, 'trial_limits': {'seconds': 420, 'turns': 20, 'messages': 24, 'tokens': 650000, 'quiet_seconds': 35},
        'campaign_token_budget': args.max_tokens,
        'source_hashes': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in runner.glob('*.py')},
        'representation': 'Baseline injected requests use body labels; candidate uses response_required. Semantic requests and scenario tools are otherwise identical.',
    }
    (args.output / 'campaign.json').write_text(json.dumps(manifest, indent=2))
    results = []

    async def trial(scenario, repeat, condition):
        name = f'{scenario}-{condition}-{repeat:02}'
        output = args.output / name
        binaries = args.baseline_bin if condition == 'baseline' else args.candidate_bin
        env = dict(os.environ, MARSHAL_EVAL_CONDITION=condition, MARSHAL_EVAL_SCENARIO=scenario)
        command = [sys.executable, str(runner / 'behavior.py'), '--output', str(output), '--label', name,
                   '--model', args.model, '--shim', str(binaries / 'marshal-shim'), '--daemon', str(binaries / 'marshal-daemon'),
                   '--seconds', '420', '--max-turns', '20', '--max-messages', '24', '--max-tokens', '650000']
        started = time.monotonic()
        with (args.output / f'{name}.log').open('wb') as log:
            process = await asyncio.create_subprocess_exec(*command, env=env, stdout=log, stderr=log)
            try:
                code = await asyncio.wait_for(process.wait(), 480)
            except asyncio.TimeoutError:
                process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), 20)
                except asyncio.TimeoutError:
                    process.kill()
                    await process.wait()
                code = 124
        summary = json.loads((output / 'summary.json').read_text()) if (output / 'summary.json').exists() else {}
        return {'name': name, 'scenario': scenario, 'condition': condition, 'repeat': repeat,
                'exit_code': code, 'elapsed_seconds': round(time.monotonic() - started, 2),
                'summary': summary}

    for scenario, repeat in pairs:
        spent = sum(row['summary'].get('total_tokens', 0) for row in results)
        if spent >= args.max_tokens:
            print(f'Campaign budget reached at {spent} reported tokens; remaining pairs not run.', flush=True)
            break
        print(f'Starting paired {scenario} repeat {repeat}', flush=True)
        pair = await asyncio.gather(*(trial(scenario, repeat, condition) for condition in ('baseline', 'candidate')))
        results.extend(pair)
        (args.output / 'results.json').write_text(json.dumps(results, indent=2))
        for row in pair:
            s = row['summary']
            print(f'{row["name"]}: exit={row["exit_code"]} status={s.get("status")} turns={s.get("turns")} messages={s.get("messages")} checks={s.get("checks")}', flush=True)
    print(args.output / 'results.json', flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--baseline-bin', type=Path, required=True)
    parser.add_argument('--candidate-bin', type=Path, required=True)
    parser.add_argument('--baseline-commit', required=True)
    parser.add_argument('--candidate-commit', required=True)
    parser.add_argument('--model', required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--max-tokens', type=int, default=8_000_000)
    asyncio.run(campaign(parser.parse_args()))
