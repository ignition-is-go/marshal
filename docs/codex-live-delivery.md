# Codex live delivery

Codex command hooks can add context to a turn, but they cannot create a turn.
That is why a direct Marshal message sent to an idle, normally launched Codex
session remains unread until the user submits another prompt.

Codex app-server provides the missing control plane. Multiple clients can
attach to one app-server, and any client can issue `turn/start` for an idle
thread. The original TUI receives that turn's normal streamed events.

It also provides the earliest authoritative session lifecycle signal.
`codex-run` connects the bridge first and waits until its app-server
subscription is ready before launching the TUI. The TUI's `thread/started`
notification contains the canonical root `sessionId` and cwd, so the bridge
registers that session with Marshal immediately—without waiting for a first
prompt or `SessionStart` hook.

## Delivery path

```text
codex-run ──wait for subscriber──> local Codex bridge
                                      ▲
                                      │ thread/started(sessionId, cwd)
                                      │
                               shared app-server <── attached TUI
                                      │
                                      └── /hook/session-register ──> roster

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

Eager registration uses the dedicated `/hook/session-register` endpoint. It
only creates or refreshes the roster row; it never surfaces or acknowledges
inbox messages because there is no model turn available to receive them.
`SessionStart` remains the identity/context injection path and a fallback for
Codex launches that do not use `codex-run`.

The bridge deliberately does not mark the message read. The existing hook
acknowledges it only after the daemon successfully writes the
`<marshal_inbox>` response back to Codex. If the app-server is unavailable, the
thread is busy, or `turn/start` fails, the durable message stays unread and the
ordinary prompt/tool-boundary path remains intact.

Only unread direct messages are wake candidates. Room broadcasts are ambient,
and the hook continues to frame peer content as untrusted coordination input:
a peer cannot expand the operator's task or authority merely by waking a turn.

## Context and wake bounds

Automatic delivery is intentionally smaller than durable history:

- a live notification or single hook entry carries at most 2,000 body
  characters;
- a hook surfaces at most 20 messages and divides an 8,000-character body
  budget across that batch;
- truncated entries include their message id, and the complete body remains in
  `marshal://messages`;
- successful wakes are coalesced per thread for 30 seconds, including after
  the first message is acknowledged, so a burst joins the active turn instead
  of creating one turn per message.

On Linux and macOS, every interactive launcher keeps its lifecycle subscriber,
but bridges attached to the same app-server and Marshal daemon elect one
host-local wake leader with an advisory file lock. A new leader waits briefly
before waking so the old leader's in-flight hook acknowledgement can settle.
Dropping the leader process releases the lock and another bridge takes over.
This prevents one unread row from producing duplicate turns when several Codex
TUIs share the managed app-server. Native Windows launchers use isolated
app-servers, so each bridge owns wake delivery for its own endpoint.

Direct messages are therefore an interrupt and consume recipient context. Use
them for an action, blocker, or needed reply and batch related information.
Use a room broadcast without an `@mention` for FYI/progress; an `@mention`
intentionally creates a direct interrupt.

## Send status model

`send_message` returns as soon as the daemon has persisted the message and
finished any synchronous live-channel push. A Codex bridge observes the unread
message later, so the send response cannot claim whether `turn/start`
succeeded:

- `persisted: true` confirms durable message storage;
- `live_push` is `delivered`, `unavailable`, `failed`, or `unknown`;
- `wake` is `not_needed` after a delivered live push, otherwise `unobserved`.

`unobserved` is deliberately not `failed`: a configured bridge may wake the
recipient immediately after the send response, as happened in the normal idle
Codex path. The legacy `delivery` and `delivered_live` fields remain for
compatibility, but describe only the synchronous live push.

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

On Linux and macOS, `codex-run` performs three actions:

1. idempotently starts `codex app-server daemon`;
2. starts `marshal-shim codex-bridge` and waits until its lifecycle
   subscription is ready;
3. runs `codex --remote unix:// ...`; the resulting `thread/started` event
   registers the session before any prompt is required.

Codex does not provide that managed daemon on native Windows. There,
`codex-run` instead starts one `codex app-server` child on an ephemeral
`127.0.0.1` WebSocket port, attaches the bridge and TUI to it, then terminates
the child when the TUI exits. The bridge refuses non-loopback WebSocket
endpoints; no unauthenticated app-server listener is exposed to the network.

The bridge exits with that TUI. More than one live launcher may run on a host;
Unix bridges elect one wake leader while all of them continue lifecycle
registration. Windows launchers use separate ephemeral ports.

For diagnostics or a supervisor-managed deployment, run the bridge directly:

```sh
marshal-shim codex-bridge \
  --daemon ws://marshal-host:6155 \
  --socket "$CODEX_HOME/app-server-control/app-server-control.sock"

# Native Windows or a manually supervised local app-server:
marshal-shim codex-bridge \
  --daemon ws://marshal-host:6155 \
  --endpoint ws://127.0.0.1:4500
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

## Platform boundary

The managed app-server daemon and its Unix-domain control socket are
Unix-only. Native Windows uses Codex's loopback WebSocket transport under the
launcher-owned process lifecycle described above. It is intentionally not a
machine-wide daemon: loopback keeps the endpoint private to the host, while
per-launcher processes avoid port sharing and make cleanup deterministic.
