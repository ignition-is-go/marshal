# Codex live delivery

Codex command hooks can add context to a turn, but they cannot create a turn.
That is why a direct Marshal message sent to an idle, normally launched Codex
session remains unread until the user submits another prompt.

Codex app-server provides the missing control plane. Multiple clients can
attach to one app-server, and any client can issue `turn/start` for an idle
thread. The original TUI receives that turn's normal streamed events.

## Delivery path

```text
sender ──send_message──> marshal daemon ──durable unread Message
                                      │
                                      ▼
                              local Codex bridge
                                      │ turn/start(thread_id)
                                      ▼
                               shared app-server ──> attached TUI
                                      │
                                      ▼
                         UserPromptSubmit hook fetches,
                         injects, then acknowledges inbox
```

The Marshal session id supplied to hooks is the Codex app-server thread id.
That lets the bridge target the thread without guessing from cwd, process age,
or rollout files.

The bridge deliberately does not mark the message read. The existing hook
acknowledges it only after the daemon successfully writes the
`<marshal_inbox>` response back to Codex. If the app-server is unavailable, the
thread is busy, or `turn/start` fails, the durable message stays unread and the
ordinary prompt/tool-boundary path remains intact.

Only unread direct messages are wake candidates. Room broadcasts are ambient,
and the hook continues to frame peer content as untrusted coordination input:
a peer cannot expand the operator's task or authority merely by waking a turn.

## Running it

First install the normal Codex integration:

```sh
marshal-shim codex-setup --daemon ws://marshal-host:6155
```

Then use the live launcher for an interactive session:

```sh
marshal-shim codex-run
marshal-shim codex-run resume --last
```

`codex-run` performs three actions:

1. idempotently starts `codex app-server daemon`;
2. starts `marshal-shim codex-bridge`;
3. runs `codex --remote unix:// ...`.

The bridge exits with that TUI. More than one live launcher may run on a host;
duplicate bridge observations are safe because app-server accepts only one
active turn for a thread.

For diagnostics or a supervisor-managed deployment, run the bridge directly:

```sh
marshal-shim codex-bridge \
  --daemon ws://marshal-host:6155 \
  --socket "$CODEX_HOME/app-server-control/app-server-control.sock"
```

## Making it the fleet default

Codex currently exposes the shared endpoint as a CLI flag, not a
`config.toml` setting. Make the behavior default at the agent-launcher layer:

- interactive `codex` and `codex resume` jobs invoke
  `marshal-shim codex-run ...`;
- non-interactive and administrative subcommands (`codex exec`, `codex mcp`,
  `codex update`, and similar) continue to invoke the real Codex executable;
- the Ansible role runs `marshal-shim codex-setup` so SessionStart,
  SessionEnd, prompt, and tool hooks stay installed and trusted.

Do not overwrite Codex's own managed executable or symlink. Its updater owns
that path. A fleet launcher or explicit wrapper command is stable across Codex
updates and avoids recursion when `codex-run` delegates back to the real
binary.

## Current boundary

The managed local app-server endpoint is a WebSocket connection over a Unix
domain socket, so the bridge currently targets Linux and macOS. Plain Codex
continues to receive durable hook-boundary inbox delivery on every supported
platform. A Windows implementation should use a supported local app-server
transport rather than opening an unauthenticated network listener.
