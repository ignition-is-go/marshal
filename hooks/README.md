# marshal hooks — dumb-curl client (no scripts, no shim)

Claude Code receives peer messages by PULLING at turn boundaries via
hooks, instead of the daemon PUSHING `notifications/claude/channel`
(which needed `--dangerously-load-development-channels` and gave the
daemon a context-injection privilege).

All hook logic lives in the **daemon**, behind plain-HTTP endpoints. The
hook command is a dumb, cross-platform `curl` one-liner — no scripts,
no jq/bash/PowerShell, nothing to install per platform:

```
curl -sS --max-time 5 -X POST \
  "$URL/hook/session-start?host=$(hostname -s)&operator=$USER" \
  --data-binary @- || true
```

curl pipes Claude Code's hook JSON (stdin) to the daemon and the
daemon's `text/plain` response back to stdout (added to the agent's
context).

## Endpoints (served by the daemon, see `crates/daemon/src/hooks.rs`)

| Endpoint | Claude Code hook | Job |
|---|---|---|
| `POST /hook/session-start` | `SessionStart` | register the roster entry keyed by `session_id`; return any backlog as `<marshal_inbox>` text. |
| `POST /hook/prompt-submit` | `UserPromptSubmit` | fetch unread for `session_id`, return framed, ack. The receive path. |
| `POST /hook/session-end` | `SessionEnd` | deregister. |

`session_id` and `cwd` come from the hook JSON body; `host`/`operator`
ride in the query string because the daemon can't know the *client's*
hostname/user — the curl command expands them locally (the only
platform-specific bit: `$VAR` on Linux, `%VAR%` on Windows cmd).

## Sending

The agent sends with the curated marshal MCP tools (`send_message`,
`broadcast`, …) configured as a `type: http` server in `.mcp.json`,
passing its `cc_session_id` as `as_session`.

## Identity & security

Identity unifies on the Claude Code `session_id`: peers address it, the
inbox keys on it, the statusline shows it. No channel push privilege, no
`--dangerously-` flag; peer content enters context only at
operator-initiated turn boundaries, framed as untrusted, behind the
mesh-only daemon bind.
