# Installing marshal

## Build

```
cargo install --path crates/daemon
cargo install --path crates/shim
```

This places `marshal-daemon` and `marshal-shim` in `~/.cargo/bin`. Make sure that's on your `PATH`.

## Configure Claude Code

Add an MCP entry — one entry per Claude Code config; every session uses the same one:

```json
{
  "mcpServers": {
    "marshal": {
      "command": "marshal-shim"
    }
  }
}
```

## Daemon lifecycle

The shim auto-starts the daemon on first connect via a detached background process. No systemd needed.

To run the daemon manually for debugging:

```
marshal-daemon --foreground
```

## State

- Socket: `$XDG_RUNTIME_DIR/marshal/sock` (fallback `~/.local/state/marshal/sock`)
- DB:     `~/.local/state/marshal/db.sqlite`
- Logs:   `~/.local/state/marshal/daemon.log` (daily rotation)

## Coordinating with peers

Each Claude session that connects via `marshal-shim` shows up on the
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
