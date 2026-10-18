import assert from "node:assert/strict";
import { test } from "node:test";
import { diffIdentityColumns } from "../src/diffColumns.ts";

test("primary keys use their existing columns without an extra identity column", () => {
  for (const keys of [["_id"], ["tenant", "id"]]) {
    const identity = diffIdentityColumns(keys, undefined, []);
    assert.equal(identity.showRowid, false);
    assert.deepEqual([...identity.primaryKeys], keys);
  }
  assert.equal(diffIdentityColumns(undefined, "id", []).showRowid, false);
});

test("older diffs identify primary keys from row keys; keyless tables show rowid", () => {
  const keyed = diffIdentityColumns(undefined, undefined, [
    { op: "insert", key: { _id: "uuid" }, values: ["uuid"] },
  ]);
  assert.equal(keyed.showRowid, false);
  assert.ok(keyed.primaryKeys.has("_id"));
  assert.equal(
    diffIdentityColumns([], undefined, [{ op: "update", rowid: 7, values: ["new"] }]).showRowid,
    true,
  );
});
