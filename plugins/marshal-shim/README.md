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

The shim writes its current nickname to a small state file keyed by its parent PID. The `marshal-shim statusline` subcommand reads that file and renders `[user@host dir] nickname` in your Claude Code footer so you can tell sessions apart at a glance.

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
