import assert from "node:assert/strict";
import test from "node:test";
import { databaseConflictRows, type DatabaseRowConflict } from "../src/databaseConflictRows.ts";

const table = { name: "tasks", columns: ["id", "title", "status", "notes"], rowid_alias: "id" };
const conflict: DatabaseRowConflict = {
  kind: "row",
  status: "unresolved",
  table: "tasks",
  rowid: 1,
  columns: ["status"],
  base_row: [null, "Task", "todo", "base"],
  ours_row: [null, "Task", "current", "base"],
  theirs_row: [null, "Task", "incoming", "incoming note"],
  cells: [{ column: "status", base: "todo", ours: "current", theirs: "incoming" }],
};
test("preserves one row and the original column order for structured cell conflicts", () => {
  const rows = databaseConflictRows([conflict], table, { "tasks:1:status": "theirs" }, {}, false);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].cells.length, 5);
  assert.equal(rows[0].cells[1].value, 1);
  assert.equal(rows[0].cells[2].value, "Task");
  assert.deepEqual(rows[0].cells[3].conflict, {
    current: "current",
    incoming: "incoming",
    selected: "theirs",
    disabled: false,
  });
  assert.equal(rows[0].cells[4].value, "incoming note");
  assert.equal(rows[0].cells[4].conflict, undefined);
});
test("delete/update conflicts select whole rows through their row header", () => {
  const deletion = { ...conflict, cells: undefined, theirs_row: undefined };
  const rows = databaseConflictRows([deletion], table, {}, { "tasks:1": "theirs" }, false);
  assert.equal(rows[0].cells[0].conflict?.selected, "theirs");
  assert.equal(rows[0].cells[0].conflict?.rowHeader, true);
  assert.equal(rows[0].cells[2].value, undefined);
  assert.equal(rows[0].cells[1].value, 1);
  assert.equal(rows[0].cells[2].conflict, undefined);
});
test("composite primary keys remain ordinary columns and controls disable during save", () => {
  const keyed = { ...conflict, key: { id: "key" }, rowid: undefined };
  const rows = databaseConflictRows([keyed], table, {}, {}, true);
  assert.equal(rows[0].cells[1].value, "key");
  assert.equal(rows[0].cells[0].conflict?.disabled, true);
  assert.equal(rows[0].cells[3].conflict?.disabled, true);
});
