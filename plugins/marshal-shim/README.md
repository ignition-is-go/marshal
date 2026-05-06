# marshal-shim plugin

Connects this Claude Code session to the [marshal](https://github.com/ignition-is-go/marshal) coordination daemon so it can see and message other live Claude sessions on the same machine.

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

After install, every Claude Code session in this directory has four MCP tools:

- `whoami` — your session id, nickname, pid, cwd
- `roster` — every live session the daemon currently sees
- `send_message` — send to a peer by `session_id` (look the id up in `roster` first; nicknames are display-only)
- `set_status` — set the free-form `current_task` text shown on the roster

Peer messages arrive as `notifications/claude/channel` events that surface in your transcript as `<channel>` blocks.

## Install

```text
/plugin marketplace add ignition-is-go/marshal
/plugin install marshal-shim@marshal
```
