import assert from "node:assert/strict";
import { test } from "node:test";
import { readSessionToken } from "../src/sessionToken.ts";

test("a new URL fragment replaces an old in-tab session after a host restart", () => {
  let stored = "old";
  let cleared = false;
  const token = readSessionToken("old", {
    hash: "#token=new",
    read: () => stored,
    save: (value) => {
      stored = value;
    },
    clearFragment: () => {
      cleared = true;
    },
  });
  assert.equal(token, "new");
  assert.equal(stored, "new");
  assert.equal(cleared, true);
});
test("blocked session storage keeps the new URL usable across reloads", () => {
  let cleared = false;
  assert.equal(
    readSessionToken("old", {
      hash: "#token=new",
      read: () => {
        throw new Error("blocked");
      },
      save: () => {
        throw new Error("quota");
      },
      clearFragment: () => {
        cleared = true;
      },
    }),
    "new",
  );
  assert.equal(cleared, false);
});
test("requests without a fragment reuse tab credentials or the memory fallback", () => {
  const access = { hash: "", read: () => "stored", save: () => {}, clearFragment: () => {} };
  assert.equal(readSessionToken("memory", access), "stored");
  assert.equal(readSessionToken("memory", { ...access, read: () => null }), "memory");
});
