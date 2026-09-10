import { describe, expect, test } from "bun:test";

import { MarshalDaemon } from "../src/daemon.ts";

// Regression: while the marshal daemon is unreachable, daemon operations must
// FAIL OPEN (resolve/return) rather than hang. A disconnected @myko/core client
// keeps `sendCommand` pending while it reconnects, so anything that awaits it
// without a connection guard blocks forever — the reported symptom is that
// calling a marshal_* tool with no daemon running spins indefinitely.
//
// Mirrors the opencode sibling plugin's test/daemon.test.ts. Run with `bun test`.
describe("marshal-pi daemon fails open while disconnected", () => {
  const makeDaemon = () =>
    new MarshalDaemon({
      address: "ws://127.0.0.1:1",
      cwd: "/tmp",
      identity: {
        operator: "test",
        host: { name: "test", os: "linux", arch: "x64" },
      },
    });

  test("reports not connected before start()", () => {
    expect(makeDaemon().isConnected()).toBe(false);
  });

  test("drainInbox returns immediately while disconnected", async () => {
    const daemon = makeDaemon();

    const outcome = await Promise.race([
      daemon.drainInbox("ses_disconnected"),
      new Promise<"blocked">((resolve) => setTimeout(() => resolve("blocked"), 100)),
    ]);

    expect(outcome).toBeNull();
  });
});
