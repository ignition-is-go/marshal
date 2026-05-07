# marshal: rooms, broadcast, identity, host info

Proposal for the next feature wave on top of v0.1. Comments / edits welcome
before any of this gets implemented.

## Summary in one paragraph

Add three orthogonal layers on top of the current flat session-id mesh:

1. **Identity** on every session — which human it belongs to, and which host
   it's on. Both auto-detected, both overridable.
2. **Rooms** — optional named groupings of sessions. Auto-rooms are derived
   from identity (`host:laptop`, `op:trevor`, `project:marshal`); ad-hoc
   rooms are anything a user creates with `join_room("frontend-redesign")`.
3. **Broadcast** — a `broadcast(room, body)` tool that fans the message out
   to every member of the room, with per-recipient delivery results.

`send_message(to: session_id, body)` stays exactly as it is. Rooms and
broadcast are additive — anyone who never calls `join_room` keeps the
current 1:1 behaviour and pays nothing.

---

## 1. Identity additions to `Session`

Two new fields, both optional in JSON for forward/back-compat:

```rust
pub struct Session {
    // ... existing fields ...

    /// Which human this session belongs to. Auto-detected from
    /// $MARSHAL_OPERATOR, then $USER, then "anonymous". Surfaces on the
    /// roster so peers can tell apart "trevor's marshal session" from
    /// "alice's marshal session" on a shared box.
    pub operator: Option<String>,

    /// Host environment summary. Auto-populated by the shim at startup.
    pub host: Option<HostInfo>,
}

pub struct HostInfo {
    /// `gethostname()` result.
    pub name: String,
    /// "linux" / "macos" / "windows".
    pub os: String,
    /// "x86_64" / "aarch64".
    pub arch: String,
}
```

**Resolution order for `operator`:** `$MARSHAL_OPERATOR` → `$USER` →
`"anonymous"`. A user override matters when one human runs
multiple OS accounts, or when sessions actually run as a service user
(`claude-bot`, etc.) but logically belong to a person.

**Why no IP, no MAC, no machine fingerprint?** Privacy + spec creep. Add
them when there's a use case; the proposal stays minimal.

The roster output gains two columns; existing tools continue to work.

## 2. Rooms

### Entity model

```rust
#[myko_item]
pub struct Room {
    pub id: RoomId,             // RoomId = SmolStr — see "Naming" below
    pub name: String,           // human label, may equal id for ad-hoc rooms
    pub description: Option<String>,
    pub kind: RoomKind,         // Auto { auto_kind } | Adhoc
    pub created_at: i64,
}

pub enum RoomKind {
    Auto { source: AutoSource },  // Everyone | Host | Operator | Project { cwd }
    Adhoc,
}

pub enum AutoSource {
    Everyone,            // singleton, every live session
    Host,                // every session whose Session.host.name matches
    Operator,            // every session whose Session.operator matches
    Project { cwd: String },
}

#[myko_item]
pub struct RoomMember {
    pub id: RoomMemberId,         // composite of (room_id, session_id)
    #[belongs_to(Room)]
    pub room_id: RoomId,
    #[belongs_to(Session)]
    pub session_id: SessionId,
    pub joined_at: i64,
}

#[myko_item]
pub struct MessageRead {
    pub id: MessageReadId,        // composite of (message_id, session_id)
    #[belongs_to(Message)]
    pub message_id: MessageId,
    #[belongs_to(Session)]
    pub session_id: SessionId,
    pub read_at: i64,
}
```

Cascade DELs: a session leaving DELs its `RoomMember` and `MessageRead`
rows; a room being deleted DELs its memberships; a message being deleted
DELs its read-acks. (Messages currently aren't deleted ad-hoc, but this
keeps the relationship model consistent.)

### Polymorphic Message recipient

`Message.to_session_id` becomes one of two recipients — a peer session
(direct) or a room (broadcast):

```rust
pub struct Message {
    pub id: MessageId,
    pub from_session_id: SessionId,
    pub from_nick: String,
    /// Direct send target. Exactly one of `to_session_id` or
    /// `to_room_id` is set; serde defaults the absent side to None for
    /// forward/back-compat.
    pub to_session_id: Option<SessionId>,
    /// Broadcast target.
    pub to_room_id: Option<RoomId>,
    /// Nickname (for direct) or room name (for broadcast) at send time,
    /// denormalized so the display works after disconnect/rename.
    pub to_nick: String,
    pub body: String,
    pub sent_at: i64,
    // `read_at` is gone — read state lives on MessageRead instead, so
    // broadcasts can have per-recipient ack without ambiguity.
}
```

Read state is now uniform across direct and broadcast: a `MessageRead`
row marks "session X has read message Y", regardless of how Y was
addressed. A message with zero `MessageRead` rows is unread by everyone;
a 1:1 message gets at most one row; a broadcast can have up to N (one
per room member who's caught up).

### Auto-rooms (derived from identity)

When a session SETs/re-SETs, the daemon's `AutoRoomSaga`:

- Ensures `everyone` exists (singleton, never DEL'd); auto-joins this session.
- Ensures `host:<name>` exists; auto-joins.
- Ensures `op:<operator>` exists; auto-joins.
- If `cwd` is inside a git repo, ensures `project:<repo-basename>` exists;
  auto-joins.

`everyone`, `host:`, `op:`, `project:` are first-class routes — you can
`broadcast("everyone", "...")`, `broadcast("op:trevor", "...")`,
`broadcast("project:marshal", "...")` exactly like ad-hoc rooms. The
identity fields drive membership; the rooms are just the addressable
view of "all sessions sharing this identity attribute".

These auto-rooms are tagged `RoomKind::Auto { source }` so the roster /
TUI can surface them differently from ad-hoc rooms (icon, dim color),
and so cleanup can sweep them when the last member leaves — except
`everyone`, which is always present.

### Naming and id rules

- Singleton: `everyone`. Always exists, every live session is a member,
  the cleanup sweeper never DELs it.
- Other auto-room ids are `host:<name>`, `op:<operator>`,
  `project:<basename>`. Stable, recomputable from a session's identity.
- Ad-hoc room ids are slugified from the user-supplied name (lowercase,
  `[a-z0-9-]+`), with `-{N}` suffix on collision (same dedup trick we
  use for nicknames).
- Reserved names + prefixes: `everyone`, `host:`, `op:`, `project:`.
  `join_room("host:foo")` and `join_room("everyone")` error loudly so
  users can't shadow auto-rooms.

### Tool surface

```text
join_room(name, description?)        -> { room_id, joined: true|already }
leave_room(room_id_or_name)          -> { ok: true }
list_rooms()                         -> { rooms: [
    { room_id, name, description?, kind, members:
        [{ session_id, nickname, operator?, host? }] } ] }
```

Plus the roster grows a per-session `rooms: [room_id]` field so peers can
see what rooms a session is in without a separate query.

### Lifecycle

- A room with `kind = Adhoc` is DEL'd by the cleanup sweeper if its
  membership count drops to zero and stays there for `STALE_AFTER` (10s,
  same as session sweep).
- Auto-rooms persist until the session that anchors them disconnects
  (and again, sweeper handles that).

## 3. Broadcast

```text
broadcast(to_room, body) -> {
  message_id,
  delivered: [{ session_id, to_nick }],
  failed:    [{ session_id, reason }],
  total: usize,
}
```

Server-side `Broadcast` command:

1. Resolve sender from `ctx.client_id()` (same path as `SendMessage`).
2. Resolve recipient set: every `RoomMember` of `to_room` except the
   sender. **If the recipient set is empty** (room exists but the
   sender is alone, or room doesn't exist), return a `CommandError`
   with a clear message — same fail-loud contract as
   `SendMessage` for an unknown recipient. Empty broadcasts are
   almost always a user error (wrong room id, forgot to `join_room`,
   typo) and should surface, not silently no-op.
3. For each recipient, run the same validation `SendMessage` does
   (recipient's `client_id` is in the live `client_registry`) and call
   the same `dispatch_notify_channel` — wire-level delivery is
   identical to a 1:1 send, just iterated.
4. Persist exactly one `Message` row with `to_room_id = Some(room)` and
   `to_session_id = None`. Per-recipient read state is the absence
   or presence of a `MessageRead { message_id, session_id }` row —
   no rows means unread by everyone.
5. Aggregate per-recipient outcomes into `delivered` / `failed`; return
   the single `message_id` plus both lists. As long as the room had
   recipients, the call returns `Ok(BroadcastResult)` even if every
   per-recipient dispatch failed — the caller looks at `failed`
   for transient issues.

Two-tier failure semantics:

- **Empty room → `CommandError`** (loud, addressable user error).
- **Partial delivery → `Ok` with `failed` list** (fail-soft on
  transient stale bindings; the Message persists either way and the
  durable record is "this was sent to the room"; live deliveries
  are for immediate UX, not history).

## 4. Reading messages

A single `read_messages` tool covers every access pattern with
composable filters. Default (no args) returns the N most recent
messages visible to this session.

```text
read_messages({
  room?:       "everyone" | "host:..." | "op:..." | "project:..." | <adhoc-id>,
  from?:       <session_id>,
  to?:         <session_id | room_id>,   // polymorphic; server figures out
  inbox?:      bool,    // addressed to me (direct or via room membership)
  sent?:       bool,    // sent by me
  unread?:     bool,    // I haven't acked yet (no MessageRead row for me)
  since?:      i64,     // sent_at >= since (millis)
  limit?:      u32,     // default 50, max 500
  mark_read?:  bool,    // create MessageRead rows for the fetched set
}) -> {
  messages: [{
    message_id,
    from_session_id, from_nick,
    to_session_id?, to_room_id?, to_nick,
    body, sent_at,
    read_by_me: bool,
  }],
  total_matched: u32,    // before `limit` truncation
}
```

### Visibility rules

A session sees a `Message` if any of:

- It sent it (`from_session_id` matches).
- It's the direct recipient (`to_session_id` matches).
- The message is addressed to a room (`to_room_id` set) and the session
  is a current `RoomMember` of that room. Members see messages sent
  while they were in the room; if they leave and rejoin, the historical
  cut is at their first `joined_at` — keeps "I just joined, what was
  said before I existed?" out of the inbox by default.

`everyone` is a member every live session belongs to, so global
broadcasts are universally visible.

### Filter examples

```text
read_messages({})                          // recent in your inbox + sent
read_messages({ inbox: true, unread: true })   // your unread inbox
read_messages({ room: "project:marshal" })     // everything in this project
read_messages({ from: "<peer-session-id>" })   // 1:1 thread + their broadcasts
read_messages({ to: "op:trevor" })             // anything addressed to trevor's ops
read_messages({ sent: true })                  // your outbox
read_messages({ inbox: true, mark_read: true })// pull-and-ack pattern
```

### Implementation

`ReadMessages` is a server-side command (not a watch — it's a one-shot
fetch). Filters AND together. Server applies visibility checks first so
unauthorised access can't leak via crafted filters. `mark_read` writes
`MessageRead` rows in the same transaction as the read; idempotent if
the row already exists.

This tool is **after-the-fact only** — backlog, history, recap,
catch-up after disconnect, bulk-ack flows. Live notifications still
flow through `notifications/claude/channel` as messages arrive; you
don't poll `read_messages` to listen.

## 5. Open questions

### Resolved

1. **Reserved prefixes (`everyone`, `host:`, `op:`, `project:`)** —
   blocked. `join_room("host:foo")` errors loudly.
2. **Auto-rooms opt-out** — none. Auto-rooms are always on. (We can
   revisit if real privacy concerns surface; out of scope for v1.)
3. **`everyone` room** — yes. Singleton auto-room every session joins.
   `broadcast("everyone", "...")` reaches every live peer in one
   server call.
4. **Read state per-broadcast-recipient** — option (b): one `Message`
   row + a `MessageRead { message_id, session_id, read_at }` join
   table. Unifies read state across direct and broadcast.
5. **Operator + project as routes** — yes. Auto-rooms `op:<operator>`
   and `project:<basename>` are first-class addressable rooms; the
   identity attribute drives membership.
7. **Empty broadcast** — `Broadcast` returns a `CommandError` if the
   resolved recipient set is empty (no other room members). Stale
   per-recipient bindings still fail-soft within a non-empty
   broadcast.

### Still open

6. **Subscribe-to-room** — should listing or watching a room produce a
   live stream of new messages, or do we keep delivery purely
   push-on-send? Tentative: push-on-send only — rooms are routing
   scope, not topics. Existing `notifications/claude/channel` already
   covers live streams; `read_messages` covers after-the-fact pulls.

## 6. Migration / back-compat

- All new fields on `Session` are `Option<...>` with `serde(default)`.
  Old `events.jsonl` rows replay cleanly into the new schema.
- `Message.to_session_id` becomes `Option<SessionId>` and gains a sibling
  `Option<RoomId>`. Old persisted Messages have `to_session_id = Some(_)`
  and `to_room_id = None`; serde defaults handle replay.
- `Message.read_at` is removed in favour of `MessageRead` rows. Replay
  of old Messages: ignore the legacy `read_at` field; if it was set,
  emit a `MessageRead` row for the recipient at that timestamp during
  the migration pass (one-shot at startup).
- New entities (`Room`, `RoomMember`, `MessageRead`) are additive.
- New tools (`join_room`, `leave_room`, `list_rooms`, `broadcast`,
  `read_messages`) are additive. The `roster()` shape gains optional
  fields.
- `send_message` is unchanged at the tool surface; internally it now
  populates the new polymorphic Message shape with `to_room_id = None`.

The bump that lands this is a minor (`0.2.0`), not a major.

## 7. Build sequence

If we go ahead:

1. Identity fields on `Session` + shim auto-detection — small, isolated.
2. Polymorphic `Message` recipient (`to_session_id` → Option, add
   `to_room_id`); replace `read_at` with `MessageRead` entity +
   one-shot replay migration.
3. `Room`/`RoomMember` entities + the four room tools
   (`join_room`, `leave_room`, `list_rooms`, plus `roster()` showing
   memberships).
4. `AutoRoomSaga` to auto-join `everyone` / host / op / project —
   depends on #1 and #3.
5. `Broadcast` command — depends on #2 and #3.
6. `ReadMessages` command — depends on #2; useful from the moment it
   lands even before broadcast does.
7. TUI + web display of rooms + operator + host + per-recipient read
   acks — UI follow-up.

Each step is independently shippable behind the existing `feat:`
release flow.
