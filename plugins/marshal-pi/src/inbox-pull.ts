// Inbox pulls that are acked only once their text has been shown.
//
// Acking in the same step as the read loses messages whenever the caller ends
// up not showing what it read: a per-turn pull that loses its timeout race, or
// an injection that fails. A pull here hands the messages back unacked; the
// caller commits it once the text is in the conversation, or releases it so
// the next pull hands the same messages out again.

/** Unread messages read but not yet acked. */
export interface InboxPull {
  /** The rendered messages, ready to inject. */
  readonly text: string;
  /** Ack the messages. Call once `text` has been shown. */
  commit(): Promise<void>;
  /** Give the messages back unacked, so the next pull hands them out again. */
  release(): void;
}

/** Where a pull reads, acks and renders messages. */
export interface InboxSource<M extends { messageId: string }> {
  read(sessionId: string): Promise<M[] | null>;
  ack(sessionId: string, messageIds: string[]): Promise<void>;
  render(messages: M[]): string;
}

export class InboxPuller<M extends { messageId: string }> {
  /** Per session, the ids an outstanding pull holds: still unread, so a read returns them. */
  private readonly held = new Map<string, Set<string>>();
  /** Per session, the pull in flight. Pulls run one at a time, so each sees what the last one holds. */
  private readonly pulling = new Map<string, Promise<unknown>>();

  private readonly source: InboxSource<M>;

  constructor(source: InboxSource<M>) {
    this.source = source;
  }

  pull(sessionId: string): Promise<InboxPull | null> {
    const previous = this.pulling.get(sessionId) ?? Promise.resolve();
    const next = previous.catch(() => {}).then(() => this.pullNow(sessionId));
    this.pulling.set(sessionId, next);
    void next.catch(() => {}).finally(() => {
      if (this.pulling.get(sessionId) === next) this.pulling.delete(sessionId);
    });
    return next;
  }

  private async pullNow(sessionId: string): Promise<InboxPull | null> {
    const held = this.held.get(sessionId) ?? new Set<string>();
    const messages = (await this.source.read(sessionId))?.filter((m) => !held.has(m.messageId));
    if (!messages?.length) return null;
    // Render before holding anything, so a render that throws holds nothing.
    const text = this.source.render(messages);
    const ids = messages.map((m) => m.messageId);
    for (const id of ids) held.add(id);
    this.held.set(sessionId, held);
    const settle = () => { for (const id of ids) held.delete(id); };
    return {
      text,
      commit: async () => {
        try {
          await this.source.ack(sessionId, ids);
        } finally {
          settle();
        }
      },
      release: settle,
    };
  }
}

/**
 * Pull for a turn, giving up after `timeoutMs` so the turn is never held up.
 * A pull that lands after that is released, not acked: its messages were not
 * shown, so they stay unread for the next turn.
 */
export async function pullForTurn(pull: () => Promise<InboxPull | null>, timeoutMs: number): Promise<InboxPull | null> {
  const pending = pull().catch(() => null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<"late">((resolve) => { timer = setTimeout(() => resolve("late"), timeoutMs); });
  const winner = await Promise.race([pending, deadline]);
  clearTimeout(timer);
  if (winner !== "late") return winner;
  void pending.then((landed) => landed?.release());
  return null;
}
