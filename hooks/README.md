# marshal hooks — pull-via-hook client (no shim, no channels)

These Claude Code hook scripts replace the marshal-shim. Instead of a
persistent MCP subprocess that receives `notifications/claude/channel`
*push* events (which require the `--dangerously-load-development-channels`
flag and give the daemon a context-injection privilege), the agent:

- **sends** via the daemon's curated HTTP-MCP tools (`send_message`,
  `broadcast`, `set_status`, …) configured as a `type: http` server in
  `.mcp.json`, passing its `cc_session_id` as `as_session`;
- **receives** by *pulling* — these hooks fetch unread messages at
  defined turn boundaries and print them into context, framed as
  untrusted peer input.

Identity unifies on the Claude Code `session_id` (`cc_session_id`):
peers address it, the inbox query keys on it, and the statusline (a `jq`
one-liner over the hook stdin) shows it. No daemon-minted session id, no
shim-picked uuid.

## Scripts

| Script | Hook event | Job |
|---|---|---|
| `mcp.sh` | — | shared helper: `marshal_mcp <method> <params>` + `marshal_surface_unread <sid>`. Sourced by the others. |
| `session-start.sh` | `SessionStart` | `register` the roster entry keyed by `session_id`; drain backlog → context. |
| `prompt-submit.sh` | `UserPromptSubmit` | fetch unread addressed to this session, surface → context, ack. The receive path. |
| `session-end.sh` | `SessionEnd` | `deregister` the roster entry on clean exit (sweeper is the crash fallback). |

## Deployment

Installed by the `marshal_client` Ansible role to `/usr/local/lib/marshal/`,
wired into `~/.claude/settings.json` `hooks` + `statusLine` and the
`marshal` `type: http` entry in `.mcp.json`. `MARSHAL_HTTP_URL` points at
the daemon's `/myko/mcp` over the NetBird mesh.

## Security

Removing channels removes the daemon's push privilege and the
`--dangerously-` flag. Peer-message content still enters context (that's
the feature) but only at operator-initiated turn boundaries, via these
operator-authored scripts, which frame it as untrusted. Combined with the
mesh-only daemon bind, the injection surface is enrolled-peers-only and
the surfacing is gated + bounded.
