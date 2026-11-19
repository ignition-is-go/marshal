# Launch a worker through Marshal

`marshal-launch` starts a harness, waits for its Marshal session, and can hand
that session an initial task. Any client can use the same launch API. The
Marshal daemon owns the launch record. The selected launch backend owns the
worker process. Marshal messages address the registered session.

```text
marshal-launch tmux codex --cwd ~/work/app --task "Run the failing test"
marshal-launch ssh claude --cwd /home/agent/app --task "Run the failing test" -- --host build-03
marshal-launch get launch-<uuid>
marshal-launch list
marshal-launch stop launch-<uuid>
```

The tmux backend runs on the daemon host. SSH starts a detached tmux session
on an existing host. Install the same version of `marshal-launch-ssh` on the
daemon's PATH and on each target's login PATH. Set up the repo, harness login, and Marshal wiring on
the target first. SSH accepts `--oauth-token-file` after `--` when Claude needs
to read a login token from a file on the target. Use `--daemon http://host:6156` or `MARSHAL_LAUNCH_API` to
point the CLI at the launch API. A WebSocket URL selects the hook port of the
same host. Configure `MARSHAL_LAUNCH_ADDRESS` on the daemon to advertise the
WebSocket URL workers can reach and `MARSHAL_BASE_URL` for a custom hook URL.

On macOS, Local Network privacy can prevent processes launched by an SSH
tmux server from joining. Use a tmux server started from the desktop session
if the host requires it.

## Service API

POST JSON to the daemon's existing HTTP listener. The service uses the
listener's existing trust model.

| Path | Request | Response |
| --- | --- | --- |
| `/launch/v1/start` | Launch ID, backend, typed intent, display name, preflight choice, join timeout, optional first task | Launch record |
| `/launch/v1/get` | `{"launch_id":"launch-…"}` | Launch record with backend status and any observed task acknowledgment |
| `/launch/v1/list` | `{}` | Launch records with their last recorded process state |
| `/launch/v1/stop` | `{"launch_id":"launch-…"}` | Launch record |

```json
{
  "launch_id": "launch-068e59ab16294023a4da6dd283b3deaa",
  "backend": "ssh",
  "name": "test-fix",
  "intent": {
    "kind": "process", "harness": "claude", "cwd": "/home/agent/app",
    "options": ["--host", "build-03"]
  },
  "check": true,
  "timeout_secs": 60,
  "first_task": "Run the failing test"
}
```

The caller generates a fresh `launch-` ID followed by 32 lowercase UUIDv4 hex digits
before submitting. The CLI prints the ID before sending its request. Repeat
the same request with the same ID to recover an interrupted launch. The CLI's
`--id` option sets the ID for that retry. A different request with the same
ID is rejected.

Before invoking the backend, the daemon stores the immutable request and its
recovery handle. It makes at most one start attempt per ID. When a start
response is lost, the backend can inspect that handle and recover metadata for
a still-running worker. A launch that is absent or whose state is unknown
retains its record. To start a replacement, use a fresh launch ID after
resolving the original attempt. Stop is idempotent. An uncertain stop can be
retried by the same ID. If the daemon disconnects during stop, retry
the stop request after reconnecting.

Records live in the daemon's `MARSHAL_LAUNCH_STATE` directory. The defaults are
`$MARSHAL_STATE_DIR/launches`, `$XDG_STATE_HOME/marshal/launches`, then
`~/.local/state/marshal/launches`. Use
a persistent directory where launches should survive daemon restart. One
daemon owns each launch state directory.

Process state, roster registration, and task state are separate fields in
the response. A task can be pending, delivery uncertain, stored, or
acknowledged. Acknowledged means the session's inbox reported delivery.
A launch correlation ID is not a credential.
Marshal records the first task's stable message ID before sending it to
the exact joined session. If a restart interrupts that send, Marshal
recovers the task state from any surviving message or inbox receipt.
Without a receipt, the task remains `delivery_uncertain` and Marshal
skips redispatch on retry. The session may already have received it.

## Launch backends

The Rust trait is `LaunchBackend`. Built-in tmux implements it directly.
Marshal runs external backends as `marshal-launch-<name> <verb>` on the
daemon's PATH. The subprocess receives one JSON object on stdin and returns
one on stdout. Diagnostics go to stderr. Each stream is capped at 1 MiB.
The following is wire version 3.

- `check` examines the target without starting a worker.
- `recover` derives the stable backend handle for a request before startup.
- `launch` starts the worker and returns the same handle.
- `inspect` optionally observes a live worker and returns `{"version":3,"launched":{…}}`
  or `{"version":3,"launched":null}`.
- `status` and `stop` take the handle. Status can report running, absent, or
  unknown in the `running` field as `true`, `false`, or `null`. A successful
  stop returns `{"version":3,"state":"stopped"}` or `{"version":3,"state":"gone"}`.

**`launch`** — request:

```json
{
  "version": 3,
  "name": "claude-app",
  "launch_id": "launch-6f1c",
  "marshal_version": "0.60.5",
  "context": {
    "daemon_address": "ws://marshal.mesh:6155",
    "hook_base": null
  },
  "intent": {
    "kind": "process",
    "harness": "claude",
    "argv": ["claude"],
    "cwd": "~/work/app",
    "env": {"MARSHAL_AGENT_ID":"launch-6f1c"},
    "options": ["--host", "build-03"]
  }
}
```

Response:

```json
{
  "version": 3,
  "handle": "build-03 marshal-launch-6f1c",
  "host": "build-03",
  "cwd": "/home/agent/work/app",
  "where": "build-03 tmux:claude-app",
  "attach": "ssh -t build-03 'tmux attach -t =marshal-launch-6f1c'"
}
```

Host and working directory describe the worker as observed by the backend.
Marshal compares them with the registering worker's report. The comparison
helps correlate the reports. It is not authentication.

The SSH preflight examines config on the target and returns compatibility
facts. The daemon service account executes the backend and supplies its SSH
configuration. Provision the backend and SSH access for that account. Build
the binaries from the same checkout with
`cargo build --locked -p marshal-daemon -p marshal-launch -p marshal-launch-ssh`.
SSH and tmux use a launch-specific session name and an ownership
marker. The display name does not select the process to stop.

Both session backends use `libtmux` directly through Marshal's native adapter.
SSH sends contract-v2 JSON to `marshal-launch-ssh --target`; it never falls
back to shell commands that manage tmux. A missing or incompatible helper
fails before launch. Set `MARSHAL_LAUNCH_SSH_HELPER` in the daemon service
environment to an absolute target path when the helper is outside the target's
login PATH. Keep that setting available for later inspect and stop calls.
The optional Claude token file is read inside the target worker process;
the credential value stays out of requests, responses, and tmux arguments.

Pulse Agents can implement an external backend using its own launch API.
That backend will pass through the provider's lifecycle handle, and Marshal
will separately record the worker's Marshal session.

## Managed launch intent

The launch API accepts a typed intent. Process intent selects `harness`, `cwd`,
and backend `options`; Marshal resolves the executable and coordination wiring.
Managed intent selects a provider-owned launch policy. The Pulse selection is:

```json
{
  "kind": "managed",
  "provider": "pulse",
  "environment_id": "development",
  "preset_id": "codex-account",
  "workspace": {"provider": "session"}
}
```

The same managed selector reaches the external backend inside `intent`.
Marshal supplies the common launch id, label, shim version, and trusted
`context` containing the daemon address and optional hook base. The selector
accepts no executable, argv, environment map, endpoint, credential, or first
turn. Pulse owns harness selection, model/auth settings, permissions, and resume
policy. Its workspace selector follows Pulse's typed `WorkspaceRequest`,
including `pulseRoot`, `existingDirectory`, and `pulse` with a named or Levi-task
identity. Pulse validates those selections and owns workspace mechanics.

`first_task` remains an optional field of the outer Marshal API request. It is
persisted by Marshal and never sent to a backend. After the backend returns its
resolved host and working directory, Marshal requires one roster row matching
that location and the launch id before sending the task through the existing
message path. Task storage and acknowledgment retain their separate receipts.
An uncertain startup retains its pre-start handle and never repeats launch.

The contract and dispatch are available for external backend implementations.
A Pulse backend and Pulse's durable admission endpoint are separate work; this
change does not provision or call a Pulse service. Built-in tmux and the SSH
backend accept only process intent. Wire version 3 replaces the unreleased
version 2 shape; there is no compatibility translation.
