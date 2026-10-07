// Text a live push injects into the session. Kept free of pi and daemon
// imports so it can be tested directly.

import type { NotifyChannelMeta } from "./entities.ts";

/** The steer message for a live push, or `undefined` when the push carries no
 *  body (older daemons), in which case the caller drains the inbox instead. */
export function livePushText(meta: NotifyChannelMeta, who: string): string | undefined {
  if (!meta.body) return undefined;
  return `new message from ${who}: ${meta.body}`;
}
