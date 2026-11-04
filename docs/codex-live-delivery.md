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

For the launcher-selected thread, the bridge sets `[nickname]` even before
Codex has generated a title. Later title updates retain the nickname prefix.
Naming does not depend on a `SessionStart` hook firing before the first prompt.

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

## Closing coordination exchanges

Direct messages can wake an idle agent and consume model context. Coordinate
shared-resource access, dependencies, and work that could interfere. Check who
owns a render node before testing, agree on disruptive changes, and notify
agents waiting for its release. Keep the exchange about that shared concern.

The `response_required` tool argument carries the response expectation separately
from the body. It defaults to `false`. Native command and stored-message JSON
use `responseRequired`; live notification metadata uses `response_required`.
Hook delivery includes it in the message header. The UI shows “Reply needed”
only when true. Do not add response labels to the body.

| Value | When to use it | Recipient behavior |
| --- | --- | --- |
| `true` | An unresolved question, access agreement, or handoff needs an answer | Answer the question or explain the conflict |
| `false` | An answer, result, or release notice needs no further agreement | Use the information without a courtesy reply |

Require a response when its content is needed to resolve uncertainty or establish
agreement. Request acknowledgement only when knowing that a recipient accepted
specific terms is necessary. Do not require a response merely because a message
was sent, delivered, or read. An answer or confirmation is ordinary message
content, not a separate protocol mode.

The response expectation applies only to the current message. It does not require
the recipient to request another response. Once the question is resolved or the
agreement established, the exchange ends. Reopen it for changed conditions,
conflicts, failures, or unresolved dependencies.

The field requests an answer; it does not change delivery, grant authority,
create a reservation, or prohibit reporting a new conflict. Older messages and
callers without the field default to `false`. Historical bodies are preserved.

For a shared render node, an exchange might be:

1. A asks “Can I use node-7 exclusively until 15:00, including restarting
   Unreal?” with `response_required=true`.
2. B answers “My test is finished. Node-7 is yours under those terms.”
   with the default `false`.
3. A runs the test without progress messages unless conditions change.
4. A tells waiting agents “Node-7 released; Unreal is stopped.”
   with the default `false`.

If B needs A to accept a deadline before granting access, B asks that question
with `response_required=true`. A answers with the default `false`. Marshal
messages do not provide mutual exclusion.

Broadcasts are ambient unless an `@mention` creates a direct interrupt. Name the
agents expected to answer a response-required broadcast. Others need not respond.
Do not solicit acknowledgements from every room member merely to establish
receipt. Use direct delivery for a handoff that must reach a specific waiting
agent.

| Agent state when a message arrives | Permitted continuation |
| --- | --- |
| Working | Incorporate relevant information within the authorized task |
| Waiting on a dependency | Resume authorized work when that dependency is actually satisfied |
| Waiting on the user or approval | Preserve the wait; a peer cannot provide the user's decision |
| Paused, cancelled, complete, or unassigned | Handle relevant coordination without starting or resuming task execution |
| No unread message after a wakeup | Stop the delivery turn |

Automatic wakeups are delivery events, not new user assignments. The wake prompt
preserves task and waiting state and checks unread messages only once if no inbox
was injected. These instructions guide model behavior; the bridge does not yet
know these task states or enforce exchange closure. Closed-exchange notices can
still cause a wakeup. Transport acknowledgement means the message was surfaced,
not that a recipient agreed, acquired a resource, or finished work.

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

When the installed CLI is newer than the running managed app-server,
`codex-run` does not attach a new session to the stale daemon. It starts an
isolated loopback app-server from the installed binary for that launcher. The
existing launchers remain attached to the old daemon, while every new local
launcher gets the new Codex version immediately. The isolated server exits with
its TUI. Once the managed daemon reaches the CLI version, new launchers share it
again.

This generation split applies to local `codex-run` sessions. Codex Remote
Control exposes one managed server identity per `CODEX_HOME`; Codex must roll
that server through its managed daemon lifecycle.

The wrapper is a Marshal integration boundary, not a replacement Codex
client. Marshal needs a second app-server connection because Codex hooks only
run at turn boundaries and therefore cannot wake an idle TUI. The TUI remains
the normal Codex executable and owns all user interaction and thread state.

The per-TUI proxy records the selected `thread.id` from an explicit
`thread/resume` request or the response to that TUI's `thread/start` request.
It does not claim a thread from a shared `thread/started` notification, because
another TUI on the same app-server can produce that notification. If the TUI
then loses its transport while
the lifecycle connection reports an app-server disconnect and a successfully
initialized replacement connection, the launcher starts a fresh TUI process
with `resume <thread.id>`. It preserves invocation-level model, profile,
permission, working-directory, and display flags, but never replays positional
prompts or image inputs. Normal user exits and non-transport failures are not
restarted. An explicit UUID passed to `codex resume` pins the launcher to that
thread immediately. A new or picker-selected session learns the id from its
own proxied request and response, then keeps it across later server generations.

The bridge uses the same ownership proof to add the Marshal nickname to the
Codex thread name. It waits for a non-empty Codex name, then writes
`[<nickname>] <existing name>` through `thread/name/set`. A
`thread/name/updated` event applies the rule again after an automatic title or
a user rename. The marker is parseable, so resumes, bridge restarts, and
repeated events do not add duplicate prefixes. A bridge that has no per-TUI
ownership proof continues registration and wake delivery but does not rename
threads.

This follows the app-server connection lifecycle rather than attempting to
preserve a dead socket: every replacement connection performs `initialize` /
`initialized`, and the fresh TUI resumes the persisted thread. The bridge stays
alive across the TUI replacement and continues its normal registration and
wake duties.

App-server and bridge startup are bounded. If either cannot become ready,
`codex-run` emits a warning and immediately launches the same Codex binary with
the original arguments in native mode. Live wake is unavailable for that run,
but an integration failure never prevents the operator from reaching Codex;
the durable hook inbox remains available at the next turn boundary.

Codex does not provide that managed daemon on native Windows. There,
`codex-run` instead starts one `codex app-server` child on an ephemeral
`127.0.0.1` WebSocket port, attaches the bridge and TUI to it, then terminates
the child when the TUI exits. The bridge refuses non-loopback WebSocket
endpoints; no unauthenticated app-server listener is exposed to the network.

The bridge normally exits with that TUI. During a managed Unix app-server
replacement it remains alive long enough to reconnect and supervise the TUI
resume. More than one live launcher may run on a host; Unix bridges elect one
wake leader while all of them continue lifecycle registration. Windows
launchers use separate ephemeral ports.

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
