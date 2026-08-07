// MarshalDaemon — the pi-side equivalent of the Rust marshal-shim's daemon
// wiring, built on the @myko/core TypeScript client (official myko protocol).
//
// Responsibilities (1:1 with the shim's main.rs, minus the Claude-specific
// session-id archaeology and the MCP stdio transport):
//   - hold one autoreconnecting WS to the marshal daemon,
//   - SET a `Session` roster row per live pi session and keep its liveness
//     fresh (5s cadence, like the shim's roster-publish loop),
//   - re-SET every tracked session on reconnect (the daemon's roster is
//     in-memory and lost on restart),
//   - run the write commands (send/broadcast/join/leave/status/ack) and the
//     per-turn inbox pull, all carrying `asSession` for caller identity,
//   - surface inbound `NotifyChannel` pushes to a callback (→ turn injection).

import { randomUUID } from "node:crypto";
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
} from "./entities.ts";
import type { Identity } from "./identity.ts";

const LIVENESS_INTERVAL_MS = 5_000;
const INBOX_PULL_LIMIT = 20;
const SOURCE_ID = "marshal-pi";

/** myko's CellServer serves its WebSocket at the `/myko` path. */
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

  private readonly sessions = new Map<string, SessionItem>();
  private readonly communicated = new Map<string, Set<string>>();
  private lastSessionId: string | undefined;

  private roster: SessionItem[] = [];
  private nicknames: SessionNicknameItem[] = [];

  private readonly draining = new Map<string, Promise<string | null>>();
  private notifyHandler: NotifyHandler | null = null;
  private livenessTimer: ReturnType<typeof setInterval> | null = null;
  private started = false;
  private connected = false;
  private rosterChangedHandler: (() => void) | null = null;
  private connectionChangedHandler: ((connected: boolean) => void) | null = null;

  constructor(cfg: DaemonConfig) {
    this.cfg = cfg;
    this.log = cfg.log ?? (() => {});
  }

  // ── Public API ─────────────────────────────────────────────────────────

  start(): void {
    if (this.started) return;
    this.started = true;

    this.client.connectionStatus$.subscribe((status) => {
      if (status === ConnectionStatus.Connected) {
        this.connected = true;
        for (const item of this.sessions.values()) this.emitSet(item);
        this.connectionChangedHandler?.(true);
      } else if (status === ConnectionStatus.Disconnected) {
        this.connected = false;
        this.connectionChangedHandler?.(false);
      }
    });

    this.client.commandIncoming$.subscribe((msg) => {
      const data = (msg as { data?: { commandId?: string; command?: unknown } }).data;
      if (!data || data.commandId !== NOTIFY_CHANNEL_COMMAND_ID) return;
      const command = (data.command ?? {}) as { meta?: NotifyChannelMeta };
      this.notifyHandler?.(command.meta ?? {});
    });

    this.watch(getAllSessions()).subscribe((items) => {
      this.roster = items;
      this.rosterChangedHandler?.();
    });

    this.watch(getAllSessionNicknames()).subscribe((items) => {
      this.nicknames = items;
    });

    this.client.setAddress(withMykoPath(this.cfg.address));
  }

  stop(): void {
    for (const item of this.sessions.values()) this.emitDel(item);
    this.sessions.clear();
    if (this.livenessTimer) { clearInterval(this.livenessTimer); this.livenessTimer = null; }
    this.client.disconnect();
    this.started = false;
  }

  onNotify(handler: NotifyHandler): void { this.notifyHandler = handler; }
  onRosterChanged(handler: () => void): void { this.rosterChangedHandler = handler; }
  onConnectionChanged(handler: (connected: boolean) => void): void { this.connectionChangedHandler = handler; }
  isConnected(): boolean { return this.connected; }

  // ── Session management ─────────────────────────────────────────────────

  registerSession(sessionId: string): void {
    this.lastSessionId = sessionId;
    const existing = this.sessions.get(sessionId);
    const now = Date.now() as unknown as bigint;
    const item: SessionItem = existing ?? {
      id: sessionId,
      pid: process.pid,
      cwd: this.cfg.cwd,
      connectedAt: now,
      operator: this.cfg.identity.operator ?? null,
      host: this.cfg.identity.host ?? null,
      gitBranch: this.cfg.identity.gitBranch ?? null,
      project: this.cfg.identity.project ?? null,
      // Pi can render custom messages and start/steer turns through its
      // extension API, so identify the harness and advertise that capability.
      channelsEnabled: true,
      kind: "pi",
    } as SessionItem;
    (item as any).lastActivityAt = now;
    this.sessions.set(sessionId, item);
    if (this.connected) this.emitSet(item);
    this.ensureLivenessTimer();
  }

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

  // ── Write tools ────────────────────────────────────────────────────────

  sendMessage(asSession: string, to: string, body: string): Promise<SendMessageResult> {
    this.recordCommunication(asSession, to);
    return this.send(sendMessage(asSession, to, body));
  }

  recordCommunication(sessionId: string, peerSessionId: string | undefined): void {
    if (!peerSessionId || peerSessionId === sessionId) return;
    const peers = this.communicated.get(sessionId) ?? new Set<string>();
    peers.add(peerSessionId);
    this.communicated.set(sessionId, peers);
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

  async setStatus(sessionId: string, text: string): Promise<void> {
    const item = this.sessions.get(sessionId);
    if (item) (item as any).currentTask = text;
    await this.send(setSessionStatus(sessionId, text));
  }

  // ── Reads ──────────────────────────────────────────────────────────────

  async roster_snapshot(): Promise<SessionItem[]> {
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

  rosterSync(): SessionItem[] { return this.roster; }

  async readHistory(asSession: string, opts: { room?: string; since?: number; limit?: number }): Promise<MessageView[]> {
    const result = await this.send(readMessages({
      asSession,
      room: opts.room,
      inbox: opts.room ? false : true,
      sent: false,
      unread: false,
      since: opts.since === undefined ? undefined : BigInt(opts.since),
      limit: opts.limit ?? 50,
    }));
    const messages = result.messages ?? [];
    for (const m of messages) this.recordCommunication(asSession, m.fromSessionId);
    return messages;
  }

  async ackMessages(asSession: string, messageIds: string[]): Promise<number> {
    await this.send(ackMessages(asSession, messageIds));
    return messageIds.length;
  }

  formatMessageLine(m: MessageView): string {
    return `- [${m.messageId}] ${this.senderLabel(m.fromSessionId)}: ${m.body}`;
  }

  drainInbox(sessionId: string): Promise<string | null> {
    const prev = this.draining.get(sessionId) ?? Promise.resolve<string | null>(null);
    const next = prev.catch(() => null).then(() => this.drainInboxInner(sessionId));
    this.draining.set(sessionId, next);
    void next.finally(() => { if (this.draining.get(sessionId) === next) this.draining.delete(sessionId); });
    return next;
  }

  nicknameFor(sessionId: string): string {
    return this.nicknames.find((n) => (n as any).id === sessionId)?.nickname ?? sessionId;
  }

  currentNickname(): string | undefined {
    const sid = this.lastSessionId ?? (this.sessions.keys().next().value as string | undefined);
    if (!sid) return undefined;
    return this.nicknameFor(sid);
  }

  // ── Internals ──────────────────────────────────────────────────────────

  private async drainInboxInner(sessionId: string): Promise<string | null> {
    let result: ReadMessagesResult | undefined;
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        result = await this.send(readMessages({
          asSession: sessionId,
          toSession: sessionId,
          inbox: false,
          sent: false,
          unread: true,
          limit: INBOX_PULL_LIMIT,
        }));
      } catch {
        return null;
      }
      if (result.messages && result.messages.length > 0) break;
      if (attempt < 2) await new Promise((r) => setTimeout(r, 100 * (attempt + 1)));
    }
    if (!result?.messages?.length) return null;

    const block = this.renderInbox(result.messages);
    try {
      await this.send(ackMessages(sessionId, result.messages.map((m) => m.messageId)));
    } catch { this.log("inbox ack failed"); }
    return block;
  }

  private renderInbox(messages: MessageView[]): string {
    if (messages.length === 1) {
      const m = messages[0];
      return `new message from ${this.senderLabel(m.fromSessionId)}: ${m.body}`;
    }
    const lines = [`${messages.length} new messages from sibling agents:`];
    for (const m of messages) lines.push(`- ${this.senderLabel(m.fromSessionId)}: ${m.body}`);
    return lines.join("\n");
  }

  private senderLabel(sessionId: string): string {
    const nick = this.nicknameFor(sessionId);
    const s = this.roster.find((r) => (r as any).id === sessionId);
    if (!s) return nick;
    const ctx = [s.operator, s.host?.name].filter(Boolean).join("@");
    return ctx ? `${nick} (${ctx})` : nick;
  }

  private ensureLivenessTimer(): void {
    if (this.livenessTimer) return;
    this.livenessTimer = setInterval(() => {
      const now = Date.now();
      for (const [id, item] of this.sessions) {
        (item as any).lastActivityAt = now;
        void this.send(setLastActivityAt(id, now)).catch(() => {});
      }
    }, LIVENESS_INTERVAL_MS);
    if (typeof (this.livenessTimer as any).unref === "function") {
      (this.livenessTimer as any).unref();
    }
  }

  private emitSet(item: SessionItem): void {
    this.client.sendEvent(this.event(item, "SET"));
  }

  private emitDel(item: SessionItem): void {
    this.client.sendEvent(this.event(item, "DEL"));
  }

  private event(item: SessionItem, changeType: "SET" | "DEL"): MEvent {
    return {
      item: item as unknown as Record<string, unknown>,
      changeType,
      itemType: "Session",
      createdAt: new Date().toISOString(),
      tx: randomUUID(),
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
