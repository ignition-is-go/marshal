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
```

`RoomMember` has cascade DEL on both sides — a session leaving DELs its
membership rows; a room being deleted DELs all its memberships.

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
  message_ids: [{ session_id, message_id, to_nick }],
  failed:      [{ session_id, reason }],
  total: usize,
}
```

Server-side `Broadcast` command:

1. Resolve sender from `ctx.client_id()` (same path as `SendMessage`).
2. Resolve recipient set: every `RoomMember` of `to_room` except the sender.
3. For each recipient, run the same validation `SendMessage` does
   (recipient's `client_id` is in the live `client_registry`).
4. Live-push first, persist on success — same contract as `SendMessage`,
   but per-recipient. A delivery that fails goes into `failed`, not
   `message_ids`. The whole call still returns `Ok(BroadcastResult)`; the
   caller's responsibility to look at `failed`.
5. The persisted Messages all carry the same `from_*` and the same
   `room_id` in their meta so a future `recent_messages_in_room` query
   can reconstruct the broadcast.

This is intentionally fail-soft: a broadcast to 10 sessions where 2 are
disconnected lands for the 8 live ones and reports the 2 misses. The
alternative (any failure aborts the whole broadcast) makes a stale
session in the room block all communication, which we don't want.

## 4. Open questions

### Resolved

1. **Reserved prefixes (`everyone`, `host:`, `op:`, `project:`)** —
   blocked. `join_room("host:foo")` errors loudly.
2. **Auto-rooms opt-out** — none. Auto-rooms are always on. (We can
   revisit if real privacy concerns surface; out of scope for v1.)
3. **`everyone` room** — yes. Singleton auto-room every session
   joins. `broadcast("everyone", "...")` reaches every live peer in
   one server call.
5. **Operator + project as routes** — yes. Auto-rooms `op:<operator>`
   and `project:<basename>` are first-class addressable rooms; the
   identity attribute drives membership.

### Still open

4. **Read state per-broadcast-recipient** — current `Message.read_at` is
   a single bool. For broadcasts that becomes ambiguous (read by whom?).
   Options: (a) one `Message` per recipient (current default in proposal),
   each with its own `read_at`; (b) one `Message` with a separate
   `MessageRead { message_id, session_id, read_at }` join table.
   (a) is simpler, (b) saves bytes for huge broadcasts. Tentative: (a).
6. **Subscribe-to-room** — should listing or watching a room produce a
   live stream of new messages, or do we keep delivery purely
   push-on-send? Tentative: push-on-send only — rooms are routing
   scope, not topics. Existing `notifications/claude/channel` already
   covers live streams.

## 5. Migration / back-compat

- All new fields on `Session` are `Option<...>` with `serde(default)`.
  Old `events.jsonl` rows replay cleanly into the new schema.
- New entities (`Room`, `RoomMember`) are additive. Old shims that don't
  know about rooms can still send and receive 1:1 messages.
- New tools (`join_room`, `leave_room`, `list_rooms`, `broadcast`) are
  additive. The `roster()` shape gains optional fields.
- `send_message` is unchanged.

The bump that lands this is a minor (`0.2.0`), not a major.

## 6. Build sequence

If we go ahead:

1. Identity fields on `Session` + shim auto-detection — small, isolated.
2. `Room`/`RoomMember` entities + the four room tools — most of the work.
3. `AutoRoomSaga` to auto-join host/op/project — depends on #1 and #2.
4. `Broadcast` command — depends on #2.
5. TUI + web display of rooms + operator + host — UI follow-up.

Each step is independently shippable behind the existing `feat:`
release flow.
