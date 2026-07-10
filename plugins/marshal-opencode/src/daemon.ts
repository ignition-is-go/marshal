// MarshalDaemon — the opencode-side equivalent of the Rust marshal-shim's
// daemon wiring, built on the @myko/core TypeScript client.
//
// Responsibilities (1:1 with the shim's main.rs, minus the Claude-specific
// session-id archaeology and the MCP stdio transport):
//   - hold one autoreconnecting WS to the marshal daemon,
//   - SET a `Session` roster row per live opencode session and keep its
//     liveness fresh (5s cadence, like the shim's roster-publish loop),
//   - re-SET every tracked session on reconnect (the daemon's roster is
//     in-memory and lost on restart),
//   - run the write commands (send/broadcast/join/leave/status/ack) and the
//     per-turn inbox pull, all carrying `asSession` for caller identity,
//   - surface inbound `NotifyChannel` pushes to a callback (→ TUI toast).
//
// Unlike Claude Code, opencode hands the plugin the session id directly, so
// the entire parent-PID / ~/.claude / ctime discovery stack the shim needs
// simply doesn't exist here.

import { ConnectionStatus, MykoClient } from "@myko/core";
import type { MEvent } from "@myko/core";

import {
  ackMessages,
  broadcastMessage,
  getAllSessions,
  getAllSessionNicknames,
  joinRoom,
  leaveRoom,
  NOTIFY_CHANNEL_COMMAND_ID,
  readMessages,
  sendMessage,
  setLastActivityAt,
  setSessionStatus,
  type BroadcastMessageResult,
  type JoinRoomResult,
  type LeaveRoomResult,
  type MarshalCommand,
  type MarshalQuery,
  type MessageView,
  type NotifyChannelMeta,
  type ReadMessagesResult,
  type SendMessageResult,
  type SessionItem,
  type SessionNicknameItem,
} from "./entities.js";
import type { Identity } from "./identity.js";

const LIVENESS_INTERVAL_MS = 5_000;
const INBOX_PULL_LIMIT = 20;
const SOURCE_ID = "marshal-opencode";

/** myko's CellServer serves its WebSocket at the `/myko` path. The fleet (and
 *  the Rust shim) configure the daemon address WITHOUT that path — bare
 *  `ws://host:6155` — and the Rust client appends it internally. The @myko/core
 *  TS client opens the address verbatim, so we append `/myko` ourselves when
 *  the configured address carries no path. Idempotent if it's already there. */
export function withMykoPath(addr: string): string {
  try {
    const u = new URL(addr);
    if (u.pathname === "" || u.pathname === "/") u.pathname = "/myko";
    return u.toString().replace(/\/$/, "");
  } catch {
    return /\/myko\/?$/.test(addr) ? addr : `${addr.replace(/\/+$/, "")}/myko`;
  }
}

export interface DaemonConfig {
  address: string;
  cwd: string;
  identity: Identity;
  log?: (msg: string) => void;
}

type NotifyHandler = (meta: NotifyChannelMeta) => void;

export class MarshalDaemon {
  private readonly client = new MykoClient();
  private readonly cfg: DaemonConfig;
  private readonly log: (msg: string) => void;

  /** One roster row per live opencode session id. A single opencode process
   *  can host several sessions; they share this one WS (and thus one server
   *  client_id), and inbound pushes are routed by `meta.to_session`. */
  private readonly sessions = new Map<string, SessionItem>();

  /** Latest roster snapshot, kept warm by a standing query subscription so
   *  inbox rendering can label senders without a per-turn fetch. */
  private roster: SessionItem[] = [];

  /** Daemon-assigned handles, kept warm the same way. Read instead of
   *  recomputing, so a wordlist change never desyncs us from the roster. */
  private nicknames: SessionNicknameItem[] = [];

  private notifyHandler: NotifyHandler | null = null;
  private livenessTimer: ReturnType<typeof setInterval> | null = null;
  private started = false;

  constructor(cfg: DaemonConfig) {
    this.cfg = cfg;
    this.log = cfg.log ?? (() => {});
  }

  /** Open the connection and wire reconnect + push handling. Idempotent. */
  start(): void {
    if (this.started) return;
    this.started = true;

    // Re-SET every tracked session whenever a connection (re)establishes —
    // the daemon's roster is in-memory and a restart drops every row.
    this.client.connectionStatus$.subscribe((status) => {
      if (status === ConnectionStatus.Connected) {
        this.log(`connected to ${this.cfg.address}; re-publishing ${this.sessions.size} session(s)`);
        for (const item of this.sessions.values()) this.emitSet(item);
      } else if (status === ConnectionStatus.Disconnected) {
        this.log("disconnected from daemon");
      }
    });

    // Real-time inbound: the daemon pushes `NotifyChannel` at our WS when a
    // peer messages one of our sessions. opencode has no in-context push, so
    // we hand it to the toast callback; the authoritative in-context delivery
    // is the per-turn inbox pull (drainInbox).
    this.client.commandIncoming$.subscribe((msg) => {
      // Server-pushed command. The wire shape (verified on render-16) is
      // `{ event:"ws:m:command", data:{ commandId, command:{ content, meta } } }`.
      const data = (msg as { data?: { commandId?: string; command?: unknown } }).data;
      if (!data || data.commandId !== NOTIFY_CHANNEL_COMMAND_ID) return;
      const command = (data.command ?? {}) as { meta?: NotifyChannelMeta };
      this.notifyHandler?.(command.meta ?? {});
    });

    // Keep a warm roster snapshot for sender labelling.
    this.watch(getAllSessions()).subscribe((items) => {
      this.roster = items;
    });
    // …and the daemon-assigned handles, read wherever we show a nickname.
    this.watch(getAllSessionNicknames()).subscribe((items) => {
      this.nicknames = items;
    });

    this.client.setAddress(withMykoPath(this.cfg.address));
  }

  onNotify(handler: NotifyHandler): void {
    this.notifyHandler = handler;
  }

  /** Tear down: DEL every roster row, stop the heartbeat, and disconnect.
   *  Used on plugin dispose and in tests. */
  stop(): void {
    for (const item of this.sessions.values()) this.emitDel(item);
    this.sessions.clear();
    if (this.livenessTimer) {
      clearInterval(this.livenessTimer);
      this.livenessTimer = null;
    }
    this.client.disconnect();
    this.started = false;
  }

  /** SET (or refresh) the roster row for an opencode session. Idempotent —
   *  safe to call on every turn to self-heal a dropped row. */
  registerSession(sessionId: string): void {
    const existing = this.sessions.get(sessionId);
    const now = Date.now();
    const item: SessionItem = existing ?? {
      id: sessionId,
      pid: process.pid,
      cwd: this.cfg.cwd,
      connectedAt: now,
      operator: this.cfg.identity.operator,
      host: this.cfg.identity.host,
      gitBranch: this.cfg.identity.gitBranch,
      project: this.cfg.identity.project,
    };
    item.lastActivityAt = now;
    this.sessions.set(sessionId, item);
    this.emitSet(item);
    this.ensureLivenessTimer();
  }

  /** Bump liveness for a session without a full re-SET — cheap setter command. */
  bumpLiveness(sessionId: string): void {
    const item = this.sessions.get(sessionId);
    if (!item) return;
    item.lastActivityAt = Date.now();
    void this.send(setLastActivityAt(sessionId, item.lastActivityAt));
  }

  /** DEL the roster row when an opencode session ends. */
  deregisterSession(sessionId: string): void {
    const item = this.sessions.get(sessionId);
    if (!item) return;
    this.sessions.delete(sessionId);
    this.emitDel(item);
    if (this.sessions.size === 0 && this.livenessTimer) {
      clearInterval(this.livenessTimer);
      this.livenessTimer = null;
    }
  }

  // ── Write tools (each carries asSession = the acting session) ────────────

  sendMessage(asSession: string, to: string, body: string): Promise<SendMessageResult> {
    return this.send(sendMessage(asSession, to, body));
  }

  broadcast(asSession: string, room: string, body: string): Promise<BroadcastMessageResult> {
    return this.send(broadcastMessage(asSession, room, body));
  }

  joinRoom(asSession: string, name: string, description?: string): Promise<JoinRoomResult> {
    return this.send(joinRoom(asSession, name, description));
  }

  leaveRoom(asSession: string, room: string): Promise<LeaveRoomResult> {
    return this.send(leaveRoom(asSession, room));
  }

  setStatus(sessionId: string, text: string): Promise<void> {
    const item = this.sessions.get(sessionId);
    if (item) item.currentTask = text;
    return this.send(setSessionStatus(sessionId, text));
  }

  // ── Reads ────────────────────────────────────────────────────────────────

  async roster_snapshot(): Promise<SessionItem[]> {
    // Prefer the warm cache; fall back to a one-shot if it hasn't filled yet.
    if (this.roster.length > 0) return this.roster;
    return new Promise<SessionItem[]>((resolve) => {
      let done = false;
      const sub = this.watch(getAllSessions()).subscribe((items) => {
        if (done) return;
        done = true;
        resolve(items);
        queueMicrotask(() => sub.unsubscribe());
      });
    });
  }

  /** Pull unread inbox messages for a session, ack them, and return a rendered
   *  `<marshal_inbox>` block ready to inject — or null if the inbox is empty.
   *  This is the per-turn delivery path, byte-for-byte the same daemon
   *  semantics as Claude Code's UserPromptSubmit hook (ReadMessages →
   *  AckMessages), just driven from opencode's chat hook instead of curl. */
  async drainInbox(sessionId: string): Promise<string | null> {
    let result: ReadMessagesResult;
    try {
      // DIRECT-ONLY auto-inject: `toSession` (messages addressed to me
      // directly), NOT room broadcasts. A broadcast is ambient — surfaced in
      // the marshal UI / an explicit `read_messages room=…`, never auto-injected
      // into the turn. Mirrors the daemon hook's surface_unread change.
      result = await this.send(
        readMessages({ asSession: sessionId, toSession: sessionId, inbox: false, sent: false, unread: true, limit: INBOX_PULL_LIMIT }),
      );
    } catch (e) {
      this.log(`inbox pull failed: ${String(e)}`);
      return null;
    }
    if (!result.messages || result.messages.length === 0) return null;

    const block = this.renderInbox(result.messages);

    try {
      await this.send(ackMessages(sessionId, result.messages.map((m) => m.messageId)));
    } catch (e) {
      this.log(`inbox ack failed (will re-surface next turn): ${String(e)}`);
    }
    return block;
  }

  // ── internals ──────────────────────────────────────────────────────────

  /** A clean, tagless notification — mirrors Claude Code's live channel push
   *  (`new message from <nickname>`), with the body inline since opencode renders
   *  it as plain conversation text. The untrusted-peer + how-to-reply framing is
   *  NOT repeated here; it lives once in the system prompt (see index.ts's
   *  experimental.chat.system.transform), exactly like Claude's MCP instructions. */
  private renderInbox(messages: MessageView[]): string {
    if (messages.length === 1) {
      const m = messages[0];
      return `new message from ${this.senderLabel(m.fromSessionId)}: ${m.body}`;
    }
    const lines = [`${messages.length} new messages from sibling agents:`];
    for (const m of messages) {
      lines.push(`- ${this.senderLabel(m.fromSessionId)}: ${m.body}`);
    }
    return lines.join("\n");
  }

  /** The sender's friendly handle: `nickname (operator@host)` — the nickname is
   *  how you address them back (send_message resolves it), so the raw session id
   *  is dropped. Falls back to just the nickname when the roster row isn't warm. */
  /** The session's daemon-assigned handle, from the warm cache. Falls back to
   *  the raw session id for the brief window before the daemon has assigned one
   *  (near-instant on a session's first SET). */
  nicknameFor(sessionId: string): string {
    return this.nicknames.find((n) => n.id === sessionId)?.nickname ?? sessionId;
  }

  private senderLabel(sessionId: string): string {
    const nick = this.nicknameFor(sessionId);
    const s = this.roster.find((r) => r.id === sessionId);
    if (!s) return nick;
    const ctx = [s.operator, s.host?.name].filter(Boolean).join("@");
    return ctx ? `${nick} (${ctx})` : nick;
  }

  private ensureLivenessTimer(): void {
    if (this.livenessTimer) return;
    this.livenessTimer = setInterval(() => {
      const now = Date.now();
      for (const [id, item] of this.sessions) {
        item.lastActivityAt = now;
        void this.send(setLastActivityAt(id, now));
      }
    }, LIVENESS_INTERVAL_MS);
    // Don't keep the opencode process alive solely for our heartbeat.
    if (typeof (this.livenessTimer as { unref?: () => void }).unref === "function") {
      (this.livenessTimer as { unref?: () => void }).unref!();
    }
  }

  private emitSet(item: SessionItem): void {
    this.client.sendEvent(this.event(item, "SET"));
  }

  private emitDel(item: SessionItem): void {
    this.client.sendEvent(this.event(item, "DEL"));
  }

  private event(item: SessionItem, changeType: "SET" | "DEL"): MEvent {
    // Matches Rust MEvent (camelCase): item / changeType / itemType / createdAt
    // / tx / sourceId (myko/core/src/wire/event/mod.rs).
    return {
      item: item as unknown as Record<string, unknown>,
      changeType,
      itemType: "Session",
      createdAt: new Date().toISOString(),
      tx: crypto.randomUUID(),
      sourceId: SOURCE_ID,
    } as unknown as MEvent;
  }

  private send<R>(command: MarshalCommand<R>): Promise<R> {
    return this.client.sendCommand(command as never) as Promise<R>;
  }

  private watch<I>(query: MarshalQuery<I>) {
    return this.client.watchQuery(query as never) as unknown as import("rxjs").Observable<I[]>;
  }
}
