# Installing claude-coord

## Build

```
cargo install --path crates/daemon
cargo install --path crates/shim
```

This places `claude-coord-daemon` and `claude-coord-shim` in `~/.cargo/bin`. Make sure that's on your `PATH`.

## Configure Claude Code

Add an MCP entry — one entry per Claude Code config; every session uses the same one:

```json
{
  "mcpServers": {
    "claude-coord": {
      "command": "claude-coord-shim"
    }
  }
}
```

## Daemon lifecycle

The shim auto-starts the daemon on first connect via a detached background process. No systemd needed.

To run the daemon manually for debugging:

```
claude-coord-daemon --foreground
```

## State

- Socket: `$XDG_RUNTIME_DIR/claude-coord/sock` (fallback `~/.local/state/claude-coord/sock`)
- DB:     `~/.local/state/claude-coord/db.sqlite`
- Logs:   `~/.local/state/claude-coord/daemon.log` (daily rotation)

Unread messages persist within a daemon's lifetime. They do **not** survive a daemon restart in v1; ids are regenerated on every restart and stored messages age out at 30 days.
