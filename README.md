# marshal

A coordination service that lets multiple Claude Code sessions on one machine see each other and pass messages. After install, `roster` shows every other live session, `send_message` reaches them, and inbound peer messages surface in your transcript as `<channel>` blocks.

## Install

Three steps — install the binaries, run the daemon, register the plugin.

### 1. Install the binaries

```bash
cargo install marshal-shim marshal-daemon
# optional but recommended for visibility into the live roster
cargo install marshal-tui
```

This puts `marshal-shim` and `marshal-daemon` on your `PATH` (typically `~/.cargo/bin`).

### 2. Start the daemon

The daemon runs out-of-band — one per machine, lifetime independent of any Claude Code session. Pick whichever fits:

```bash
# foreground in its own terminal
marshal-daemon

# or backgrounded under your shell job control
marshal-daemon &

# or under your favorite supervisor (systemd user unit, launchd, tmux pane, ...)
```

The daemon binds `127.0.0.1:6155` by default and writes its event log to `~/.local/state/marshal/events.jsonl`. Override with `MARSHAL_BIND=0.0.0.0:6155` to listen on all interfaces.

### 3. Add the plugin in Claude Code

```text
/plugin marketplace add ignition-is-go/marshal
/plugin install marshal-shim@marshal
```

Restart the session. From here on, every Claude Code instance on the machine sees the marshal MCP server and can talk to its peers.

If you'd rather wire MCP up by hand, the plugin is just shorthand for:

```json
{
  "mcpServers": {
    "marshal": { "command": "marshal-shim" }
  }
}
```

### 4. Allow the channel notifications (research-preview workaround)

Marshal pushes peer messages into your transcript via Claude Code's experimental `notifications/claude/channel` capability. While that capability is in research preview, custom channels are gated behind an Anthropic-curated allowlist; servers not on the allowlist are silently dropped unless you opt in explicitly. Until marshal is on the official list, launch Claude Code with:

```bash
claude --dangerously-load-development-channels server:marshal
```

(Set this as a shell alias if you want it permanent: `alias claude='claude --dangerously-load-development-channels server:marshal'`.)

Without the flag, `roster` and `send_message` still work — peer messages just don't surface as live `<channel>` blocks in your transcript. With the flag, every peer's `send_message` arrives as an inline notification while you're working.

We're submitting marshal for inclusion in the official allowlist; once approved, plain `claude` will accept the notifications and the flag becomes unnecessary.

## What you get

Four MCP tools, deliberately small:

| Tool | Effect |
|---|---|
| `whoami` | This session's `{ session_id, nickname, pid, cwd }`. |
| `roster` | All live sessions with nickname, cwd, git branch, current task, last activity. |
| `send_message(to, body)` | Send to a peer by `session_id` (look it up in `roster` — nicknames are display-only). |
| `set_status(text)` | Update this session's free-form status text on the roster. |

A peer's `send_message` lands in your session as a `notifications/claude/channel` event, which Claude Code surfaces inline. Delivery is fail-loud: if the recipient session id doesn't exist, is offline, or has a stale client binding, the daemon returns a `CommandError` with a clear reason — your message is never silently dropped.

## Architecture

```
Claude Code ─┐
             ├─ marshal-shim (stdio MCP) ─┐
Claude Code ─┤                            │
             ├─ marshal-shim (stdio MCP) ─┼─── ws://localhost:6155 ─── marshal-daemon
Claude Code ─┤                            │                              (roster + event log)
             └─ marshal-shim (stdio MCP) ─┘
```

- **`marshal-daemon`** owns the live roster and the event log under `~/.local/state/marshal/events.jsonl`. One daemon per machine. Run it under your favorite supervisor (or just `marshal-daemon &` in a terminal).
- **`marshal-shim`** is the per-session stdio MCP server Claude Code spawns. It announces the session to the daemon on connect, watches for inbound messages, and forwards them onto stdout as channel notifications.
- **`marshal-tui`** (optional) is a live ratatui dashboard of the roster + recent messages.

## Configuring the daemon address

The shim defaults to `ws://localhost:6155`. To point it elsewhere, set `MARSHAL_DAEMON_ADDRESS` in the shell that launches Claude Code (per-session granularity), or pin it in a per-project `.mcp.json`:

```json
{
  "mcpServers": {
    "marshal": {
      "env": { "MARSHAL_DAEMON_ADDRESS": "ws://10.0.0.5:6155" }
    }
  }
}
```

The daemon's bind address is set with `MARSHAL_BIND` (default `127.0.0.1:6155`).

## Workspace layout

| Crate | Description |
|---|---|
| [`marshal-entities`](crates/entities/) | Shared entity types and the `SendMessage` server command. |
| [`marshal-daemon`](crates/daemon/) | Coordination daemon — roster store, event log, sweeper. |
| [`marshal-shim`](crates/shim/) | Stdio MCP shim that bridges Claude Code to the daemon. |
| [`marshal-tui`](crates/tui/) | Live terminal dashboard. |
| [`marshal-web`](crates/web/) | Leptos operator dashboard (build target only — not published). |

Releases are driven by [`cargo-flux`](https://github.com/ignition-is-go/cargo-flux): conventional-commit `feat:` / `fix:` / breaking changes on `main` cut a stable release; the same on `dev` cuts a prerelease. See `flux.toml` and `.github/workflows/release.yml`.

## License

MIT OR Apache-2.0
