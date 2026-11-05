import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { InboxPuller, pullForTurn } from "../src/inbox-pull.ts";

// An in-memory inbox: `read` returns what is still unread, `ack` marks read.
function fakeInbox(ids: string[]) {
  const unread = new Set(ids);
  const acked: string[] = [];
  const puller = new InboxPuller<{ messageId: string }>({
    read: async () => [...unread].map((messageId) => ({ messageId })),
    ack: async (_sessionId, messageIds) => {
      for (const id of messageIds) {
        unread.delete(id);
        acked.push(id);
      }
    },
    render: (messages) => messages.map((m) => m.messageId).join(","),
  });
  return { puller, unread, acked };
}

describe("inbox pull", () => {
  test("acks nothing until the pull is committed", async () => {
    const inbox = fakeInbox(["m1", "m2"]);
    const pull = await inbox.puller.pull("s");
    assert.equal(pull?.text, "m1,m2");
    assert.deepEqual(inbox.acked, []);

    await pull!.commit();
    assert.deepEqual(inbox.acked, ["m1", "m2"]);
  });

  test("does not hand out messages another pull is still holding", async () => {
    const inbox = fakeInbox(["m1"]);
    const first = await inbox.puller.pull("s");
    assert.equal(first?.text, "m1");
    assert.equal(await inbox.puller.pull("s"), null);

    inbox.unread.add("m2");
    assert.equal((await inbox.puller.pull("s"))?.text, "m2");
  });

  test("two pulls started together never hand out the same message", async () => {
    const inbox = fakeInbox(["m1"]);
    const pulls = await Promise.all([inbox.puller.pull("s"), inbox.puller.pull("s")]);
    assert.deepEqual(pulls.filter((pull) => pull !== null).map((pull) => pull!.text), ["m1"]);
  });

  test("a released pull leaves its messages unread for the next pull", async () => {
    const inbox = fakeInbox(["m1"]);
    const first = await inbox.puller.pull("s");
    first!.release();
    assert.deepEqual(inbox.acked, []);
    assert.equal((await inbox.puller.pull("s"))?.text, "m1");
  });

  test("a commit whose ack fails rejects and leaves the messages for the next pull", async () => {
    const unread = new Set(["m1"]);
    const puller = new InboxPuller<{ messageId: string }>({
      read: async () => [...unread].map((messageId) => ({ messageId })),
      ack: async () => { throw new Error("daemon gone"); },
      render: (messages) => messages.map((m) => m.messageId).join(","),
    });
    const pull = await puller.pull("s");
    await assert.rejects(pull!.commit(), /daemon gone/);
    assert.equal((await puller.pull("s"))?.text, "m1");
  });
  test("a pull whose render throws leaves the messages for the next pull", async () => {
    let renders = 0;
    const puller = new InboxPuller<{ messageId: string }>({
      read: async () => [{ messageId: "m1" }],
      ack: async () => {},
      render: (messages) => {
        renders += 1;
        if (renders === 1) throw new Error("bad message");
        return messages.map((m) => m.messageId).join(",");
      },
    });
    await assert.rejects(puller.pull("s"), /bad message/);
    assert.equal((await puller.pull("s"))?.text, "m1");
  });
});

describe("pull for a turn", () => {
  test("returns a pull that lands in time, still uncommitted", async () => {
    const inbox = fakeInbox(["m1"]);
    const pull = await pullForTurn(() => inbox.puller.pull("s"), 1_000);
    assert.equal(pull?.text, "m1");
    assert.deepEqual(inbox.acked, []);
  });

  test("a pull that lands after the deadline leaves its messages for the next turn", async () => {
    const inbox = fakeInbox(["m1"]);
    let land!: () => void;
    const slowRead = new Promise<void>((resolve) => { land = resolve; });
    const late = pullForTurn(async () => {
      await slowRead;
      return inbox.puller.pull("s");
    }, 10);
    assert.equal(await late, null);

    land();
    await new Promise((resolve) => setTimeout(resolve, 10));
    assert.deepEqual(inbox.acked, []);
    assert.equal((await inbox.puller.pull("s"))?.text, "m1");
  });
});
