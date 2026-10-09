// How a live push reaches the session. Kept free of pi and daemon imports so
// it can be tested with a fake inbox.

import type { NotifyChannelMeta } from "./entities.ts";
import { livePushText } from "./inbound.ts";

export interface LivePushTarget {
  /** Inject `content` into the session as a steer message. */
  inject(content: string): Promise<void> | void;
  /** Read, ack and render this session's unread direct messages. */
  drainInbox(): Promise<string | null>;
  /** Mark one message read for this session. */
  ack(messageId: string): Promise<void>;
}

export async function deliverLivePush(meta: NotifyChannelMeta, who: string, target: LivePushTarget): Promise<void> {
  const text = livePushText(meta, who);
  if (text) {
    // Don't drain here: that acks any other unread message without showing
    // it. Ack the pushed message alone: the daemon marks a direct push read
    // when it delivers it, but not a room @mention.
    await target.inject(text);
    if (meta.message_id) await target.ack(meta.message_id).catch(() => {});
    return;
  }
  // Older daemons send no body: read it from the inbox instead.
  const inbox = await target.drainInbox().catch(() => null);
  if (!inbox) return;
  await target.inject(inbox);
}
