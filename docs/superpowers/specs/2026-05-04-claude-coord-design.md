# claude-coord — design

## Goal

Let multiple Claude Code sessions running on one machine see each other and pass async notes. v1 covers two primitives:

- **Roster / awareness** — every session announces itself; any session can ask who else is running and what they're doing.
- **Async messaging** — sessions drop notes into each other's inboxes; the recipient reads its inbox on its next tool call.

Out of scope for v1: locks, request/response, broadcast channels, capability tags, persistent identity across reconnects, multi-machine, auth beyond filesystem perms.

## Architecture

A long-running per-user daemon owns the shared state. Each Claude Code session launches a thin stdio MCP server (the "shim") that forwards each tool call to the daemon over a unix socket.

```
Claude Code session A ──┐
                        ├── stdio MCP shim A ──┐
Claude Code session B ──┤                       │   unix socket
                        ├── stdio MCP shim B ──┼─────────────────► claude-coord-daemon
Claude Code session C ──┘                       │                   (roster + SQLite inbox)
                        └── stdio MCP shim C ──┘
```

Shim and daemon are separate Rust binaries in one workspace.

```
claude-coord/
├── crates/
│   ├── proto/      # shared wire types (serde) + length-prefixed framing
│   ├── daemon/     # long-running tokio binary, owns state
│   └── shim/       # rmcp stdio server, thin client to daemon
└── Cargo.toml
```

`proto` is the only dependency shared between `daemon` and `shim`.

## MCP tool surface

Six tools, deliberately small.

| Tool | Effect |
|---|---|
| `whoami()` | Returns this session's `{ session_id, nickname, pid, cwd }`. |
| `set_status(text)` | Updates this session's free-text `current_task` on the roster. |
| `roster()` | Returns all live sessions with nickname, cwd, git_branch, current_task, last_heartbeat. Self is included and marked. |
| `send_message(to, body)` | `to` is a session id or nickname. Returns `{ message_id }`. Ambiguous nickname returns an error listing candidates. |
| `inbox(mark_read=true)` | Returns unread messages addressed to me, oldest first. |
| `recent_messages(limit=50)` | All recent messages involving me (sent or received), read or not. Read-only context. |

No locks, channels, broadcast, or tags in v1. Adding any of them later does not break this surface.

## Identity and roles

Identity is `(session_id, nickname)`.

- `session_id` is a short auto-generated id (`s-` + 4 hex chars; regenerated if it collides with a live session). Stable for the life of the shim process.
- `nickname` defaults to `cwd.file_name()` (the basename of the working directory). One folder = one role; running an agent in `~/Code/eww` makes it `eww` on the roster.
- A user can override their nickname later — out of scope for v1; if two sessions share a basename, address them by `session_id`.

`send_message(to: "eww", ...)` resolves to the live session whose nickname is `eww`. If multiple match, the daemon returns an error containing the candidates' ids.

## Daemon state

**In-memory:**
- `roster: HashMap<SessionId, SessionInfo>` where `SessionInfo` is `{ nickname, pid, cwd, git_branch, current_task, connected_at, last_heartbeat }`.
- A tokio `broadcast` channel for "roster changed" / "new message" events. Unused by v1 tools but plumbed for future push-style features.

**Persistent (SQLite at `~/.local/state/claude-coord/db.sqlite`):**

```sql
CREATE TABLE messages (
  id           INTEGER PRIMARY KEY,
  from_session TEXT NOT NULL,   -- session_id at send time
  from_nick    TEXT NOT NULL,   -- denormalized for display after sender disconnects
  to_session   TEXT NOT NULL,   -- session_id resolved at send time
  to_nick      TEXT NOT NULL,
  body         TEXT NOT NULL,
  sent_at      INTEGER NOT NULL,  -- unix millis
  read_at      INTEGER             -- null if unread
);
CREATE INDEX idx_messages_to ON messages(to_session, read_at);
```

Roster is not persisted — sessions re-announce on connect. Messages persist; `inbox()` returns rows where `to_session = me AND read_at IS NULL`. Messages older than 30 days are pruned by a periodic task.

## Session lifecycle

1. Shim starts. Tries to `connect()` the unix socket at `$XDG_RUNTIME_DIR/claude-coord/sock` (fallback `~/.local/state/claude-coord/sock`).
2. On `ENOENT` or `ECONNREFUSED`, shim forks `claude-coord-daemon` as a detached background process (double-fork), polls the socket for up to ~2s, retries.
3. Shim sends `Hello { pid, cwd, git_branch? }`. Daemon assigns a fresh `session_id`, derives the nickname from `cwd.file_name()`, and replies `Welcome { session_id, nickname }`.
4. Each MCP tool call becomes one daemon RPC. Any RPC counts as a heartbeat — `last_heartbeat` updates on every call. No separate ping in v1.
5. On clean disconnect (shim process exits), daemon removes the session from the roster. Persistent messages addressed to that id remain in SQLite (they will simply never be read unless the same id reconnects, which v1 doesn't support — they age out at 30 days).
6. On abrupt disconnect (broken pipe), daemon's per-connection task exits and removes the entry the same way.

## Wire protocol (shim ↔ daemon)

JSON over a unix stream socket. Each frame is a 4-byte little-endian length prefix followed by JSON bytes.

```rust
// proto crate
enum ClientMsg {
    Hello { pid: u32, cwd: PathBuf, git_branch: Option<String> },
    Rpc   { id: u64, method: String, params: serde_json::Value },
}

enum ServerMsg {
    Welcome { session_id: String, nickname: String },
    RpcOk   { id: u64, result: serde_json::Value },
    RpcErr  { id: u64, code: ErrorCode, message: String },
    Event   { kind: String, payload: serde_json::Value },  // reserved, unused in v1
}

enum ErrorCode { UnknownRecipient, AmbiguousRecipient, BadRequest, Internal }
```

Methods (1:1 with the MCP tools, plus internal):

- `roster` — params `{}` → list of `SessionInfo` with `session_id` and `is_self` flags.
- `set_status` — params `{ text: String }` → `{ ok: true }`.
- `send_message` — params `{ to: String, body: String }` → `{ message_id: i64 }`.
- `inbox` — params `{ mark_read: bool }` → list of `{ id: i64, from_session, from_nick, body, sent_at: i64 (unix ms) }`.
- `recent_messages` — params `{ limit: u32 }` → list of messages (sent + received).

There is no explicit disconnect RPC. The shim closes the socket on exit; the daemon's per-connection task detects EOF and removes the entry from the roster.

## Errors and edge cases

- **Daemon not running and auto-spawn fails:** every MCP tool call returns a clear error explaining how to start the daemon manually (`claude-coord-daemon --foreground`). The shim does not retry forever.
- **Daemon crashes mid-session:** shim's next tool call fails with `daemon disconnected`. Shim attempts one reconnect on the *following* call. No silent retry loops — the model needs to see that something went wrong.
- **Unknown recipient:** `send_message` returns `UnknownRecipient` with the current roster in the error body.
- **Ambiguous nickname:** `send_message` returns `AmbiguousRecipient` with the matching `session_id`s.
- **Oversized frame (> 1 MiB):** daemon rejects the frame and closes the connection. Tool body should be small notes, not file dumps.
- **SQLite errors:** logged to `~/.local/state/claude-coord/daemon.log`; surfaced as `Internal` RPC errors. No retry magic.
- **Two sessions with the same `session_id`:** can't happen during a daemon's lifetime (daemon assigns ids). Across daemon restarts, ids are regenerated; nothing relies on cross-restart stability in v1.

## Install and lifecycle

- `cargo install --path crates/shim --path crates/daemon` puts both binaries in `~/.cargo/bin`.
- Claude Code MCP entry: `command: "claude-coord-shim"`, no args. One entry, used by every session.
- Daemon auto-spawns on first shim connect (see "Session lifecycle" above).
- `claude-coord-daemon --foreground` runs the daemon attached to a terminal for debugging or for wrapping in a systemd user unit. A sample unit file ships in `contrib/systemd/` but is not required.
- Logs: `~/.local/state/claude-coord/daemon.log`, rotated at 10 MiB, keeping 3 files.

## Testing

- **`proto`:** roundtrip serde tests for every `ClientMsg` and `ServerMsg` variant; framing tests including partial reads and rejected oversized frames.
- **`daemon`:** integration tests that spin up the daemon on a temp socket and a `:memory:` SQLite, open multiple client connections, and exercise:
  - happy path: hello → roster → send → inbox → mark read.
  - failure paths: hello-twice, unknown recipient, ambiguous recipient, abrupt disconnect mid-RPC, oversized frame.
  - persistence: send while the recipient is offline (still within the same daemon — `to_session` is resolved at send time and `session_id`s do not survive a daemon restart in v1); recipient reconnects with the same id during the daemon's lifetime and reads the inbox.
  - schema durability: write to a tempfile DB, restart the daemon, confirm the rows are still there (delivery across daemon restarts is *not* a v1 guarantee — see Open questions).
- **`shim`:** unit tests for the MCP-call → daemon-RPC translation. One end-to-end test that boots `daemon` + `shim`, drives the shim over its stdio interface, and checks the MCP responses.
- No mocks for SQLite; use `:memory:` or a tempdir DB.

## Open questions / deferred

- Persistent session identity (so a crashed shim that reconnects keeps its inbox).
- Push-style notification of new messages while a session is idle (the `Event` plumbing exists for this).
- Capability tags / non-cwd roles.
- Locks and request/response semantics — if and when a real workflow demands them.
- Multi-machine / network transport.
