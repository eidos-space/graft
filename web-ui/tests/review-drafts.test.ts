import assert from "node:assert/strict";
import { test } from "node:test";
import { readReviewDraft, writeReviewDraft, clearReviewDraft } from "../src/reviewDrafts.ts";

test("review choices survive file navigation and stay scoped to their merge", () => {
  const storage = new Map<string, string>();
  Object.defineProperty(globalThis, "sessionStorage", {
    configurable: true,
    value: {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => storage.set(key, value),
      removeItem: (key: string) => storage.delete(key),
    },
  });
  writeReviewDraft("merge-a/file-a", { draft: "resolved block", remaining: 1 });
  writeReviewDraft("merge-a/file-b", { "tasks:1:title": "theirs" });
  assert.deepEqual(readReviewDraft("merge-a/file-a", {}), {
    draft: "resolved block",
    remaining: 1,
  });
  assert.deepEqual(readReviewDraft("merge-b/file-a", {}), {});
  clearReviewDraft("merge-a/file-a");
  assert.deepEqual(readReviewDraft("merge-a/file-a", {}), {});
  assert.deepEqual(readReviewDraft("merge-a/file-b", {}), { "tasks:1:title": "theirs" });
});

test("blocked storage still retains drafts until a successful save clears them", () => {
  Object.defineProperty(globalThis, "sessionStorage", {
    configurable: true,
    get: () => {
      throw new Error("blocked");
    },
  });
  writeReviewDraft("private/file", { selected: "ours" });
  assert.deepEqual(readReviewDraft("private/file", {}), { selected: "ours" });
  clearReviewDraft("private/file");
  assert.deepEqual(readReviewDraft("private/file", {}), {});
});
