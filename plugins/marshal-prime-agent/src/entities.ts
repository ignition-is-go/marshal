// Marshal wire shapes for the pi extension.
//
// ALL marshal types come from the generated bindings in `./generated/`, which
// are produced from the canonical Rust `marshal-entities` definitions by
// `scripts/generate-pi-types.sh`. Regenerate them whenever the Rust entities
// change so the wire contract can't drift.
//
// What stays here:
//   - `MarshalCommand` / `MarshalQuery` — generic myko envelopes (not marshal-specific).
//   - builder functions — thin wrappers that return `{ commandId, command }`.
//   - `NotifyChannelMeta` — documents the runtime shape of the freeform JSON
//     `meta` field the daemon pushes (the Rust `serde_json::Value` has no type).
//   - `NOTIFY_CHANNEL_COMMAND_ID` — the push command id constant.

import type {
  AckMessagesResult,
  BroadcastMessageResult,
  JoinRoomResult,
  LeaveRoomResult,
  MessageView,
  ReadMessagesArgs,
  ReadMessagesResult,
  Room,
  RoomMember,
  SendMessageResult,
  Session,
  SessionNickname,
} from "./generated/index.ts";

// ── Re-exported generated types ──────────────────────────────────────────

export type {
  AckMessagesResult,
  BroadcastMessageResult,
  HostInfo,
  JoinRoomResult,
  LeaveRoomResult,
  MessageView,
  ReadMessagesResult,
  Room,
  RoomMember,
  SendMessageResult,
  Session,
  SessionNickname,
  SessionId,
  RoomId,
  MessageId,
} from "./generated/index.ts";

/** The roster-row payload this extension SETs. The generated `Session` type
 *  brands timestamps as `bigint` (ts-rs maps Rust `i64` that way); at runtime
 *  marshal serializes them as cbor numbers, so the daemon casts `Date.now()`
 *  values at construction. */
export type SessionItem = Session;

/** The daemon-assigned handle, read from the `SessionNickname` store. */
export type SessionNicknameItem = SessionNickname;

/** A room entry from `GetAllRooms`. */
export type RoomItem = Room;

/** A room membership from `GetAllRoomMembers`. */
export type RoomMemberItem = RoomMember;

// ── Server→client push meta ──────────────────────────────────────────────

/** The `NotifyChannel.meta` the daemon dispatches to the connected client
 *  when a peer sends a message. The Rust source defines it as `serde_json::Value`
 *  (freeform), so this is a convenience type, not a generated marshal one. */
export interface NotifyChannelMeta {
  source?: string;
  kind?: string;
  message_id?: string;
  from_session?: string;
  from_nickname?: string;
  to_session?: string;
  to_operator?: string | null;
  body?: string;
  body_truncated?: boolean;
  sent_at?: number;
}

export const NOTIFY_CHANNEL_COMMAND_ID = "NotifyChannel";

// ── Envelope types ───────────────────────────────────────────────────────

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

// ── Write command builders ───────────────────────────────────────────────

export function sendMessage(
  asSession: string,
  toSessionId: string,
  body: string,
): MarshalCommand<SendMessageResult> {
  return { commandId: "SendMessage", command: { toSessionId, body, asSession } };
}

export function broadcastMessage(
  asSession: string,
  toRoomId: string,
  body: string,
): MarshalCommand<BroadcastMessageResult> {
  return { commandId: "BroadcastMessage", command: { toRoomId, body, asSession } };
}

export function joinRoom(
  asSession: string,
  name: string,
  description?: string,
): MarshalCommand<JoinRoomResult> {
  return { commandId: "JoinRoom", command: stripUndefined({ name, description, asSession }) };
}

export function leaveRoom(
  asSession: string,
  room: string,
): MarshalCommand<LeaveRoomResult> {
  return { commandId: "LeaveRoom", command: { room, asSession } };
}

export function ackMessages(
  asSession: string,
  messageIds: string[],
): MarshalCommand<AckMessagesResult> {
  return { commandId: "AckMessages", command: { messageIds, asSession } };
}

/** `set_status` maps to the generated setter for `Session.current_task`
 *  (`Set{Item}{Field}` — myko/macros/src/item.rs). Payload is `{ id, currentTask }`. */
export function setSessionStatus(
  sessionId: string,
  currentTask: string | null,
): MarshalCommand<void> {
  return { commandId: "SetSessionCurrentTask", command: { id: sessionId, currentTask } };
}

/** Liveness bump — the generated setter for `Session.last_activity_at`. Mirrors
 *  the Rust shim's 5s roster-publish loop so the daemon sweeper doesn't reap us. */
export function setLastActivityAt(
  sessionId: string,
  lastActivityAt: number,
): MarshalCommand<void> {
  return { commandId: "SetSessionLastActivityAt", command: { id: sessionId, lastActivityAt } };
}

export function readMessages(args: ReadMessagesArgs): MarshalCommand<ReadMessagesResult> {
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

// ── Read query builders ──────────────────────────────────────────────────

export function getAllSessions(): MarshalQuery<Session> {
  return { queryId: "GetAllSessions", queryItemType: "Session", query: {} };
}

export function getAllRooms(): MarshalQuery<Room> {
  return { queryId: "GetAllRooms", queryItemType: "Room", query: {} };
}

export function getAllRoomMembers(): MarshalQuery<RoomMember> {
  return { queryId: "GetAllRoomMembers", queryItemType: "RoomMember", query: {} };
}

export function getAllSessionNicknames(): MarshalQuery<SessionNickname> {
  return { queryId: "GetAllSessionNicknames", queryItemType: "SessionNickname", query: {} };
}

function stripUndefined(o: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(o)) {
    if (v !== undefined) out[k] = v;
  }
  return out;
}