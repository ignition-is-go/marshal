import { describe, expect, test } from "bun:test";

import { pushText } from "../src/push.js";

describe("live push text", () => {
  test("a whole body is shown as is", () => {
    expect(pushText("alice", { body: "hi", body_truncated: false, message_id: "m1" })).toBe(
      "new message from alice: hi",
    );
  });

  test("a truncated body says so and names the message", () => {
    expect(pushText("alice", { body: "the first part", body_truncated: true, message_id: "m1" })).toBe(
      "new message from alice: the first part\n" +
        "[preview truncated; read the full message with marshal_messages (message m1)]",
    );
  });

  test("a truncated body without an id still says so", () => {
    expect(pushText("alice", { body: "the first part", body_truncated: true })).toEndWith(
      "[preview truncated; read the full message with marshal_messages]",
    );
  });
});
