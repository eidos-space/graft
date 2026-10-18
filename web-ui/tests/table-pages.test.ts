import assert from "node:assert/strict";
import { test } from "node:test";
import { TablePageCache, viewportPages } from "../src/tablePages.ts";

test("jumping to the end loads only nearby pages, including a partial final page", () => {
  assert.deepEqual(viewportPages(99980, 100000, 100000), [99800, 99900]);
  assert.deepEqual(viewportPages(0, 30, 100000), [0, 100]);
  assert.deepEqual(viewportPages(120, 130, 130), [0, 100]);
  assert.deepEqual(viewportPages(0, 30, 0), []);
});
test("page cache remains bounded and preserves recently viewed pages", () => {
  const cache = new TablePageCache<string>();
  for (let index = 0; index < 32; index++) cache.set(index * 100, String(index));
  assert.equal(cache.get(0), "0");
  cache.set(3200, "32");
  assert.equal(cache.size, 32);
  assert.equal(cache.get(100), undefined);
  assert.equal(cache.get(0), "0");
  cache.clear();
  assert.equal(cache.size, 0);
});
