import { describe, expect, test } from "bun:test";

import type { NotifyChannelMeta } from "../src/entities.ts";
import { deliverLivePush, type LivePushTarget } from "../src/live-push.ts";

// A session inbox: drainInbox reads every unread message, acks it and returns
// the rendered block, like MarshalDaemon.drainInbox.
function fakeSession(unread: string[]) {
  const injected: string[] = [];
  const target: LivePushTarget = {
    inject: async (content) => { injected.push(content); },
    drainInbox: async () => {
      if (unread.length === 0) return null;
      const block = unread.join("\n");
      unread.length = 0;
      return block;
    },
  };
  return { target, injected, unread };
}

describe("deliverLivePush", () => {
  test("injects the pushed body", async () => {
    const session = fakeSession([]);
    await deliverLivePush({ body: "hello" }, "alice", session.target);
    expect(session.injected).toEqual(["new message from alice: hello"]);
  });

  // The daemon marks a delivered push read, so the drain after it can only
  // find OTHER messages, e.g. one sent while this session had no live client.
  test("leaves another unread message for the inbox", async () => {
    const session = fakeSession(["new message from bob: sent while you were reconnecting"]);
    await deliverLivePush({ body: "hello" }, "alice", session.target);
    expect(session.injected).toEqual(["new message from alice: hello"]);
    expect(session.unread).toEqual(["new message from bob: sent while you were reconnecting"]);
  });

  test("falls back to the inbox when the push has no body", async () => {
    const meta: NotifyChannelMeta = { from_session: "ses_a" };
    const session = fakeSession(["new message from alice: hello"]);
    await deliverLivePush(meta, "alice", session.target);
    expect(session.injected).toEqual(["new message from alice: hello"]);
    expect(session.unread).toEqual([]);
  });

  test("injects nothing when the push has no body and the inbox is empty", async () => {
    const session = fakeSession([]);
    await deliverLivePush({}, "alice", session.target);
    expect(session.injected).toEqual([]);
  });
});
