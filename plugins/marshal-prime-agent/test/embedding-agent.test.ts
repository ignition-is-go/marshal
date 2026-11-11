import { expect, test } from "bun:test";
import { createServer } from "node:net";
import { MarshalDaemon } from "../src/daemon.ts";
import { resolveIdentity } from "../src/identity.ts";

async function freePort(): Promise<number> {
  const server = createServer();
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") throw new Error("expected TCP address");
      server.close(() => resolve(address.port));
    });
  });
}

async function until(predicate: () => boolean | Promise<boolean>): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (!(await predicate())) {
    if (Date.now() > deadline) throw new Error("metadata did not reach the roster");
    await Bun.sleep(25);
  }
}

test.skipIf(!process.env.MARSHAL_DAEMON_BIN)("embedding metadata survives native registration and daemon reconnect", async () => {
  const binary = process.env.MARSHAL_DAEMON_BIN;
  if (!binary) throw new Error("set MARSHAL_DAEMON_BIN to the built daemon");
  const before = process.env.MARSHAL_AGENT_ID;
  const ws = await freePort();
  const http = await freePort();
  const env = { ...process.env };
  delete env.MYKO_POSTGRES_URL;
  const spawn = () => Bun.spawn([binary], {
    env: { ...env, MARSHAL_BIND: `127.0.0.1:${ws}`, MARSHAL_HOOK_BIND: `127.0.0.1:${http}` },
    stdout: "ignore", stderr: "inherit",
  });
  let processHandle = spawn();
  let client: MarshalDaemon | undefined;
  try {
    for (const value of [undefined, "", " \t "]) {
      if (value === undefined) delete process.env.MARSHAL_AGENT_ID;
      else process.env.MARSHAL_AGENT_ID = value;
      expect((await resolveIdentity("/tmp")).agentId).toBeUndefined();
    }
    const agent = 'agent/"x\nopaque';
    process.env.MARSHAL_AGENT_ID = ` ${agent} `;
    const identity = await resolveIdentity("/tmp");
    expect(identity.agentId).toBe(agent);
    client = new MarshalDaemon({ address: `ws://127.0.0.1:${ws}`, cwd: "/tmp", identity });
    const daemon = client;
    daemon.registerSession("metadata-native");
    daemon.start();
    await until(() => daemon.isConnected());
    const correct = () => daemon.rosterSync().some((s) => s.id === "metadata-native" && s.agentId === agent);
    await until(correct);
    daemon.registerSession("history-sender");
    await until(() => daemon.rosterSync().some((s) => s.id === "history-sender"));
    const earlier = await daemon.sendMessage("history-sender", "metadata-native", "before history cutoff");
    const history = await daemon.readHistory("metadata-native", {});
    const first = history.find((message) => message.messageId === earlier.messageId);
    if (!first) throw new Error("Sent message did not reach history");
    expect(typeof first.sentAt).toBe("number");
    const since = Number(first.sentAt) + 1;
    await until(() => Date.now() >= since);
    const later = await daemon.sendMessage("history-sender", "metadata-native", "after history cutoff");
    const filtered = await daemon.readHistory("metadata-native", { since });
    expect(filtered.map((message) => message.messageId)).toContain(later.messageId);
    expect(filtered.map((message) => message.messageId)).not.toContain(earlier.messageId);
    daemon.registerSession("metadata-native");
    await until(correct);
    processHandle.kill();
    await processHandle.exited;
    await until(() => !daemon.isConnected());
    processHandle = spawn();
    await until(() => daemon.isConnected());
    await until(correct);
  } finally {
    client?.stop();
    processHandle.kill();
    await processHandle.exited;
    if (before === undefined) delete process.env.MARSHAL_AGENT_ID;
    else process.env.MARSHAL_AGENT_ID = before;
  }
}, 30_000);
