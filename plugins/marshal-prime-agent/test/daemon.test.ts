import assert from "node:assert/strict";
import { describe, test } from "node:test";

import { MarshalDaemon } from "../src/daemon.ts";

describe("inbox pull", () => {
  test("returns at once while Marshal is disconnected", async () => {
    const daemon = new MarshalDaemon({
      address: "ws://127.0.0.1:1",
      cwd: "/tmp",
      identity: { operator: "test", host: { name: "test", os: "linux", arch: "x64" } },
    });

    const outcome = await Promise.race([
      daemon.pullInbox("ses_disconnected"),
      new Promise<"blocked">((resolve) => setTimeout(() => resolve("blocked"), 100)),
    ]);

    assert.equal(outcome, null);
  });
});
