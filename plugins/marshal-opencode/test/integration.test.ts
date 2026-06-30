// Integration test: the plugin's real MarshalDaemon (the @myko/core MykoClient
// path that the unit tests can't reach) round-tripping against a REAL
// marshal-daemon binary over the real myko WS wire (CBOR), real command/query
// handlers, real NotifyChannel push.
//
// This deliberately does NOT fake the daemon or the wire — the only thing
// under test is that round-trip, so faking either would prove nothing (cf. the
// "test the real spawn path, not a synthetic one" rule).
//
// The daemon binary is located in this order:
//   1. $MARSHAL_DAEMON_BIN
//   2. ../../target/{debug,release}/marshal-daemon (cargo build -p marshal-daemon)
// If neither exists the whole suite SKIPS (loudly) so `bun test` stays green
// on a box without the binary — run `cargo build -p marshal-daemon` first to
// enable it.
//
// Run: bun test test/integration.test.ts

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { connect, createServer } from "node:net";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { MarshalDaemon } from "../src/daemon.js";
import type { Identity } from "../src/identity.js";
import { nickname } from "../src/nickname.js";

function resolveDaemonBin(): string | null {
  const fromEnv = process.env.MARSHAL_DAEMON_BIN;
  if (fromEnv && existsSync(fromEnv)) return fromEnv;
  for (const profile of ["debug", "release"]) {
    const p = join(import.meta.dir, "..", "..", "..", "target", profile, "marshal-daemon");
    if (existsSync(p)) return p;
  }
  return null;
}

const DAEMON_BIN = resolveDaemonBin();

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const s = createServer();
    s.once("error", reject);
    s.listen(0, "127.0.0.1", () => {
      const port = (s.address() as { port: number }).port;
      s.close(() => resolve(port));
    });
  });
}

function tcpOpen(port: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const c = connect({ host: "127.0.0.1", port }, () => {
      c.end();
      resolve();
    });
    c.once("error", reject);
  });
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function waitFor(predicate: () => Promise<boolean> | boolean, timeoutMs: number, stepMs = 100): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      if (await predicate()) return true;
    } catch {
      /* keep polling */
    }
    await sleep(stepMs);
  }
  return false;
}

function makeIdentity(operator: string): Identity {
  return { operator, host: { name: "itest", os: "linux", arch: "x64" }, project: "marshal-opencode" };
}

// ── suite ────────────────────────────────────────────────────────────────

if (!DAEMON_BIN) {
  // eslint-disable-next-line no-console
  console.warn(
    "[marshal-opencode integration] SKIPPED — no marshal-daemon binary found. " +
      "Build it with `cargo build -p marshal-daemon` (from external/marshal) or set " +
      "MARSHAL_DAEMON_BIN. Unit tests still cover the wire shapes.",
  );
}

const suite = DAEMON_BIN ? describe : describe.skip;

suite("marshal-opencode ↔ real marshal-daemon", () => {
  let proc: ReturnType<typeof Bun.spawn> | undefined;
  let stateDir = "";
  let address = "";

  const A = "ses_itest_a";
  const B = "ses_itest_b";
  let daemonA: MarshalDaemon;
  let daemonB: MarshalDaemon;

  beforeAll(async () => {
    const wsPort = await freePort();
    const hookPort = await freePort();
    stateDir = mkdtempSync(join(tmpdir(), "marshal-itest-"));
    address = `ws://127.0.0.1:${wsPort}`;

    proc = Bun.spawn([DAEMON_BIN!], {
      env: {
        ...process.env,
        MARSHAL_BIND: `127.0.0.1:${wsPort}`,
        MARSHAL_HOOK_BIND: `127.0.0.1:${hookPort}`,
        MARSHAL_STATE_DIR: stateDir,
        RUST_LOG: "warn",
      },
      stdout: "pipe",
      stderr: "pipe",
    });

    const up = await waitFor(() => tcpOpen(wsPort).then(() => true), 15_000);
    if (!up) throw new Error(`marshal-daemon did not accept connections on ${wsPort} within 15s`);

    daemonA = new MarshalDaemon({ address, cwd: "/tmp/itest-a", identity: makeIdentity("alice") });
    daemonB = new MarshalDaemon({ address, cwd: "/tmp/itest-b", identity: makeIdentity("bob") });
    daemonA.start();
    daemonB.start();
    // Let both MykoClients complete their WS handshake before SETs go out.
    await sleep(500);
  }, 30_000);

  afterAll(async () => {
    try {
      daemonA?.stop();
      daemonB?.stop();
    } finally {
      proc?.kill();
      if (stateDir) rmSync(stateDir, { recursive: true, force: true });
    }
  });

  test("both sessions appear on the live roster", async () => {
    daemonA.registerSession(A);
    daemonB.registerSession(B);

    const ok = await waitFor(async () => {
      const ids = (await daemonA.roster_snapshot()).map((s) => s.id);
      return ids.includes(A) && ids.includes(B);
    }, 10_000);
    expect(ok).toBe(true);

    const roster = await daemonA.roster_snapshot();
    const a = roster.find((s) => s.id === A);
    // Assert the Session entity fields the plugin populates round-trip intact —
    // a rename on the Rust side (operator/host/cwd/project) trips these.
    expect(a?.operator).toBe("alice");
    expect(a?.host?.name).toBe("itest");
    expect(a?.cwd).toBe("/tmp/itest-a");
    expect(a?.project).toBe("marshal-opencode");
  }, 20_000);

  test("a sent message lands in the recipient's inbox, then acks", async () => {
    const res = await daemonA.sendMessage(A, B, "hello from alice");
    expect(res.messageId).toBeString();
    expect(res.toSessionId).toBe(B);
    // SendMessageResult.deliveredLive — assert the field exists with its real
    // type so a Rust-side rename/retype of the result is caught here.
    expect(typeof res.deliveredLive).toBe("boolean");

    const inbox = await daemonB.drainInbox(B);
    expect(inbox).toContain("hello from alice");
    expect(inbox).toContain(nickname(A)); // sender's nickname is shown (address replies to it)

    // Acked on first drain → second drain is empty.
    const again = await daemonB.drainInbox(B);
    expect(again).toBeNull();
  }, 20_000);

  test("inbound message pushes a real-time NotifyChannel to the recipient", async () => {
    const seen: string[] = [];
    daemonB.onNotify((meta) => {
      if (meta.body) seen.push(meta.body);
    });

    await daemonA.sendMessage(A, B, "ping over the wire");
    const got = await waitFor(() => seen.some((b) => b.includes("ping over the wire")), 5_000);
    expect(got).toBe(true);
  }, 20_000);

  test("join_room and broadcast round-trip over the wire", async () => {
    // JoinRoom: validate the command + JoinRoomResult shape. (Ad-hoc rooms get
    // a freshly-minted unique id per join, so this is a solo room.)
    const join = await daemonA.joinRoom(A, "itest-room");
    expect(join.name).toBe("itest-room");
    expect(typeof join.roomId).toBe("string");
    expect(join.created).toBe(true);

    // BroadcastMessage + delivery: broadcast to the `everyone` auto-room, which
    // both sessions are members of. Retry to absorb the auto-room saga
    // populating membership after registration (the "no other members" guard
    // throws before persisting, so retries don't duplicate the message).
    let res: Awaited<ReturnType<typeof daemonA.broadcast>> | undefined;
    const sent = await waitFor(async () => {
      try {
        res = await daemonA.broadcast(A, "everyone", "hello everyone");
        return true;
      } catch {
        return false;
      }
    }, 10_000);
    expect(sent).toBe(true);
    expect(res!.total).toBeGreaterThanOrEqual(1);
    expect(res!.delivered.length).toBeGreaterThanOrEqual(1);

    const inboxB = await daemonB.drainInbox(B);
    expect(inboxB).toContain("hello everyone");
  }, 20_000);

  test("set_status surfaces on the roster", async () => {
    await daemonA.setStatus(A, "running the integration test");
    const ok = await waitFor(async () => {
      const a = (await daemonA.roster_snapshot()).find((s) => s.id === A);
      return a?.currentTask === "running the integration test";
    }, 10_000);
    expect(ok).toBe(true);
  }, 20_000);
});
