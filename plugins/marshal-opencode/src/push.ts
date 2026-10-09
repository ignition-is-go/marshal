// Kept out of index.ts: opencode treats every function a plugin module
// exports as a plugin.

import type { NotifyChannelMeta } from "./entities.js";

/** The turn text for a live push. `meta.body` is a bounded preview; when the
 *  daemon cut it, say so and name the message, so the agent reads the rest
 *  with marshal_messages instead of acting on half a message. */
export function pushText(who: string, meta: NotifyChannelMeta): string {
  const text = `new message from ${who}: ${meta.body ?? ""}`;
  if (!meta.body_truncated) return text;
  const id = meta.message_id ? ` (message ${meta.message_id})` : "";
  return `${text}\n[preview truncated; read the full message with marshal_messages${id}]`;
}
