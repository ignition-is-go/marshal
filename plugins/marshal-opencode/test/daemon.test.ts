import { describe, expect, test } from "bun:test";

import { MarshalDaemon } from "../src/daemon.js";

describe("optional per-turn inbox delivery", () => {
  test("returns immediately while Marshal is disconnected", async () => {
    const daemon = new MarshalDaemon({
      address: "ws://127.0.0.1:1",
      cwd: "/tmp",
      identity: {
        operator: "test",
        host: { name: "test", os: "linux", arch: "x64" },
      },
    });

    const outcome = await Promise.race([
      daemon.drainInbox("ses_disconnected"),
      new Promise<"blocked">((resolve) => setTimeout(() => resolve("blocked"), 100)),
    ]);

    expect(outcome).toBeNull();
  });
});
