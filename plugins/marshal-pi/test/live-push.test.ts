import { describe, expect, test } from "bun:test";

import type { NotifyChannelMeta } from "../src/entities.ts";
import { deliverLivePush, type LivePushTarget } from "../src/live-push.ts";

// A session inbox: drainInbox reads every unread message, acks it and returns
// the rendered block, like MarshalDaemon.drainInbox.
function fakeSession(unread: string[]) {
  const injected: string[] = [];
  const acked: string[] = [];
  const target: LivePushTarget = {
    inject: async (content) => { injected.push(content); },
    ack: async (messageId) => { acked.push(messageId); },
    drainInbox: async () => {
      if (unread.length === 0) return null;
      const block = unread.join("\n");
      unread.length = 0;
      return block;
    },
  };
  return { target, injected, unread, acked };
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

  // A direct push is already read when it arrives, but a room @mention is not:
  // ack the pushed message itself so the next inbox doesn't show it again.
  test("acks the pushed message and nothing else", async () => {
    const session = fakeSession(["new message from bob: still unread"]);
    await deliverLivePush({ body: "@you look at this", message_id: "msg_7" }, "alice", session.target);
    expect(session.acked).toEqual(["msg_7"]);
    expect(session.unread).toEqual(["new message from bob: still unread"]);
  });

  test("acks nothing when the push names no message", async () => {
    const session = fakeSession([]);
    await deliverLivePush({ body: "hello" }, "alice", session.target);
    expect(session.acked).toEqual([]);
  });

  test("keeps the truncation notice and still leaves the inbox alone", async () => {
    const session = fakeSession(["new message from bob: still unread"]);
    await deliverLivePush({ body: "the first part", body_truncated: true, message_id: "msg_1" }, "alice", session.target);
    expect(session.injected).toEqual([
      "new message from alice: the first part\n[preview truncated; read the full message with marshal_messages (message msg_1)]",
    ]);
    expect(session.unread).toEqual(["new message from bob: still unread"]);
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
