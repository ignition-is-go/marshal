// How a live push reaches the session. Kept free of pi and daemon imports so
// it can be tested with a fake inbox.

import type { NotifyChannelMeta } from "./entities.ts";
import { livePushText } from "./inbound.ts";

export interface LivePushTarget {
  /** Inject `content` into the session as a steer message. */
  inject(content: string): Promise<void> | void;
  /** Read, ack and render this session's unread direct messages. */
  drainInbox(): Promise<string | null>;
}

export async function deliverLivePush(meta: NotifyChannelMeta, who: string, target: LivePushTarget): Promise<void> {
  const text = livePushText(meta, who);
  if (text) {
    // The daemon marked this message read when it delivered the push. Don't
    // drain here: that acks any other unread message without showing it.
    await target.inject(text);
    return;
  }
  // Older daemons send no body: read it from the inbox instead.
  const inbox = await target.drainInbox().catch(() => null);
  if (!inbox) return;
  await target.inject(inbox);
}
