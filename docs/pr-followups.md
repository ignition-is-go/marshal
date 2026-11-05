# Preserved PR follow-ups

The closed PRs contain useful ideas that remain separate from their rejected or obsolete rollout details.

| Original PR | Preserved work | Current boundary |
| --- | --- | --- |
| [#91](https://github.com/ignition-is-go/marshal/pull/91) | Manual Windows pool acceptance recipe and offline checks | No current workflow targets the old managed pool. GitHub-hosted Windows is unchanged. |
| [#104](https://github.com/ignition-is-go/marshal/pull/104) | [Embedding agent metadata](embedding-agent-id.md) | Self-reported correlation data; no authenticated identity or Pulse Agents binding replacement. |
| [#113](https://github.com/ignition-is-go/marshal/pull/113) | Immutable action references, workflow validation, and resource reports | Existing runner selection, release permissions, and target caches remain. |
| [#125](https://github.com/ignition-is-go/marshal/pull/125) | Executable compiler-cache qualification and acceptance requirements | Production compiler caching remains disabled. |

## Validate workflows locally

Install `scripts/ci/requirements.txt` in a Python environment, then run `python3 scripts/ci/check_workflows.py` with actionlint on your path, or pass `--actionlint PATH`.
The helper requires actionlint and rejects moving external action references after parsing YAML, including quoted keys, flow mappings, and aliases.
It retains actionlint's default ShellCheck and pyflakes integration when those tools are available.

Linux x86-64 CI installs actionlint 1.7.12 with `scripts/ci/install-actionlint.sh DESTINATION`.
The installer verifies the upstream release archive's SHA-256, `8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8`.
Other platforms can install [actionlint](https://github.com/rhysd/actionlint/releases/tag/v1.7.12) directly.
Run the helper tests with `ACTIONLINT=/path/to/actionlint python3 -m unittest discover -s scripts/ci -p 'test_*.py' -v`.

CI prints CPU, physical-memory, and free-disk facts before and after builds where checkout is available.
Unavailable measurements are `null`. Reporting steps are bounded and cannot fail the build. They never dump environment variables.

Linux CI installs Bun 1.3.11 and runs all three native plugins' `embedding-agent.test.ts` files against the built daemon and shim after the regular Cargo tests.
It resolves the binary directory from Cargo metadata and sets `MARSHAL_DAEMON_BIN`, so those registration and reconnect tests run instead of taking their opt-in skip path.

## Exercise a future managed Windows pool

Run `scripts/ci/windows-isolation.ps1` inside the intended Windows guest with its actual runner root and expected limits.
For example, `pwsh -NoProfile -File scripts/ci/windows-isolation.ps1 -RunnerRoot C:\actions-runner -MaxCpu 16 -MaxMemoryBytes 47244640256 -ExpectedDiskCount 1` checks local resources, forbidden paths, named environment variables, and a marker beneath `_work`.
Adapt `-ForbiddenPath` and `-ForbiddenEnvironment` to the pool's provisioning contract.

Network checks require both `-RunNetworkChecks` and explicit `-BlockedEndpoint host:port` values supplied by the pool operator.
No infrastructure endpoint is embedded in the script. Successful connections fail the check; connection refusal, timeout, and unreachable-network responses count as denial. Unexpected runtime errors fail the recipe.

The marker is deliberately retained for the next job to detect stale state. A single run cannot prove teardown, and finite path and environment-name lists cannot prove the absence of every credential or host resource.
The recipe is not scheduled in current CI. Run `pwsh -NoProfile -File scripts/ci/test_windows_isolation.ps1` for offline fixtures and loopback-only checks. Those tests run in the existing native Windows job.

## Qualify a preinstalled compiler cache

Run `python3 scripts/ci/qualify_compiler_cache.py --cache /path/to/sccache --output-dir /new/probe-directory`.
The probe downloads nothing and configures no credentials. It uses the cache's existing operator-supplied environment.
It compiles a fixed fixture directly and through the cache into different output directories, then compares their program output.
Each tool invocation has a deadline, configurable with `--timeout SECONDS`. Direct compiler failures keep their exit status.

The JSON report always says `production_qualified: false`. A successful invocation proves only that fixture's cached compile and output comparison.
Missing tools, startup errors, compile errors, and timeouts remain explicit failed outcomes. The probe retains its output directory on every outcome.
A cache client's timeout does not prove a detached daemon or its compiler has stopped. The probe never falls back into, deletes, or reuses that cached output path. Stop any remaining processes before manually removing its artifacts.

An enabled compiler-cache integration must first prove that cache retrieval and runtime failures cannot block direct compilation, that no cached compiler can still write fallback outputs, that real compiler failures remain failures, and that cache credentials and namespaces respect trust boundaries.
No build workflow sets a new `RUSTC_WRAPPER`, downloads a compiler-cache action or executable, or adds shared-cache secrets. Existing Cargo and npm caches retain their behavior.
