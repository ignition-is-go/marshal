// Wire shapes for marshal's myko entities, hand-mirrored from the Rust
// `marshal-entities` crate. This is the ONE place the opencode plugin
// encodes marshal's command/query/event contract — everything else goes
// through these builders so a wire change is a single-file edit.
//
// Conventions pulled from the Rust source (verified, not guessed):
//   - myko derives `commandId` / `queryId` from the struct name verbatim
//     (`stringify!(StructName)` — myko/macros/src/command.rs, query.rs).
//   - a query's `queryItemType` is the item struct name
//     (`GetAllSessions` → item `Session`).
//   - every entity serializes `camelCase` (`#[serde(rename_all=camelCase)]`).
//   - `SessionId` / `RoomId` / `MessageId` are newtype wrappers over a
//     string, so they serialize as bare strings.
//   - write commands carry caller identity in `asSession` (the connection
//     has no per-session client_id binding for our shared WS — every
//     command names the acting session explicitly). See
//     marshal-entities/src/session.rs::resolve_caller.
//
// Drift-safety: these shapes are guarded by the real-daemon integration test
// (test/integration.test.ts), which round-trips every command/query against a
// live marshal-daemon — rename a field in the Rust `marshal-entities` source
// and the wire round-trip fails. That end-to-end guard is why this stays a
// small hand-mirror rather than generated code. (marshal-entities CAN emit
// these as TS via `cargo run -p marshal-entities --features codegen --bin
// typegen` if a future consumer wants the full static surface, but ts-rs dumps
// the whole linked graph incl. myko's base types, so wiring that into this
// 6-verb plugin wasn't worth the build pipeline.)

/** A myko command envelope as `MykoClient.sendCommand` expects it. */
export interface MarshalCommand<_Result = unknown> {
  readonly commandId: string;
  readonly command: Record<string, unknown>;
}

/** A myko query envelope as `MykoClient.watchQuery` expects it. */
export interface MarshalQuery<_Item = unknown> {
  readonly queryId: string;
  readonly queryItemType: string;
  readonly query: Record<string, unknown>;
}

// ── Session entity (roster row) ──────────────────────────────────────────

export interface HostInfo {
  name: string;
  os: string;
  arch: string;
}

/** The `Session` item payload SET onto the roster. `clientId` is omitted —
 *  the server populates it from the live WS connection. */
export interface SessionItem {
  id: string;
  pid: number;
  cwd: string;
  connectedAt: number;
  gitBranch?: string;
  currentTask?: string;
  lastActivityAt?: number;
  operator?: string;
  host?: HostInfo;
  project?: string;
}

// ── Write commands ───────────────────────────────────────────────────────

export function sendMessage(asSession: string, toSessionId: string, body: string): MarshalCommand<SendMessageResult> {
  return { commandId: "SendMessage", command: { toSessionId, body, asSession } };
}

export function broadcastMessage(asSession: string, toRoomId: string, body: string): MarshalCommand<BroadcastMessageResult> {
  return { commandId: "BroadcastMessage", command: { toRoomId, body, asSession } };
}

export function joinRoom(asSession: string, name: string, description?: string): MarshalCommand<JoinRoomResult> {
  return { commandId: "JoinRoom", command: stripUndefined({ name, description, asSession }) };
}

export function leaveRoom(asSession: string, room: string): MarshalCommand<LeaveRoomResult> {
  return { commandId: "LeaveRoom", command: { room, asSession } };
}

export function ackMessages(asSession: string, messageIds: string[]): MarshalCommand<AckMessagesResult> {
  return { commandId: "AckMessages", command: { messageIds, asSession } };
}

/** `set_status` maps to the generated setter command for `Session.current_task`
 *  (`Set{Item}{Field}` — myko/macros/src/item.rs). Payload is `{ id, currentTask }`. */
export function setSessionStatus(sessionId: string, currentTask: string | null): MarshalCommand<void> {
  return { commandId: "SetSessionCurrentTask", command: { id: sessionId, currentTask } };
}

/** Liveness bump — the generated setter for `Session.last_activity_at`. Mirrors
 *  the Rust shim's 5s roster-publish loop so the daemon sweeper doesn't reap us. */
export function setLastActivityAt(sessionId: string, lastActivityAt: number): MarshalCommand<void> {
  return { commandId: "SetSessionLastActivityAt", command: { id: sessionId, lastActivityAt } };
}

export interface ReadMessagesArgs {
  asSession: string;
  inbox?: boolean;
  sent?: boolean;
  unread?: boolean;
  room?: string;
  from?: string;
  toSession?: string;
  since?: number;
  limit?: number;
}

export function readMessages(args: ReadMessagesArgs): MarshalCommand<ReadMessagesResult> {
  // `inbox`/`sent`/`unread` are non-Option bools on the Rust side — always send them.
  const command = stripUndefined({
    asSession: args.asSession,
    inbox: args.inbox ?? false,
    sent: args.sent ?? false,
    unread: args.unread ?? false,
    room: args.room,
    from: args.from,
    toSession: args.toSession,
    since: args.since,
    limit: args.limit,
  });
  return { commandId: "ReadMessages", command };
}

// ── Read queries ─────────────────────────────────────────────────────────

export function getAllSessions(): MarshalQuery<SessionItem> {
  return { queryId: "GetAllSessions", queryItemType: "Session", query: {} };
}

export function getAllRooms(): MarshalQuery<RoomItem> {
  return { queryId: "GetAllRooms", queryItemType: "Room", query: {} };
}

export function getAllRoomMembers(): MarshalQuery<RoomMemberItem> {
  return { queryId: "GetAllRoomMembers", queryItemType: "RoomMember", query: {} };
}

/** The daemon-assigned, frozen handle for a session (id = the session id). The
 *  daemon owns the wordlists + assignment now, so the plugin READS this instead
 *  of recomputing — a wordlist change can never desync it from the roster. */
export interface SessionNicknameItem {
  id: string;
  nickname: string;
}

export function getAllSessionNicknames(): MarshalQuery<SessionNicknameItem> {
  return { queryId: "GetAllSessionNicknames", queryItemType: "SessionNickname", query: {} };
}

// ── Command/query result shapes (the fields the plugin actually reads) ────

export interface SendMessageResult {
  messageId: string;
  toSessionId: string;
  sentAt: number;
  /** Optional only for compatibility with daemons older than v0.45.0. */
  livePush?: "unknown" | "delivered" | "unavailable" | "failed";
  /** Optional only for compatibility with daemons older than v0.45.0. */
  wake?: "not_needed" | "unobserved";
  /** Compatibility alias for livePush === "delivered"; not wake status. */
  deliveredLive: boolean;
}

export interface BroadcastMessageResult {
  messageId: string;
  toRoomId: string;
  toRoomName: string;
  sentAt: number;
  total: number;
  delivered: Array<{ sessionId: string }>;
  failed: Array<{ sessionId: string; reason: string }>;
}

export interface JoinRoomResult {
  roomId: string;
  name: string;
  joined: boolean;
  created: boolean;
}

export interface LeaveRoomResult {
  roomId: string;
  left: boolean;
}

export interface AckMessagesResult {
  newlyAcked: number;
  alreadyAcked: number;
}

export interface MessageView {
  messageId: string;
  fromSessionId: string;
  toSessionId?: string;
  toRoomId?: string;
  body: string;
  sentAt: number;
  readByMe: boolean;
}

export interface ReadMessagesResult {
  messages: MessageView[];
  totalMatched: number;
}

export interface RoomItem {
  id: string;
  name: string;
}

export interface RoomMemberItem {
  id: string;
  roomId: string;
  sessionId: string;
}

// ── The server→client push command we consume for live delivery ──────────

/** `NotifyChannel` is marshal's server→client push (a peer sent us a message).
 *  The Rust shim turns it into a `notifications/claude/channel` MCP event;
 *  opencode has no such channel, so we consume it directly off the WS and
 *  surface it as a TUI toast. */
export const NOTIFY_CHANNEL_COMMAND_ID = "NotifyChannel";

export interface NotifyChannelMeta {
  source?: string;
  kind?: string;
  from_session?: string;
  from_nickname?: string;
  to_session?: string;
  body?: string;
  sent_at?: number;
}

function stripUndefined(o: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(o)) {
    if (v !== undefined) out[k] = v;
  }
  return out;
}
