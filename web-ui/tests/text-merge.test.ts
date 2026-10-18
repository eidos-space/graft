import assert from "node:assert/strict";
import { test } from "node:test";
import { prepareTextMerge } from "../src/textMerge.ts";

test("three-way preview preserves independent edits and creates separate conflict blocks", () => {
  const base = "a\nbase-1\nb\nc\nd\nbase-2\ne\n";
  const current = "a\nours-1\nb\nc\nd\nours-2\ne\ncurrent-only\n";
  const incoming = "incoming-only\na\ntheirs-1\nb\nc\nd\ntheirs-2\ne\n";
  const result = prepareTextMerge(base, current, incoming);
  assert.equal(result.conflicts, 2);
  assert.ok(result.contents.startsWith("incoming-only\na\n"));
  assert.ok(result.contents.endsWith("e\ncurrent-only\n"));
  assert.equal(result.contents.match(/^<<<<<<< Current$/gm)?.length, 2);
  assert.ok(result.contents.includes("ours-1\n=======\ntheirs-1"));
});

test("CRLF and missing final newline are retained by the resolution draft", () => {
  const crlf = prepareTextMerge("base\r\n", "ours\r\n", "theirs\r\n");
  assert.ok(crlf.contents.includes("<<<<<<< Current\r\nours\r\n"));
  assert.equal(crlf.trimFinalNewline, false);
  const noNewline = prepareTextMerge("base", "ours", "theirs");
  assert.equal(noNewline.trimFinalNewline, true);
});

test("nonconflicting edits need no manual choices and preserve blank lines", () => {
  const result = prepareTextMerge("a\n\nb\n", "A\n\nb\n", "a\n\nB\n");
  assert.equal(result.conflicts, 0);
  assert.equal(result.contents, "A\n\nB\n");
});
