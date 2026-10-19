// Text a live push injects into the session. Kept free of pi and daemon
// imports so it can be tested directly.

import type { NotifyChannelMeta } from "./entities.ts";

/** The steer message for a live push, or `undefined` when the push carries no
 *  body (older daemons), in which case the caller drains the inbox instead.
 *
 *  The daemon caps `body` at `CONTEXT_BODY_MAX_CHARS` and marks a delivered
 *  push read, so a clipped body is never completed by the inbox. Say so, and
 *  name the message so the agent can read the rest with marshal_messages. */
export function livePushText(meta: NotifyChannelMeta, who: string): string | undefined {
  if (!meta.body) return undefined;
  const text = `new message from ${who}: ${meta.body}`;
  if (!meta.body_truncated) return text;
  const id = meta.message_id ? ` (message ${meta.message_id})` : "";
  return `${text}\n[preview truncated; read the full message with marshal_messages${id}]`;
}
