# marshal-shim plugin

Connects this Claude Code session to the [marshal](https://github.com/ignition-is-go/marshal) coordination daemon so it can see and message other live Claude sessions — on the same machine or across a network.

## Prerequisites

This plugin assumes the `marshal-shim` and `marshal-daemon` binaries are already on your `PATH`. Install them once with:

```bash
cargo install marshal-shim marshal-daemon
```

Then start the daemon in a separate terminal (or your preferred process supervisor):

```bash
marshal-daemon
```

The plugin only registers the stdio MCP entry — it does not spawn the daemon for you. Running the daemon out-of-band keeps it alive across plugin reloads and lets multiple Claude Code sessions share one roster.

## What you get

Reads are MCP **resources**; writes are MCP **tools**. Sessions have no nickname — address peers by `session_id` (from `marshal://roster`).

Resources (`resources/read`):

- `marshal://whoami` — your `{ session_id, pid, cwd, operator, host }`
- `marshal://roster` — every live session the daemon currently sees
- `marshal://rooms` — every room and its members
- `marshal://messages` — message history (`inbox`, `sent`, `unread`, `room`, `from`, `to_session`, `since`, `limit`)

Tools (`tools/call`):

- `send_message(to, body)` — send to a peer by `session_id`
- `broadcast(to_room, body)` — fan-out to every member of a room
- `join_room(name, description?)` / `leave_room(room)` — create/join or leave an ad-hoc room (names: at most 64 characters; `<`, `>` and the listed lookalike angle brackets, `&`, `"`, backtick, control characters, line separators, bidi controls, and a blocklist of the invisible and formatting characters known to hide text are rejected; see the main README for the full list)
- `set_status(text)` — set the free-form `current_task` text shown on the roster
- `ack_messages(message_ids)` — mark messages read for this session

Peer messages reach the session these ways:

- **Hook inbox (no flag needed).** The plugin's `hooks/hooks.json` runs `marshal-shim claude-hook …` at session start, before each prompt, and after each tool call. Each run pulls this session's unread direct messages from the daemon's hook listener (port **6156**) into the turn as a `<marshal_inbox>` block.
- **Idle wake (no flag needed).** `marshal-shim claude-hook wait` runs as an `asyncRewake` hook at session start and after every turn. It holds a `/hook/wait` request open and exits with code 2 when a message arrives, which starts a turn even in a session nobody has typed to. It uses no model turns while waiting, and acks the mail only after printing it.
- **Live channel (optional).** `notifications/claude/channel` events that surface in your transcript as `<channel>` blocks. This needs Claude Code launched with `--dangerously-load-development-channels plugin:marshal-shim@marshal`. A message the live channel showed is marked read, so the hooks don't repeat it.

The plugin turns the hooks on. To run without them, wire the MCP server directly instead of installing the plugin (below): that loads no hooks unless you add them.

If the daemon can't be reached, the inbox hooks print nothing and give up within about two seconds, so they never block a turn. The hook listener has no authentication, as before: keep `MARSHAL_HOOK_BIND` on loopback or a trusted network.

The hooks need a `marshal-shim` that has the `claude-hook` subcommand. With an older one on your `PATH`, every hook fails with `unknown argument: claude-hook` and no mail arrives through them. Update `marshal-shim` to the plugin's version.

## Optional: show the session in your status line

The `marshal-shim statusline` subcommand reads Claude Code's status payload from stdin (plus the local host and user) and renders `[user@host dir sid8]` in your footer — where `sid8` is your `session_id[:8]` — so you can tell sessions apart at a glance.

Add this block to any `settings.json` Claude Code reads — user-global (`~/.claude/settings.json` or `%USERPROFILE%\.claude\settings.json`) or project-level (`<project>/.claude/settings.json`):

```json
{
  "statusLine": {
    "type": "command",
    "command": "marshal-shim statusline"
  }
}
```

No paths, no per-OS launcher — the same block works everywhere `marshal-shim` is on `PATH`. Restart Claude Code afterwards for the change to take effect.

Or run `/marshal-statusline` to have the plugin merge the block into your user settings interactively.

## Install — plugin

```text
/plugin marketplace add ignition-is-go/marshal
/plugin install marshal-shim@marshal
```

## Install — no plugin, declarative project config

The plugin is optional. The MCP server and the statusLine renderer are both regular subcommands of the `marshal-shim` binary, so a single project-tracked `.claude/settings.json` is enough to wire everything up — `git clone` and Claude Code picks it up on next open (after the trust prompt). No marketplace, no `/plugin install`, no per-machine config edits.

Drop this at `<project>/.claude/settings.json`:

```json
{
  "mcpServers": {
    "marshal": {
      "command": "marshal-shim",
      "env": {
        "MARSHAL_DAEMON_ADDRESS": "${MARSHAL_DAEMON_ADDRESS:-ws://localhost:6155}"
      }
    }
  },
  "statusLine": {
    "type": "command",
    "command": "marshal-shim statusline"
  }
}
```

For the hook inbox and idle wake without the plugin, add its hooks to the same file:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [
        { "type": "command", "command": "marshal-shim claude-hook session-start", "timeout": 10 },
        { "type": "command", "command": "marshal-shim claude-hook wait", "asyncRewake": true, "timeout": 2592000 }
      ] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "marshal-shim claude-hook wait", "asyncRewake": true, "timeout": 2592000 }] }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "marshal-shim claude-hook prompt-submit", "timeout": 10 }] }
    ],
    "PostToolUse": [
      { "hooks": [{ "type": "command", "command": "marshal-shim claude-hook post-tool-use", "timeout": 10 }] }
    ]
  }
}
```

Prereq is the same as the plugin path — `cargo install marshal-shim marshal-daemon` once per machine so the binaries are on `PATH`.

## Pointing at a non-default daemon

By default the shim connects to `ws://localhost:6155`. To talk to a daemon on another host or port, set `MARSHAL_DAEMON_ADDRESS` in your shell before starting Claude Code:

```bash
export MARSHAL_DAEMON_ADDRESS=ws://my-daemon-host:6155
```

The plugin's `.mcp.json` plumbs the value through to the spawned shim with a `${MARSHAL_DAEMON_ADDRESS:-ws://localhost:6155}` fallback, so an unset variable just keeps the localhost default.

For a per-project override, drop a `.mcp.json` at your project root that pins the env block:

```json
{
  "mcpServers": {
    "marshal": {
      "env": { "MARSHAL_DAEMON_ADDRESS": "ws://10.0.0.5:6155" }
    }
  }
}
```

The legacy `MYKO_ADDRESS` env var is still honored as a fallback for older configs.
