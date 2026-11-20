import json
import os
from pathlib import Path
import shutil
import subprocess


def clean_failed_restore(workspace: Path) -> None:
    root = workspace.resolve(strict=True)
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout)
    owned = root / "target"
    targets = set()
    for value in [metadata["target_directory"], metadata.get("build_directory") or metadata["target_directory"]]:
        target = Path(value)
        resolved = target.resolve()
        if not target.is_absolute() or target.is_symlink() or not resolved.is_relative_to(owned):
            raise ValueError("Refusing to clean a Cargo target outside this checkout's target directory")
        targets.add(resolved)
    for target in sorted(targets, key=lambda path: len(path.parts)):
        if target.is_dir():
            shutil.rmtree(target)
        else:
            target.unlink(missing_ok=True)
        print(f"Discarded failed compiler snapshot target: {target}")


if __name__ == "__main__":
    clean_failed_restore(Path(os.environ["GITHUB_WORKSPACE"]))
