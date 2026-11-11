#!/usr/bin/env python3
import argparse
import json
import math
import os
from pathlib import Path
import shutil
import subprocess


def attempt(command: list[str], directory: Path, timeout: float) -> dict:
    try:
        with (directory / "stdout.log").open("wb") as stdout, (directory / "stderr.log").open("wb") as stderr:
            result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=timeout, check=False)
        return {"outcome": "ok" if result.returncode == 0 else "failed", "exit_code": result.returncode}
    except subprocess.TimeoutExpired:
        return {"outcome": "timeout", "exit_code": None}
    except OSError as error:
        (directory / "stderr.log").write_text(str(error))
        return {"outcome": "unavailable", "exit_code": None}


def qualify(cache: str, rustc: str, output: Path, timeout: float) -> tuple[dict, int]:
    output.mkdir(parents=True, exist_ok=False)
    for name in ("direct", "startup", "cached", "direct-run", "cached-run"):
        (output / name).mkdir()
    source = output / "probe.rs"
    source.write_text('fn main() { println!("marshal-cache-probe"); }\n')
    suffix = ".exe" if os.name == "nt" else ""
    direct = output / "direct" / f"probe{suffix}"
    cached = output / "cached" / f"probe{suffix}"
    report = {
        "production_qualified": False,
        "output_directory": str(output),
        "limitations": [
            "A successful invocation does not prove failure recovery or trust separation.",
            "A timed-out cache client may leave a daemon-owned compiler running; outputs are retained.",
        ],
    }
    report["direct"] = attempt([rustc, str(source), "-o", str(direct)], output / "direct", timeout)
    if report["direct"]["outcome"] != "ok":
        return report, report["direct"]["exit_code"] or 1
    report["startup"] = attempt([cache, "--start-server"], output / "startup", timeout)
    if report["startup"]["outcome"] != "ok":
        return report, 1
    report["cached"] = attempt([cache, rustc, str(source), "-o", str(cached)], output / "cached", timeout)
    if report["cached"]["outcome"] != "ok":
        return report, 1
    report["direct_run"] = attempt([str(direct)], output / "direct-run", timeout)
    report["cached_run"] = attempt([str(cached)], output / "cached-run", timeout)
    report["output_matches"] = (
        report["direct_run"]["outcome"] == report["cached_run"]["outcome"] == "ok"
        and (output / "direct-run/stdout.log").read_bytes() == (output / "cached-run/stdout.log").read_bytes()
    )
    return report, 0 if report["output_matches"] else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="Probe a preinstalled compiler cache in disposable output paths.")
    parser.add_argument("--cache", required=True, help="Preinstalled sccache executable; never downloaded.")
    parser.add_argument("--rustc", default="rustc")
    parser.add_argument("--output-dir", required=True, type=Path, help="New directory; retained even after failure.")
    parser.add_argument("--timeout", default=30.0, type=float)
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    if args.output_dir.exists():
        parser.error("--output-dir must not already exist")
    cache = shutil.which(args.cache) or args.cache
    rustc = shutil.which(args.rustc) or args.rustc
    report, status = qualify(cache, rustc, args.output_dir.resolve(), args.timeout)
    encoded = json.dumps(report, indent=2) + "\n"
    (args.output_dir / "report.json").write_text(encoded)
    print(encoded, end="")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
