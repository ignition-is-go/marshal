# marshal-opencode

The [opencode](https://opencode.ai) counterpart of the Claude Code `marshal-shim`. Connects an opencode session to the [marshal](../../README.md) coordination daemon so it shows up on the same roster as every other live agent session and can pass messages to them.

Where the Claude integration needs a shim **binary** (an MCP child process), a proprietary `notifications/claude/channel` push, curl **hooks**, and `~/.claude` session-id discovery, this is a single in-process opencode **plugin**. opencode's plugin hooks supply everything the shim had to bolt on for Claude:

| Concern | Claude `marshal-shim` | this plugin |
|---|---|---|
| Tools (send/broadcast/…) | stdio MCP server | native opencode tools (`marshal_*`), acting session auto-filled |
| Roster registration | shim startup + `/hook/session-start` | `event` hook → `SET Session` over the myko client |
| Session id | `CLAUDE_CODE_SESSION_ID` / `~/.claude` heuristics | handed to the plugin by opencode |
| Live inbound | `notifications/claude/channel` (proprietary, flag-gated) | `client.tui.showToast` on the daemon's `NotifyChannel` push |
| In-context delivery | `/hook/prompt-submit` → `<marshal_inbox>` | `experimental.chat.system.transform` → `<marshal_inbox>` |
| Liveness / cleanup | 5 s shim loop + `/hook/session-end` | 5 s timer + `session.deleted` |

It talks to the daemon with [`@myko/core`](https://www.npmjs.com/package/@myko/core) — the same myko client the rest of the stack uses — so there is no second source of truth for the wire protocol (the marshal command/query/event shapes live in [`src/entities.ts`](src/entities.ts), hand-mirrored from the Rust `marshal-entities` crate).

## Requirements

- A reachable `marshal-daemon` (see the [top-level README](../../README.md)). The plugin defaults to `ws://localhost:6155`.
- opencode (the plugin runs under opencode's Bun runtime).

## Install

### Option A — npm package (once published)

Add it to your opencode config (`opencode.json` / `~/.config/opencode/opencode.json`):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["marshal-opencode"]
}
```

opencode installs the plugin and its `@myko/core` dependency into `~/.cache/opencode/node_modules` on next start.

### Option B — local checkout (development)

From this directory:

```bash
bun install
```

then point opencode at the local plugin. Either symlink the package into opencode's global plugin dir:

```bash
ln -s "$PWD" ~/.config/opencode/plugin/marshal-opencode
```

or reference it from `opencode.json` by path (see `opencode.example.jsonc`).

## Configure

Set these in the environment opencode launches under:

| Var | Default | Meaning |
|---|---|---|
| `MARSHAL_DAEMON_ADDRESS` | `ws://localhost:6155` | daemon WebSocket URL (`MYKO_ADDRESS` honored as a fallback) |
| `MARSHAL_OPERATOR` | `$USER` / `$USERNAME` | who this session belongs to (anchors the `op:*` room) |

## What you get

Tools (call them like any other opencode tool):

- `marshal_roster` — list every live session (host, cwd, branch, operator, session id)
- `marshal_send_message` — direct-message a peer by session id
- `marshal_broadcast` — message a room (`everyone`, `host:*`, `op:*`, `project:*`, ad-hoc)
- `marshal_join_room` / `marshal_leave_room`
- `marshal_set_status` — set the status peers see on the roster

Inbound peer messages arrive two ways: an instant TUI toast (real-time) and, authoritatively, as a `<marshal_inbox>` block injected at the start of your next turn (untrusted-input framed, then acked).

## Testing

```bash
bun test                              # unit + integration
bun test test/entities.test.ts        # unit only (wire-shape contract)
```

- **Unit** (`test/entities.test.ts`) pins the marshal wire shapes — command/query ids, camelCase payloads, `asSession` — so an edit to `entities.ts` that drifts from the Rust `marshal-entities` serde fails here, no daemon needed.
- **Integration** (`test/integration.test.ts`) spins up a **real `marshal-daemon` binary** and round-trips the plugin's actual `MarshalDaemon` (the `@myko/core` client) against it over the real myko WS wire: roster registration + entity fields, send → inbox-pull + ack, the real-time `NotifyChannel` push, `join_room` + `broadcast` delivery, and `set_status`. It does **not** fake the daemon or the wire — that round-trip is the thing under test, and it is also the **drift guard** for the wire shapes in `entities.ts`: rename a field in the Rust `marshal-entities` source and the round-trip fails. The suite skips (loudly) if no daemon binary is found; enable it with:

  ```bash
  (cd ../.. && cargo build -p marshal-daemon)   # builds target/debug/marshal-daemon
  bun test test/integration.test.ts             # or set MARSHAL_DAEMON_BIN=/path/to/marshal-daemon
  ```

## Status / follow-ups

What's verified: typecheck against the real `@opencode-ai/plugin` + `@myko/core` types, and the **full daemon wire round-trip** (the 4 integration tests above). Not yet exercised: the opencode-hook glue inside a *live opencode session* (the hooks are typechecked against opencode's real types but unrun end-to-end in opencode itself).

- `src/entities.ts` is a small hand-mirror of the Rust `marshal-entities` wire shapes, kept honest by the integration test above (the drift guard). `marshal-entities` *can* emit these as TypeScript via `cargo run -p marshal-entities --features codegen --bin typegen -- <dir>` (the `typegen` bin is wired and works), but ts-rs dumps the whole linked type graph — marshal's types plus all of myko's base types — so consuming the full static surface wasn't worth the build pipeline for this 6-verb plugin. The hand-mirror + live round-trip is the deliberate trade.
- A pulse-deploy Ansible role to deploy + wire this on the fleet is a separate follow-up (parallel to `roles/marshal_shim*`).
- A couple of opencode-surface details (the exact field carrying the session id into `experimental.chat.system.transform`, and toast variant names) are handled defensively; verify against the opencode version you run.

> Note: the daemon serves its WebSocket at the `/myko` path. The plugin accepts a bare `ws://host:6155` (the fleet convention) and appends `/myko` automatically — see `withMykoPath` in `src/daemon.ts`.
