// Locks the TS nickname port to the Rust daemon's output. These pairs are
// pinned against the shared wordlists (crates/entities/src/nickname.rs); a
// regression here means the plugin would show a DIFFERENT handle than peers
// see — defeating the point. Regenerate if the wordlists change on both sides.
//
// Run: bun test

import { describe, expect, test } from "bun:test";

import { nickname } from "../src/nickname.js";

describe("nickname matches the Rust daemon byte-for-byte", () => {
  test.each([
    ["0834c23f-94ab-4a39-b344-94d6bc8fdd47", "vernal-hedgehog"],
    ["ses_0ef1bd4a4ffe36zulNOpPiU7Ll", "gleaming-guppy"],
    ["ses_0ef404a1affe3fi2IhMtYQ9IjL", "windswept-urchin"],
    ["", "anon"],
  ])("%s -> %s", (id, expected) => {
    expect(nickname(id)).toBe(expected);
  });

  test("is a pure function of the id", () => {
    const id = "c80daf7d-2f6f-40cc-8cdf-fae30a5d842a";
    expect(nickname(id)).toBe(nickname(id));
  });
});
