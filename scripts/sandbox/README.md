# Marshal behavior sandbox

Run real Codex agents against private Marshal daemon, shim, hooks, and wake
bridge processes. Observe whether experimental changes preserve coordination
and allow exchanges to stop. Node operations are simulated; no Unreal instance
or render node is used.

The initial scenario has two agents. One owns node-7 after finishing a test;
the other needs it for a new test. Agents choose how to negotiate access, use
the node, release it, and finish communicating. The controller supplies each
agent's task once. Subsequent turns use the actual Marshal delivery and wake
paths, not scripted agent replies.

## Run an experiment

Build the version you want to evaluate:

```sh
cargo build -p marshal-daemon -p marshal-shim
```

Run from this directory or the repository root. Use the actual binary paths
printed by Cargo for your workspace:

```sh
uv run scripts/sandbox/run.py \
  --label candidate \
  --model gpt-6.1-sol \
  --shim /absolute/path/to/target/debug/marshal-shim \
  --daemon /absolute/path/to/target/debug/marshal-daemon \
  --output /tmp/marshal-candidate-01
```

Choose a model available to your Codex login. The model must be explicit and is
recorded with the run. The runner uses file-based authentication from
`$CODEX_HOME/auth.json`, or `~/.codex/auth.json`; select another login location
with `--auth-home`. It temporarily links that file into the private Codex home
and removes the link during cleanup. It does not copy credentials into the
report or replace your installed Codex configuration.

The runner currently supports Linux/macOS. Python 3.11+ and Codex app-server
with dynamic tools are required. `uv` installs the script's pinned WebSocket
dependency. An environment that already provides `websockets==10.4` can use
`python3` instead. These experiments consume model usage and run outside CI.

The output directory must be new and outside project checkouts and inherited
project instructions. Each run gets loopback ports, ephemeral Marshal state,
a private Codex home, and a private XDG configuration directory. The explicit
MCP child environment also points to that directory because Codex filters
inherited environment. A preflight checks that the observer can see both private
sessions before any model turns start. Optional Codex apps, web search, shell
execution, and multi-agent delegation are disabled. The simulated node is an
app-server dynamic tool.

This isolates the experiment's configuration and effects. It is not a hostile
code isolation boundary. Account-level policies and Codex's built-in instructions
still apply. Inspect captured context before drawing conclusions about a run.

## Observe and compare

Open `report.html` while the run is active. It refreshes every five seconds and
shows the timeline, messages, resource actions, turn counts, token usage, and
failures. `events.jsonl` retains raw app-server events, including streamed model
output, hook events, and tool calls. `summary.json` records measured outcomes.

`manifest.json` records the selected model, Codex version, binary hashes, local
endpoints, and budgets. `prompts.json` and `codex-home/AGENTS.md` preserve the
scenario tasks and generated instructions. The private config and process logs
are also retained. Treat the run directory as private; transcripts can include
account metadata and experimental prompts.

For a baseline and candidate comparison, build each revision in its own
checkout and pass its daemon and shim binaries to separate runs. Keep the model,
scenario, and budgets the same. Repeat each variant; one run demonstrates a
trace, not a reliable behavioral improvement.

```sh
python3 scripts/sandbox/compare.py \
  /tmp/marshal-baseline-01 /tmp/marshal-candidate-01
```

The comparison reports task completion and resource conflicts alongside message,
turn, and token counts. Fewer messages are useful only if the shared-resource
handoff still works. Cached input is reported separately from total input;
these counts are not a price estimate. Model reasoning is limited to what the
harness emits; the runner does not expose hidden reasoning.

The default limits are 240 seconds, 12 turns, 20 messages, and 500,000 cumulative
reported input/output tokens. Budget checks run every half second; token limits
are checked after usage reports and are not a hard provider-side token cap.
The runner stops after at least 35 seconds without activity and no active turn,
covering the bridge's 30-second wake coalescing window. That observation window
is not proof that an exchange can never restart.

Exit status is zero only if the run becomes quiescent, completes the simulated
test, releases the node, and records no resource violation. A quiet run that
fails to coordinate is a failed scenario. Runtime errors, model failures, and
budget exhaustion remain visible rather than being scored as good silence.
SIGINT/SIGTERM or a normal exit stop owned process groups and remove the auth
link. A forced SIGKILL cannot run cleanup.

## Check the runner without model calls

```sh
MARSHAL_TEST_SHIM=/absolute/path/to/target/debug/marshal-shim \
  python3 -m unittest discover -s scripts/sandbox -v
```

These checks cover routing and hook-trust compatibility with the actual built
shim, simulated resource conflict detection, and process/auth-link cleanup.
They do not evaluate model behavior.
