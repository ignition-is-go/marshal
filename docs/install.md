# Installing marshal

## Build

```
cargo install --path crates/server
cargo install --path crates/stdio
```

This places `marshal-server` and `marshal-stdio` in `~/.cargo/bin`. Make sure that's on your `PATH`.

## Configure Claude Code

Add an MCP entry — one entry per Claude Code config; every session uses the same one:

```json
{
  "mcpServers": {
    "marshal": {
      "command": "marshal-stdio"
    }
  }
}
```

## Daemon lifecycle

The shim auto-starts the daemon on first connect via a detached background process. No systemd needed.

To run the daemon manually for debugging (it stays in the foreground; `marshal-server --help` lists the environment it reads):

```
marshal-server
```

## State

- Socket: `$XDG_RUNTIME_DIR/marshal/sock` (fallback `~/.local/state/marshal/sock`)
- DB:     `~/.local/state/marshal/db.sqlite`
- Logs:   `~/.local/state/marshal/daemon.log` (daily rotation)

## Coordinating with peers

Each Claude session that connects via `marshal-stdio` shows up on the
roster with a unique nickname (the shim's cwd basename, with `-N`
appended on collision). Sessions coordinate by sending free-form
messages to each other:

- `roster` — list every live session with its nickname, cwd, branch,
  and current status text.
- `send_message` — deliver a message to another session by nickname or
  id. Inbound messages surface as `notifications/claude/channel`
  events, so the recipient sees them mid-conversation without polling.
- `set_status` — publish a free-form line about what this session is
  currently doing; visible to all peers via `roster`.
- `whoami` — your own session id / nickname / cwd.

That's the whole surface. Asking a peer a question, splitting work,
flagging a blocker — all of it goes through `send_message`.


## Register an agent host

Set `MARSHAL_HOST_CREDENTIALS_FILE` on the server to the path of a JSON object
mapping host IDs to distinct bearer tokens. Use at least 32 random printable
ASCII characters per token. The host listener binds to `127.0.0.1:6157` by
default. For remote hosts, expose its `/host` WebSocket route through a TLS
reverse proxy.

Run `marshal-host` with `MARSHAL_HOST_URL`, `MARSHAL_HOST_ID`, and
`MARSHAL_HOST_TOKEN_FILE`. The URL ends in `/host` and the token file contains
the token for that one host. A host advertises no launch capability by
default. Pass `--launch-harness codex` or `--launch-harness claude` to
enable it explicitly. Launch-enabled hosts require tmux and a
`MARSHAL_HOST_WORKSPACE_ROOT` directory. A launch workspace must exist within
that directory.

Install both binaries with `cargo install --path crates/host`.
`marshal configure <server-ws-url> <host-ws-url>` saves both endpoints.
With the host ID and token-file variables set, `marshal service install`
installs and starts the host under your account using systemd on Linux,
launchd on macOS, or Task Scheduler on Windows. macOS and Windows start the
host when you log in. Append launch flags on a host with tmux installed.
`marshal service status` shows the service state.

An SSH-reachable machine can run the same service under the account that owns
its checkouts and harness credentials. When the machine cannot reach Marshal
directly, keep an SSH reverse tunnel open from a machine that can reach it.
For example, run this on the Marshal server when both listeners are local.

```sh
ssh -N -o ExitOnForwardFailure=yes \
  -R 127.0.0.1:6155:127.0.0.1:6155 \
  -R 127.0.0.1:6157:127.0.0.1:6157 operator@agent-host
```

On the SSH machine, configure `ws://127.0.0.1:6155` and
`ws://127.0.0.1:6157/host`, then install the host service with the intended
harness flags. The host reconnects when the tunnel returns. Codex launches use
`marshal-stdio codex-run` for idle delivery. Install and configure
`marshal-stdio` for each launch-enabled harness before enabling dispatch.

Use `marshal agents` to list sessions. Send to one or more agents from your
existing session with `marshal send --as <session-id> <recipient>... -- "message"`.

Harness hooks use the configured Myko server. Regenerate older Codex hook
configuration with `marshal-stdio codex-setup` during the upgrade.

## Work with server tasks

Use `marshal task list [--project <name>]` or `marshal task show <id>` to
inspect server tasks. Create tasks with `marshal task create "Task title"`. The command groups
checkouts by their Git origin. Use `--project <name>` to choose a project
explicitly. Include `--body`, `--priority`, and repeatable `--label` options
when recreating an existing task. Use `marshal task dep add <id> <blocker-id>`
to link tasks after both exist. `marshal task close <id> [commit]` records completion with optional
commit evidence, and `marshal task reopen <id>` reopens the task.

Press `t` in `marshal-tui` to open the task board and use `[` and `]` to
filter by project. Press `d` to select a task, host, and harness, then enter
the workspace and press Enter to dispatch. When server authentication is enabled, set
`MARSHAL_OPERATOR_TOKEN_FILE` to a file containing a valid operator bearer
token for the CLI and TUI.

Agents can use `find_coordination` to look up an open task by ID, title, or
label and get the claimant's session ID. Include the project to find nearby
idle peers. The discovery response contains bounded task summaries and
session addresses, and is available to agents on the trusted Marshal network.

An authenticated operator can use `marshal hosts` to see connected hosts and
the harnesses each host can launch. Request an agent launch with
`marshal task dispatch <id> <host> <claude|codex> <workspace>`. The workspace
is a path on the selected host. Repeating the same request returns the same
reservation and its current state. If a start reply is lost, the server
inspects the existing launch ID when the host reconnects. The task board shows
launch progress separately from a worker's claim.
Install the matching Claude or Codex session-start hooks on launch-enabled
hosts. A launch only moves to Joined when its host reports a session that also
registered the matching launch ID. Offered means the assignment is in that
session's inbox. Delivered means that session acknowledged the inbox message.
The launched worker uses `marshal task accept <id>` to record acceptance on
its host. Marshal creates the claim when the authenticated host reports the
matching task and session.

An authenticated operator can list active leases with `marshal task claims`.
Use its task and lease IDs with `marshal task renew <id> <lease-id>` to extend
the claim or `marshal task release <id> <lease-id>` to return the task to the
ready queue when its dependencies permit.
