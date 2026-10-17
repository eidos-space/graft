import assert from "node:assert/strict";
import { test } from "node:test";
import { createReadQueue } from "../src/databaseReads.ts";

test("database inspections wait for earlier reads and resume after failures", async () => {
  const queue = createReadQueue();
  const calls: string[] = [];
  let finish!: () => void;
  const gate = new Promise<void>((resolve) => {
    finish = resolve;
  });
  const first = queue(async () => {
    calls.push("first");
    await gate;
    throw new Error("read failed");
  });
  const failed = assert.rejects(first, /read failed/);
  const second = queue(async () => {
    calls.push("second");
    return "rows";
  });
  await Promise.resolve();
  assert.deepEqual(calls, ["first"]);
  finish();
  await failed;
  assert.equal(await second, "rows");
  assert.deepEqual(calls, ["first", "second"]);
});
test("obsolete queued reads never start database IO", async () => {
  const queue = createReadQueue();
  let finish!: () => void;
  const gate = new Promise<void>((resolve) => {
    finish = resolve;
  });
  const first = queue(() => gate);
  const controller = new AbortController();
  let started = false;
  const obsolete = queue(async () => {
    started = true;
  }, controller.signal);
  controller.abort();
  const cancelled = assert.rejects(obsolete, { name: "AbortError" });
  finish();
  await first;
  await cancelled;
  assert.equal(started, false);
  assert.equal(await queue(async () => "next"), "next");
});
