import assert from "node:assert/strict";
import test from "node:test";
import { historyChange, historyChanges } from "./history.ts";

const commit = {
  changes: [
    { change: "added", kind: "text_file", path: "README.md", storage: "inline" },
    {
      change: "added",
      kind: "binary_file",
      path: "attachments/graft-app-state.png",
      storage: "external",
    },
    {
      change: "added",
      kind: "sqlite_database",
      path: "data.sqlite",
      storage: "sqlite_snapshot",
    },
  ],
};

test("history exposes every changed path in a commit", () => {
  assert.deepEqual(
    historyChanges(commit).map((change) => change.path),
    ["README.md", "attachments/graft-app-state.png", "data.sqlite"],
  );
});

test("history selects the requested text, binary, or SQLite change", () => {
  assert.equal(historyChange(commit)?.path, "README.md");
  assert.equal(
    historyChange(commit, "attachments/graft-app-state.png")?.kind,
    "binary_file",
  );
  assert.equal(historyChange(commit, "data.sqlite")?.kind, "sqlite_database");
  assert.equal(historyChange(commit, "missing.txt"), undefined);
});
