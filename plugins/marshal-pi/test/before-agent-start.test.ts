import { describe, expect, test } from "bun:test";

import marshalPi from "../src/index.ts";

// pi 1.2.0 fires before_agent_start for a run started by
// pi.sendMessage(..., { triggerTurn: true }) as well as for a prompt. That
// event has messageType "custom", the starting `message`, and no `prompt`.
// marshal-pi's own live push starts runs that way, so the status handler must
// not assume a prompt. Run with `bun test`.
type Handler = (event: Record<string, unknown>, ctx: unknown) => unknown;

async function loadExtension() {
  const handlers = new Map<string, Handler[]>();
  const pi = {
    on: (name: string, handler: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), handler]),
    registerTool: () => {},
    registerCommand: () => {},
    sendMessage: () => {},
  };
  let footer = "";
  const ctx = {
    hasUI: true,
    ui: { setStatus: (key: string, text?: string) => { if (key === "marshal") footer = text ?? ""; }, notify: () => {} },
    sessionManager: { getSessionFile: () => "/tmp/marshal-pi-before-agent-start.jsonl" },
  };
  await marshalPi(pi as never);
  // Each handler separately, as pi's runner calls them: a throw is reported
  // against that handler and the others still run.
  const emit = async (name: string, event: Record<string, unknown>) => {
    const errors: string[] = [];
    for (const handler of handlers.get(name) ?? []) {
      try { await handler(event, ctx); } catch (error) { errors.push(String(error)); }
    }
    return errors;
  };
  await emit("session_start", { type: "session_start" });
  return { emit, footer: () => footer };
}

const base = { type: "before_agent_start", systemPrompt: "", systemPromptOptions: {} };

describe("marshal-pi before_agent_start", () => {
  test("a prompt run sets the status from the prompt", async () => {
    const ext = await loadExtension();
    expect(await ext.emit("before_agent_start", { ...base, messageType: "user", prompt: "fix the deploy" })).toEqual([]);
    expect(ext.footer()).toContain('"fix the deploy"');
    await ext.emit("session_shutdown", { type: "session_shutdown" });
  });

  test("a custom-message run does not throw and keeps the prompt's status", async () => {
    const ext = await loadExtension();
    await ext.emit("before_agent_start", { ...base, messageType: "user", prompt: "fix the deploy" });
    const errors = await ext.emit("before_agent_start", {
      ...base,
      messageType: "custom",
      message: { role: "custom", customType: "marshal-message", content: "hi", display: true },
    });
    expect(errors).toEqual([]);
    expect(ext.footer()).toContain('"fix the deploy"');
    await ext.emit("session_shutdown", { type: "session_shutdown" });
  });

  test("an event without messageType (pi before 1.2.0) is a prompt run", async () => {
    const ext = await loadExtension();
    expect(await ext.emit("before_agent_start", { ...base, prompt: "older pi" })).toEqual([]);
    expect(ext.footer()).toContain('"older pi"');
    await ext.emit("session_shutdown", { type: "session_shutdown" });
  });
});
