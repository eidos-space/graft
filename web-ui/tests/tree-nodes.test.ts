import assert from "node:assert/strict";
import { test } from "node:test";
import {
  buildTreeNodes,
  catalogFromData,
  catalogKey,
  tableNodeId,
  removedTreePaths,
  retainCatalog,
} from "../src/treeNodes.ts";

test("transient catalog failures retain table selection until a successful refresh confirms removal", () => {
  const previous = { tables: [{ name: "notes" }], default_table: "notes" };
  const failed = retainCatalog(previous, { tables: [], error: "database is locked" });
  assert.deepEqual(failed.tables, previous.tables);
  assert.equal(failed.default_table, "notes");
  assert.equal(failed.error, "database is locked");
  const recovered = retainCatalog(failed, { tables: [{ name: "other" }] });
  assert.equal(recovered.error, undefined);
  assert.equal(recovered.default_table, undefined);
  assert.deepEqual(recovered.tables, [{ name: "other" }]);
});

test("repository refresh removes virtual children before database containers", () => {
  const parent = "folder/data.sqlite/";
  const child = tableNodeId("folder/data.sqlite", "notes");
  assert.deepEqual(
    removedTreePaths(new Set([parent, child, "README.md"]), new Set(["README.md"])),
    [child, parent, "folder/"],
  );
  assert.deepEqual(removedTreePaths(new Set([parent, child]), new Set([parent])), [child]);
});

test("filtering removes implicit empty directories but preserves matching ancestors", () => {
  const previous = new Set([
    "assets/import/image.png",
    "docs/blog/post.md",
    "docs/project.md",
    "eidos-project.eidos/",
    "eidos-project.eidos/::status::",
  ]);
  const next = new Set([
    "docs/project.md",
    "eidos-project.eidos/",
    "eidos-project.eidos/::status::",
  ]);
  const removed = removedTreePaths(previous, next);
  assert.ok(removed.includes("assets/import/"));
  assert.ok(removed.includes("assets/"));
  assert.ok(removed.includes("docs/blog/"));
  assert.ok(!removed.includes("docs/"));
  assert.ok(!removed.includes("eidos-project.eidos/"));
  assert.ok(removed.indexOf("assets/import/image.png") < removed.indexOf("assets/import/"));
  assert.ok(removed.indexOf("assets/import/") < removed.indexOf("assets/"));
});

test("virtual IDs preserve arbitrary SQL names and never become file selections", () => {
  const names = ["a/b", "a%2Fb", ".", "..", "", "数据表", "a\\b", "::status::"];
  const catalog = { tables: names.map((name) => ({ name })) };
  const result = buildTreeNodes(
    ["folder/data.db", "README.md"],
    new Set(["folder/data.db"]),
    { [catalogKey("folder/data.db", "files")]: catalog },
    {},
    "files",
  );
  assert.equal(new Set(result.paths).size, result.paths.length);
  assert.ok(!result.paths.includes("folder/data.db"));
  assert.ok(result.paths.includes("folder/data.db/"));
  assert.ok(result.paths.includes("README.md"));
  for (const name of names) {
    const id = tableNodeId("folder/data.db", name);
    assert.equal(id.split("/").length, 3);
    assert.deepEqual(result.nodes.get(id)?.selection, { path: "folder/data.db", table: name });
  }
});

test("table catalogs stay scoped to files, working changes and each commit", () => {
  assert.notEqual(catalogKey("data.db", "files"), catalogKey("data.db", "changes"));
  assert.notEqual(catalogKey("data.db", "history", "a"), catalogKey("data.db", "history", "b"));
  const catalogs = { [catalogKey("data.db", "history", "a")]: { tables: [{ name: "old_table" }] } };
  const result = buildTreeNodes(["data.db"], new Set(["data.db"]), catalogs, {}, "history", "b");
  assert.ok(!result.paths.includes(tableNodeId("data.db", "old_table")));
  assert.equal(result.nodes.get("data.db/::status::")?.label, "Loading tables…");
});

test("schema-only and opaque table changes remain reachable", () => {
  const catalog = catalogFromData({
    summaries: [{ name: "notes", inserts: 1, updates: 2, deletes: 0 }],
    schema_changes: [
      { name: "empty_added", entry_type: "table", op: "add", sql: "CREATE TABLE empty_added(id)" },
      { name: "empty_deleted", entry_type: "table", op: "delete", sql: "" },
      {
        name: "index_name",
        entry_type: "index",
        op: "add",
        sql: "CREATE INDEX index_name ON notes(id)",
      },
    ],
    opaque_changes: [{ name: "search", change: "modified", reason: "virtual_table" }],
  });
  assert.deepEqual(
    catalog.tables.map((table) => table.name),
    ["empty_added", "empty_deleted", "notes", "search"],
  );
  assert.equal(catalog.tables[0].schema_changed, true);
  assert.equal(catalog.tables[3].opaque, true);
  assert.equal(catalog.default_table, "notes");
});

test("empty and failed catalogs show distinct inline states", () => {
  const path = "data.db";
  const key = catalogKey(path, "changes");
  const empty = buildTreeNodes([path], new Set([path]), { [key]: { tables: [] } }, {}, "changes");
  assert.equal(empty.nodes.get(`${path}/::status::`)?.label, "No changed tables");
  const failed = buildTreeNodes([path], new Set([path]), {}, { [key]: "locked" }, "changes");
  assert.equal(failed.nodes.get(`${path}/::status::`)?.kind, "message");
  assert.equal(failed.nodes.get(`${path}/::status::`)?.label, "Could not load tables · Retry");
});
