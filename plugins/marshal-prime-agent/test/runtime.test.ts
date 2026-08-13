import assert from "node:assert/strict";
import test from "node:test";

import extension from "../src/index.ts";
import { isPrimeAgentSubagent, shouldRegisterPrimeAgentSession } from "../src/runtime.ts";

test("only positive integer RLM depths identify sub-agents", () => {
  for (const depth of [1, 2, 99]) assert.equal(isPrimeAgentSubagent(depth), true);
  for (const depth of [undefined, null, "1", "", 0, -1, 1.5, "not-a-depth"]) {
    assert.equal(isPrimeAgentSubagent(depth), false);
  }
});

test("session headers only exclude RLM children", () => {
  assert.equal(shouldRegisterPrimeAgentSession(null), true);
  assert.equal(shouldRegisterPrimeAgentSession({}), true);
  assert.equal(shouldRegisterPrimeAgentSession({ rlmDepth: 0 }), true);
  assert.equal(shouldRegisterPrimeAgentSession({ parentSession: "/parent.jsonl", rlmDepth: 0 }), true);
  assert.equal(shouldRegisterPrimeAgentSession({ rlmDepth: 1 }), false);
  assert.equal(shouldRegisterPrimeAgentSession({ rlmDepth: 2 }), false);
});

test("a child session_start cannot initialize a Marshal session", async () => {
  const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => unknown>>();
  const tools: Array<{ name: string; execute: (...args: unknown[]) => unknown }> = [];
  const pi = {
    on(name: string, handler: (event: unknown, ctx: unknown) => unknown) {
      const registered = handlers.get(name) ?? [];
      registered.push(handler);
      handlers.set(name, registered);
    },
    registerTool(tool: { name: string; execute: (...args: unknown[]) => unknown }) { tools.push(tool); },
    registerCommand() {},
  };

  await extension(pi as never);
  const sessionStart = handlers.get("session_start")?.[0];
  assert.ok(sessionStart);
  await sessionStart({}, {
    sessionManager: {
      getHeader: () => ({ parentSession: "/parent.jsonl", rlmDepth: 1 }),
      getSessionId: () => { throw new Error("child session id must not be read"); },
    },
  });

  const contextInjection = handlers.get("before_agent_start")?.[1];
  assert.ok(contextInjection);
  assert.equal(await contextInjection({ prompt: "child task", systemPrompt: "base" }, {}), undefined);

  const roster = tools.find((tool) => tool.name === "marshal_roster");
  assert.ok(roster);
  const result = await roster.execute("call", {});
  assert.equal((result as { content: Array<{ text: string }> }).content[0].text, "marshal: not connected");
});
