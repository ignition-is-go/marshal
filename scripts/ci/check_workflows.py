#!/usr/bin/env python3
import argparse
from pathlib import Path
import re
import shutil
import subprocess
import sys

import yaml


ROOT = Path(__file__).resolve().parents[2]


def unpinned_actions(path: Path) -> list[str]:
    failures = []
    document = yaml.safe_load(path.read_text())

    def action_refs(value):
        jobs = value.get("jobs", {}) if isinstance(value, dict) else {}
        if not isinstance(jobs, dict):
            return
        for job in jobs.values():
            if not isinstance(job, dict):
                continue
            if "uses" in job:
                yield job["uses"]
            steps = job.get("steps", [])
            if isinstance(steps, list):
                for step in steps:
                    if isinstance(step, dict) and "uses" in step:
                        yield step["uses"]

    for action in action_refs(document):
        if not isinstance(action, str):
            failures.append(f"{path}: action reference must be a string")
            continue
        if action.startswith("./"):
            continue
        immutable = (
            re.fullmatch(r"[^@\s]+@[0-9a-f]{40}", action)
            or re.fullmatch(r"docker://[^\s]+@sha256:[0-9a-f]{64}", action)
        )
        if not immutable:
            failures.append(f"{path}: action must use an immutable commit or digest: {action}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description="Check action pins and validate workflows with actionlint.")
    parser.add_argument("--actionlint", default="actionlint")
    parser.add_argument("workflows", nargs="*", type=Path)
    args = parser.parse_args()
    paths = [path.resolve() for path in args.workflows] or sorted((ROOT / ".github/workflows").glob("*.y*ml"))
    try:
        failures = [failure for path in paths for failure in unpinned_actions(path)]
    except (OSError, yaml.YAMLError) as error:
        print(error, file=sys.stderr)
        return 1
    for failure in failures:
        print(failure, file=sys.stderr)
    executable = shutil.which(args.actionlint)
    if executable is None:
        print("actionlint is required; install it or pass --actionlint PATH", file=sys.stderr)
        return 1
    result = subprocess.run(
        [executable, "-color=false", *map(str, paths)],
        cwd=ROOT,
        check=False,
    )
    return 1 if failures or result.returncode else 0


if __name__ == "__main__":
    raise SystemExit(main())
