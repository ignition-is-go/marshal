#!/usr/bin/env python3
import ctypes
import json
import os
from pathlib import Path
import shutil


def memory_bytes() -> int | None:
    if os.name == "nt":
        class MemoryStatus(ctypes.Structure):
            _fields_ = [("length", ctypes.c_ulong), ("load", ctypes.c_ulong)] + [
                (name, ctypes.c_ulonglong)
                for name in ("total", "available", "page", "page_available", "virtual", "virtual_available", "extended")
            ]
        status = MemoryStatus()
        status.length = ctypes.sizeof(status)
        return status.total if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)) else None
    return os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")


def collect() -> dict[str, int | None]:
    resources = {"logical_cpus": os.cpu_count(), "memory_bytes": None, "disk_free_bytes": None}
    try:
        resources["memory_bytes"] = memory_bytes()
    except (AttributeError, OSError, ValueError):
        pass
    try:
        resources["disk_free_bytes"] = shutil.disk_usage(Path.cwd()).free
    except OSError:
        pass
    return resources


def main() -> None:
    report = json.dumps(collect(), sort_keys=True)
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        try:
            with open(summary, "a", encoding="utf-8") as output:
                output.write(f"### Runner resources\n\n```json\n{report}\n```\n")
        except OSError:
            pass


if __name__ == "__main__":
    main()
