import assert from "node:assert/strict";
import { test } from "node:test";
import { treeDirectories, expandableDirectories } from "../src/treeDirectories.ts";

test("bulk controls address canonical parents, including collapsed descendants", () => {
  assert.deepEqual(treeDirectories(["a/b/c/file.ts", "a/b/readme.md", "root.ts"]), [
    "a/",
    "a/b/",
    "a/b/c/",
  ]);
});

test("database containers stay distinct from ordinary ancestors and SQL names", () => {
  const directories = treeDirectories([
    "data/app.sqlite/",
    "data/app.sqlite/::table::a%2Fb",
    "data/app.sqlite/::table::a%252Fb",
  ]);
  assert.deepEqual(directories, ["data/", "data/app.sqlite/"]);
});

test("expand folders reveals database parents without disclosing any database", () => {
  const directories = treeDirectories([
    "data/nested/app.sqlite/::table::a%2Fb",
    "data/nested/other.sqlite/",
    "src/deep/file.ts",
  ]);
  const targets = expandableDirectories(
    directories,
    new Set(["data/nested/app.sqlite/", "data/nested/other.sqlite/"]),
  );
  assert.deepEqual(targets, ["src/", "data/", "src/deep/", "data/nested/"]);
  assert.ok(!targets.some((path) => path.endsWith(".sqlite/")));
});
