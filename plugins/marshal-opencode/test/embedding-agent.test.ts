import { expect, test } from "bun:test";
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { hostname, tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
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
  const fixture = mkdtempSync(join(tmpdir(), "marshal-opencode-identity-"));
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
    await Bun.$`git -C ${fixture} init -b metadata-fixture`.quiet();
    await Bun.$`git -C ${fixture} -c user.name=Marshal -c user.email=marshal@example.invalid commit --allow-empty -m fixture`.quiet();
    for (const value of [undefined, "", " \t "]) {
      if (value === undefined) delete process.env.MARSHAL_AGENT_ID;
      else process.env.MARSHAL_AGENT_ID = value;
      expect((await resolveIdentity(Bun.$, fixture)).agentId).toBeUndefined();
    }
    const agent = 'agent/"x\nopaque';
    process.env.MARSHAL_AGENT_ID = ` ${agent} `;
    const identity = await resolveIdentity(Bun.$, fixture);
    expect(identity.agentId).toBe(agent);
    expect(identity.host.name).not.toBe("unknown");
    expect(identity.host.name).toBe(hostname().split(".")[0]);
    expect(identity.gitBranch).toBe("metadata-fixture");
    expect(identity.project).toBe(basename(fixture));
    client = new MarshalDaemon({ address: `ws://127.0.0.1:${ws}`, cwd: fixture, identity });
    const daemon = client;
    daemon.registerSession("metadata-native");
    daemon.start();
    await until(() => daemon.isConnected());
    const correct = () => daemon.rosterSync().some((s) => s.id === "metadata-native" && s.agentId === agent);
    await until(correct);
    const hook = Bun.spawn([join(dirname(binary), "marshal-shim"), "codex-hook", "session-start"], {
      env: { ...env, MARSHAL_BASE_URL: `http://127.0.0.1:${http}`, MARSHAL_AGENT_ID: ` ${agent} ` },
      stdin: "pipe", stdout: "pipe", stderr: "pipe",
    });
    hook.stdin.write(JSON.stringify({ session_id: "metadata-hook", cwd: fixture }));
    hook.stdin.end();
    expect(await hook.exited).toBe(0);
    await until(() => daemon.rosterSync().some((s) => s.id === "metadata-hook" && s.agentId === agent));
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
    rmSync(fixture, { recursive: true, force: true });
  }
}, 30_000);
