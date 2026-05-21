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

## Optional: show the nickname in your status line

The shim writes its current nickname to a small state file keyed by its parent PID. A bundled helper script reads that file and renders `[user@host dir] nickname` in your Claude Code footer so you can tell sessions apart at a glance.

The fastest way to wire it up:

```text
/marshal-statusline
```

The command resolves the plugin's `bin/` directory, picks the right launcher for your OS (bash on macOS/Linux, PowerShell on Windows), shows you the JSON it intends to merge into your `settings.json`, and writes it on confirmation. Restart Claude Code afterwards for the change to take effect.

If you'd rather configure it by hand:

**macOS / Linux** — in `~/.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "bash <plugin-root>/bin/statusline.sh"
  }
}
```

**Windows** — in `%USERPROFILE%\.claude\settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "powershell -NoProfile -ExecutionPolicy Bypass -File <plugin-root>\\bin\\statusline.ps1"
  }
}
```

Replace `<plugin-root>` with the absolute path of this installed plugin directory.

## Install

```text
/plugin marketplace add ignition-is-go/marshal
/plugin install marshal-shim@marshal
```

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
