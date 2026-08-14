"use strict"

const assert = require("node:assert/strict")
const { spawnSync } = require("node:child_process")
const fs = require("node:fs/promises")
const http = require("node:http")
const os = require("node:os")
const path = require("node:path")
const test = require("node:test")

const packageMetadata = require("../package.json")

let DatabaseSync
try {
  ;({ DatabaseSync } = require("node:sqlite"))
} catch (error) {
  if (error?.code !== "ERR_UNKNOWN_BUILTIN_MODULE") throw error
}
const nodeSqliteTest = DatabaseSync
  ? {}
  : { skip: "requires the node:sqlite integration available in Node.js 22+" }

const {
  RepositorySession,
  operationMaterializesWorktree,
  sdkVersion,
} = require("..")

test("exposes ABI-stable SDK metadata and materialization contract", () => {
  assert.equal(sdkVersion(), packageMetadata.version)
  for (const operation of [
    "restore",
    "restorePaths",
    "pull",
    "cloneRepository",
    "applyMerge",
    "setMergePathResult",
    "unresolveMergePath",
    "resolveMergeRow",
    "resolveMergeCell",
    "resolveMergeTable",
    "acceptSemanticMergeResult",
    "writeAndStageTextResult",
    "continueMerge",
    "abortMerge",
  ]) {
    assert.equal(operationMaterializesWorktree(operation), true)
  }
  for (const operation of [
    "init",
    "status",
    "statusIncremental",
    "repositoryMetadata",
    "listRemotes",
    "addAll",
    "stagePaths",
    "recordPathMove",
    "untrackPaths",
    "commit",
    "diff",
    "diffPaths",
    "readPathContent",
    "history",
    "historySummaries",
    "commitDetails",
    "commitChangedPaths",
    "isIgnoredPath",
    "isIgnoredPaths",
    "inventory",
    "configureRemote",
    "push",
    "fetch",
    "planMerge",
    "getMergeStatus",
    "listMergePaths",
    "listMergeConflicts",
    "readMergeVersion",
    "diffMergeSqlite",
    "getMergePolicy",
    "validateMergePolicy",
    "setMergePolicy",
    "stageMergeSqliteResult",
    "prepareSemanticMerge",
    "recordSemanticMergeConflicts",
  ]) {
    assert.equal(operationMaterializesWorktree(operation), false)
  }
})

test("merge policy SDK is versioned, CAS guarded, and cancellable", async () => {
  await withTemporaryDirectory("graft-sdk-merge-policy-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()

    const initial = await session.getMergePolicy()
    assert.equal(initial.policy.version, 1)
    assert.equal(initial.policy.same_row_merge, undefined)
    assert.match(initial.policy_token, /^merge-policy-v1:/)
    assert.equal(initial.active_merge, false)

    const invalid = await session.validateMergePolicy({
      policy: { version: 2 },
    })
    assert.equal(invalid.valid, false)
    assert.equal(invalid.errors[0].key, "version")

    for (const operation of [
      (signal) => session.getMergePolicy({ signal }),
      (signal) =>
        session.validateMergePolicy({ policy: { version: 1 }, signal }),
      (signal) =>
        session.setMergePolicy({
          policy: { version: 1, same_row_merge: true },
          expectedPolicyToken: initial.policy_token,
          signal,
        }),
    ]) {
      const controller = new AbortController()
      controller.abort()
      await assert.rejects(operation(controller.signal), (error) => {
        assert.equal(error.name, "AbortError")
        return true
      })
    }

    const updated = await session.setMergePolicy({
      policy: {
        version: 1,
        same_row_merge: true,
        semantic_keys: { records: ["external_id"] },
        semantic_key_collations: {
          records: { external_id: "nocase" },
        },
        column_resolvers: {
          records: { updated_at: "max_timestamp" },
        },
      },
      expectedPolicyToken: initial.policy_token,
    })
    assert.notEqual(updated.policy_token, initial.policy_token)
    assert.equal(updated.policy.same_row_merge, true)
    assert.equal(
      updated.policy.semantic_key_collations.records.external_id,
      "nocase"
    )
    assert.equal(
      updated.policy.column_resolvers.records.updated_at,
      "max_timestamp"
    )
    await assert.rejects(
      session.setMergePolicy({
        policy: { version: 1 },
        expectedPolicyToken: initial.policy_token,
      }),
      (error) => error.code === "GRAFT_SDK_REPOSITORY_STALE"
    )
    await session.close()
  })
})

test("reads bounded revision path content without materializing", async () => {
  await withTemporaryDirectory("graft-sdk-path-content-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()
    await fs.writeFile(path.join(root, "note.txt"), "one\n")
    await session.addAll()
    const baseline = await session.commit("baseline text")

    const baselineContent = await session.readPathContent({
      path: "note.txt",
      revision: baseline.commit.id,
      maxBytes: 1024,
    })
    assert.equal(baselineContent.revision, baseline.commit.id)
    assert.equal(baselineContent.path, "note.txt")
    assert.equal(baselineContent.kind, "text_file")
    assert.equal(baselineContent.content.state, "utf8")
    assert.equal(baselineContent.content.content, "one\n")
    assert.equal(baselineContent.content.size, 4)
    const absent = await session.readPathContent({
      path: "missing.txt",
      revision: baseline.commit.id,
      maxBytes: 1024,
    })
    assert.equal(absent.kind, null)
    assert.equal(absent.storage, null)
    assert.deepEqual(absent.content, { state: "absent" })

    await fs.writeFile(path.join(root, "note.txt"), "two\n")
    await session.stagePaths({ paths: ["note.txt"] })
    const updated = await session.commit("updated text")
    const before = await session.readPathContent({
      path: "note.txt",
      revision: baseline.commit.id,
      maxBytes: 1024,
    })
    const after = await session.readPathContent({
      path: "note.txt",
      revision: updated.commit.id,
      maxBytes: 1024,
    })
    assert.equal(before.content.state, "utf8")
    assert.equal(before.content.content, "one\n")
    assert.equal(after.content.state, "utf8")
    assert.equal(after.content.content, "two\n")

    const bounded = await session.readPathContent({
      path: "note.txt",
      revision: updated.commit.id,
      maxBytes: 3,
    })
    assert.equal(bounded.content.state, "too_large")
    await assert.rejects(
      session.readPathContent({
        path: "note.txt",
        revision: updated.commit.id,
        maxBytes: 8 * 1024 * 1024 + 1,
      }),
      /between 1 and 8388608/
    )

    const controller = new AbortController()
    const running = Array.from({ length: 24 }, () => session.diff({ rows: true }))
    const queued = session.readPathContent({
      path: "note.txt",
      revision: updated.commit.id,
      maxBytes: 1024,
      signal: controller.signal,
    })
    controller.abort()
    await assert.rejects(queued, (error) => error.name === "AbortError")
    await Promise.all(running)
    assert.equal((await session.repositoryMetadata()).current_head, updated.commit.id)
    await session.close()
  })
})

test("repeated status and diff reuse one native session without a CLI", async () => {
  await withTemporaryDirectory("graft-sdk-hot-", async (root) => {
    const previousCliPath = process.env.GRAFT_CLI_PATH
    process.env.GRAFT_CLI_PATH = path.join(root, "must-not-exist", "graft")
    try {
      const session = await RepositorySession.open(root)
      await session.init()
      await fs.writeFile(path.join(root, "note.txt"), "resident\n")
      await session.addAll()
      await session.commit("initial")

      for (let index = 0; index < 25; index += 1) {
        const status = await session.status()
        assert.equal(status.dirty, false)
        const diff = await session.diff()
        assert.deepEqual(diff.paths, [])
      }
      const history = await session.history({ limit: 1 })
      assert.equal(history.commits.length, 1)
      assert.equal(session.lifecycle, "open")
      await session.close()
    } finally {
      restoreEnvironment("GRAFT_CLI_PATH", previousCliPath)
    }
  })
})

test("records file moves as one rename without re-reading the payload", async () => {
  await withTemporaryDirectory("graft-sdk-move-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()
    await fs.writeFile(path.join(root, "old.txt"), "same payload\n")
    await session.addAll()
    await session.commit("base")

    await fs.mkdir(path.join(root, "folder"))
    await fs.rename(
      path.join(root, "old.txt"),
      path.join(root, "folder", "new.txt")
    )
    const recorded = await session.recordPathMove({
      previousPath: "old.txt",
      path: "folder/new.txt",
    })
    assert.deepEqual(recorded, {
      previous_path: "old.txt",
      path: "folder/new.txt",
      change: "renamed",
      materializes_worktree: false,
    })

    const status = await session.statusIncremental()
    assert.equal(status.status.staged_changes.length, 1)
    assert.equal(status.status.staged_changes[0].change, "renamed")
    assert.equal(status.status.staged_changes[0].previous_path, "old.txt")
    const committed = await session.commit("move")
    const changed = await session.commitChangedPaths({
      revision: committed.commit.id,
    })
    assert.equal(changed.total_changed_paths, 1)
    assert.equal(changed.paths[0].change, "renamed")
    assert.equal(changed.paths[0].previous_path, "old.txt")
    await session.close()
  })
})

test(
  "table-scoped row diff scans and returns only the requested table",
  nodeSqliteTest,
  async () => {
    await withTemporaryDirectory("graft-sdk-table-diff-", async (root) => {
      const databasePath = path.join(root, "space.eidos")
      const database = new DatabaseSync(databasePath)
      database.exec(`
        CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
        CREATE TABLE unrelated (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
        INSERT INTO customers (name) VALUES ('Ada');
        INSERT INTO unrelated (value) VALUES ('unchanged');
      `)
      database.close()

      const session = await RepositorySession.open(root)
      await session.init()
      await session.addAll()
      const baseline = await session.commit("baseline")

      const changed = new DatabaseSync(databasePath)
      changed.exec("INSERT INTO customers (name) VALUES ('Grace')")
      changed.close()
      await session.addAll()
      const updated = await session.commit("update customers")

      const result = await session.diffPaths({
        paths: ["space.eidos"],
        rows: true,
        table: "customers",
        from: baseline.commit.id,
        to: updated.commit.id,
        limit: 1,
      })
      assert.deepEqual(
        result.paths[0].diff.files[0].tables.map(({ name }) => name),
        ["customers"]
      )
      assert.deepEqual(result.paths[0].diff.files[0].telemetry, {
        requested_table: "customers",
        tables_considered: 1,
        tables_scanned: 1,
      })
      assert.equal(result.telemetry.table_filter_fast_path, true)
      assert.equal(result.telemetry.requested_table, "customers")
      assert.equal(result.telemetry.tables_scanned, 1)

      await assert.rejects(
        session.diffPaths({
          paths: ["space.eidos"],
          table: "customers",
          limit: 1,
        }),
        /requires row details/
      )
      await session.close()
    })
  }
)

test(
  "returns SQLite summaries and stable bounded row pages without full payloads",
  nodeSqliteTest,
  async () => {
    await withTemporaryDirectory("graft-sdk-bounded-row-diff-", async (root) => {
      const databasePath = path.join(root, "space.eidos")
      const database = new DatabaseSync(databasePath)
      database.exec("CREATE TABLE records (id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
      database.close()

      const session = await RepositorySession.open(root)
      await session.init()
      await session.addAll()
      const baseline = await session.commit("empty records")

      const changed = new DatabaseSync(databasePath)
      const insert = changed.prepare("INSERT INTO records (value) VALUES (?)")
      changed.exec("BEGIN")
      for (let index = 0; index < 5_000; index += 1) {
        insert.run(`value-${index}`)
      }
      changed.exec("COMMIT")
      changed.close()
      await session.addAll()
      const updated = await session.commit("insert records")

      const summary = await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "summary",
        from: baseline.commit.id,
        to: updated.commit.id,
        limit: 1,
      })
      assert.deepEqual(summary.paths[0].diff.files[0].summaries, [
        { name: "records", inserts: 5_000, deletes: 0, updates: 0 },
      ])
      assert.equal(summary.paths[0].diff.files[0].tables, undefined)
      assert.equal(summary.telemetry.rows_returned, 0)
      assert.equal(summary.telemetry.response_scope, "streaming_rowid")

      const first = await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "rows",
        table: "records",
        rowLimit: 2,
        from: baseline.commit.id,
        to: updated.commit.id,
        limit: 1,
      })
      const firstFile = first.paths[0].diff.files[0]
      assert.deepEqual(
        firstFile.tables[0].changes.map(({ rowid }) => rowid),
        [1, 2]
      )
      assert.equal(firstFile.has_more, true)
      assert.match(firstFile.next_cursor, /^graft-row-v1:/)
      assert.equal(first.telemetry.rows_returned, 2)
      assert.equal(first.telemetry.truncated, true)

      const second = await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "rows",
        table: "records",
        rowLimit: 2,
        rowAfter: firstFile.next_cursor,
        from: baseline.commit.id,
        to: updated.commit.id,
        limit: 1,
      })
      assert.deepEqual(
        second.paths[0].diff.files[0].tables[0].changes.map(({ rowid }) => rowid),
        [3, 4]
      )

      await assert.rejects(
        session.diffSqlitePaths({
          paths: ["space.eidos"],
          mode: "rows",
          table: "records",
          rowLimit: 2,
          rowAfter: "not-a-cursor",
          from: baseline.commit.id,
          to: updated.commit.id,
          limit: 1,
        }),
        /invalid or incompatible row diff cursor/
      )
      await session.close()
    })
  }
)

test(
  "streams Eidos-style STRICT WITHOUT ROWID changes in primary-key order",
  nodeSqliteTest,
  async () => {
    await withTemporaryDirectory("graft-sdk-without-rowid-diff-", async (root) => {
      const databasePath = path.join(root, "space.eidos")
      const database = new DatabaseSync(databasePath)
      database.exec(`
        PRAGMA journal_mode=DELETE;
        CREATE TABLE records (
          id TEXT PRIMARY KEY COLLATE BINARY,
          value TEXT NOT NULL COLLATE NOCASE,
          score INTEGER NOT NULL
        ) STRICT, WITHOUT ROWID;
        CREATE TABLE eidos__meta (
          singleton INTEGER PRIMARY KEY,
          revision INTEGER NOT NULL
        ) STRICT, WITHOUT ROWID;
        INSERT INTO eidos__meta VALUES (1, 1);
      `)
      database.close()

      const session = await RepositorySession.open(root)
      await session.init()
      await session.addAll()
      const empty = await session.commit("empty records")

      const populated = new DatabaseSync(databasePath)
      const insert = populated.prepare(
        "INSERT INTO records (id, value, score) VALUES (?, ?, ?)"
      )
      populated.exec("BEGIN")
      for (let index = 0; index < 5_000; index += 1) {
        const id = index.toString().padStart(8, "0")
        insert.run(id, `value-${id}`, index)
      }
      populated.exec("COMMIT")
      populated.close()
      await session.addAll()
      const baseline = await session.commit("populate records")

      const initialSummary = await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "summary",
        from: empty.commit.id,
        to: baseline.commit.id,
        limit: 1,
      })
      assert.deepEqual(initialSummary.paths[0].diff.files[0].summaries, [
        { name: "records", inserts: 5_000, deletes: 0, updates: 0 },
      ])
      // The inserted table is counted from B-tree headers; only the unchanged singleton metadata
      // row is decoded on both sides.
      assert.ok(initialSummary.telemetry.rows_scanned <= 2)
      assert.equal(
        initialSummary.telemetry.response_scope,
        "streaming_primary_key"
      )

      const changed = new DatabaseSync(databasePath)
      changed.exec(`
        DELETE FROM records WHERE id = '00000001';
        INSERT INTO records VALUES ('00005000', 'inserted', 5000);
      `)
      changed
        .prepare("UPDATE records SET value = ? WHERE id = '00000002'")
        .run("updated".repeat(600))
      changed.close()
      await session.addAll()
      const updated = await session.commit("mutate records")

      const summary = await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "summary",
        from: baseline.commit.id,
        to: updated.commit.id,
        limit: 1,
      })
      assert.deepEqual(summary.paths[0].diff.files[0].summaries, [
        { name: "records", inserts: 1, deletes: 1, updates: 1 },
      ])
      assert.equal(summary.telemetry.response_scope, "streaming_primary_key")

      const first = await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "rows",
        table: "records",
        rowLimit: 2,
        from: baseline.commit.id,
        to: updated.commit.id,
        limit: 1,
      })
      const firstFile = first.paths[0].diff.files[0]
      assert.deepEqual(
        firstFile.tables[0].changes.map(({ op, key }) => [op, key.id]),
        [
          ["delete", "00000001"],
          ["update", "00000002"],
        ]
      )
      assert.equal(firstFile.has_more, true)
      assert.equal(first.telemetry.response_scope, "streaming_primary_key")

      const second = await session.diffSqlitePaths({
        paths: ["space.eidos"],
        mode: "rows",
        table: "records",
        rowLimit: 2,
        rowAfter: firstFile.next_cursor,
        from: baseline.commit.id,
        to: updated.commit.id,
        limit: 1,
      })
      assert.deepEqual(
        second.paths[0].diff.files[0].tables[0].changes.map(({ op, key }) => [
          op,
          key.id,
        ]),
        [["insert", "00005000"]]
      )
      assert.equal(second.paths[0].diff.files[0].has_more, false)
      await session.close()
    })
  }
)

test("classifies UTF-8 codepoints crossing the sniff boundary as text", async () => {
  await withTemporaryDirectory("graft-sdk-utf8-boundary-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()
    const paths = []

    for (const [width, character] of [
      [2, "¢"],
      [3, "中"],
      [4, "😀"],
    ]) {
      for (let bytesBeforeBoundary = 1; bytesBeforeBoundary < width; bytesBeforeBoundary += 1) {
        const relativePath = `utf8-${width}-${bytesBeforeBoundary}.txt`
        paths.push(relativePath)
        await fs.writeFile(
          path.join(root, relativePath),
          Buffer.concat([
            Buffer.alloc(8192 - bytesBeforeBoundary, "a"),
            Buffer.from(`${character}\n`),
          ])
        )
      }
    }

    const untracked = await session.statusIncremental()
    assert.deepEqual(
      untracked.status.paths.map(({ path: relativePath, kind }) => [relativePath, kind]),
      paths.map((relativePath) => [relativePath, "text_file"])
    )

    await session.stagePaths({ paths })
    const committed = await session.commit("UTF-8 boundary")
    const changed = await session.commitChangedPaths({
      revision: committed.commit.id,
      limit: 100,
    })
    assert.deepEqual(
      changed.paths.map(({ path: relativePath, kind }) => [relativePath, kind]),
      paths.map((relativePath) => [relativePath, "text_file"])
    )
    await session.close()
  })
})

test("incremental status exposes a stable session generation", async () => {
  await withTemporaryDirectory("graft-sdk-generation-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()
    const note = path.join(root, "note.txt")
    await fs.writeFile(note, "one\n")
    await session.addAll()
    await session.commit("baseline")

    const first = await session.statusIncremental()
    const hot = await session.statusIncremental()
    assert.equal(first.status.dirty, false)
    assert.equal(hot.generation, first.generation)
    assert.equal(hot.change_token, first.change_token)
    assert.equal(hot.telemetry.status_cache_hit, true)

    await fs.writeFile(note, "two\n")
    const changed = await session.statusIncremental()
    assert.equal(changed.status.dirty, true)
    assert.ok(changed.generation > hot.generation)
    assert.equal(changed.telemetry.status_cache_hit, false)
    assert.equal(typeof changed.telemetry.persistent_snapshot_saved, "boolean")
    await session.close()

    const reopened = await RepositorySession.open(root)
    const persisted = await reopened.statusIncremental()
    assert.equal(persisted.telemetry.persistent_snapshot_hit, true)
    assert.equal(persisted.telemetry.status_cache_hit, true)
    assert.equal(persisted.generation, changed.generation)
    assert.equal(persisted.change_token, changed.change_token)
    await reopened.close()
  })
})

test("metadata and remotes avoid worktree classification and credentials", async () => {
  await withTemporaryDirectory("graft-sdk-metadata-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()
    await fs.writeFile(path.join(root, "note.txt"), "one\n")
    await session.addAll()
    const committed = await session.commit("initial")
    await session.configureRemote({
      name: "origin",
      url: "https://example.invalid/acme/space",
      bearerToken: "in-memory-only",
    })

    const metadata = await session.repositoryMetadata()
    assert.equal(metadata.current_head, committed.commit.id)
    assert.equal(metadata.current_branch, "main")
    assert.equal(metadata.upstream_target, null)
    assert.equal(metadata.telemetry.paths_examined, 0)
    const remotes = await session.listRemotes()
    assert.equal(remotes.telemetry.paths_examined, 0)
    assert.deepEqual(remotes.remotes, [
      {
        name: "origin",
        kind: "http",
        url: "https://example.invalid/acme/space",
      },
    ])
    assert.equal(JSON.stringify(remotes).includes("in-memory-only"), false)

    const controller = new AbortController()
    const inFlight = Array.from({ length: 24 }, () =>
      session.diff({ rows: true })
    )
    const queuedMetadata = session.repositoryMetadata({
      signal: controller.signal,
    })
    controller.abort()
    await assert.rejects(
      queuedMetadata,
      (error) => error.name === "AbortError"
    )
    await Promise.all(inFlight)
    assert.equal((await session.listRemotes()).telemetry.paths_examined, 0)
    await session.close()
  })
})

test("incremental SDK pages history, diffs, ignore inventory, and batch mutations", async () => {
  await withTemporaryDirectory("graft-sdk-incremental-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()
    await fs.mkdir(path.join(root, "node_modules", "pkg"), { recursive: true })
    await fs.writeFile(path.join(root, "node_modules", "pkg", "index.js"), "one\n")
    await fs.writeFile(path.join(root, "note.txt"), "one\n")
    await session.addAll()
    const baselineCommit = await session.commit("baseline")

    await fs.writeFile(path.join(root, ".gitignore"), "node_modules/\n")
    await session.addAll()
    const ignoredCommit = await session.commit("ignore dependencies")
    const history = await session.historySummaries({ limit: 1 })
    assert.equal(history.commits.length, 1)
    assert.equal(history.commits[0].id, ignoredCommit.commit.id)
    assert.equal(history.commits[0].path_counts_complete, true)
    assert.equal(history.telemetry.tree_objects_read, 0)
    assert.equal(history.telemetry.blob_objects_read, 0)
    assert.equal((await session.commitDetails(history.commits[0].id)).id, history.commits[0].id)

    const rootFirstPage = await session.commitChangedPaths({
      revision: baselineCommit.commit.id,
      limit: 1,
    })
    assert.equal(rootFirstPage.parent, null)
    assert.equal(rootFirstPage.total_changed_paths, 2)
    assert.equal(rootFirstPage.paths.length, 1)
    assert.equal(rootFirstPage.has_more, true)
    assert.equal(rootFirstPage.telemetry.blob_objects_read, 0)
    const rootSecondPage = await session.commitChangedPaths({
      revision: baselineCommit.commit.id,
      limit: 1,
      after: rootFirstPage.next_cursor,
    })
    assert.equal(rootSecondPage.paths.length, 1)
    assert.equal(rootSecondPage.has_more, false)
    const rootPaths = [...rootFirstPage.paths, ...rootSecondPage.paths].map(
      ({ path: changedPath }) => changedPath
    )
    const rootDiff = await session.diffPaths({
      paths: rootPaths,
      root: baselineCommit.commit.id,
      limit: 100,
    })
    assert.equal(rootDiff.paths.length, 2)

    const commitPaths = await session.commitChangedPaths({
      revision: ignoredCommit.commit.id,
      limit: 100,
    })
    assert.equal(commitPaths.parent, baselineCommit.commit.id)
    assert.deepEqual(
      commitPaths.paths.map(({ path: changedPath }) => changedPath),
      [".gitignore"]
    )
    const commitDiff = await session.diffPaths({
      paths: commitPaths.paths.map(({ path: changedPath }) => changedPath),
      from: commitPaths.parent,
      to: commitPaths.revision,
      limit: 100,
    })
    assert.equal(commitDiff.paths.length, 1)
    await assert.rejects(
      session.commitChangedPaths({
        revision: ignoredCommit.commit.id,
        limit: 101,
      }),
      /between 1 and 100/
    )
    const historyAbort = new AbortController()
    const historyRunning = Array.from({ length: 24 }, () => session.diff({ rows: true }))
    const queuedHistory = session.commitChangedPaths({
      revision: ignoredCommit.commit.id,
      signal: historyAbort.signal,
    })
    historyAbort.abort()
    await assert.rejects(
      queuedHistory,
      (error) => error.name === "AbortError"
    )
    await Promise.all(historyRunning)
    assert.equal(
      (await session.commitChangedPaths({ revision: ignoredCommit.commit.id })).paths.length,
      1
    )

    const ignored = await session.isIgnoredPath("node_modules/pkg/index.js")
    assert.equal(ignored.is_ignored, true)
    assert.equal(ignored.is_tracked, true)
    assert.equal(ignored.is_directory, false)
    const ignoredBatch = await session.isIgnoredPaths({
      paths: ["node_modules", "node_modules/pkg/index.js", "note.txt"],
    })
    assert.equal(ignoredBatch.paths.length, 3)
    assert.equal(ignoredBatch.paths[0].is_ignored, true)
    assert.equal(ignoredBatch.paths[0].is_directory, true)
    assert.equal(ignoredBatch.paths[0].has_tracked_descendants, true)
    assert.equal(ignoredBatch.paths[0].is_tracked, false)
    assert.equal(ignoredBatch.paths[1].is_ignored, true)
    assert.equal(ignoredBatch.paths[1].is_tracked, true)
    assert.equal(ignoredBatch.paths[2].is_ignored, false)
    await assert.rejects(
      session.isIgnoredPaths({
        paths: Array.from({ length: 1001 }, (_, index) => `query-${index}`),
      }),
      /exceeds 1000/
    )
    const ignoreAbort = new AbortController()
    const ignoreRunning = Array.from({ length: 24 }, () => session.diff({ rows: true }))
    const queuedIgnore = session.isIgnoredPaths({
      paths: ["node_modules"],
      signal: ignoreAbort.signal,
    })
    ignoreAbort.abort()
    await assert.rejects(
      queuedIgnore,
      (error) => error.name === "AbortError"
    )
    await Promise.all(ignoreRunning)
    assert.equal((await session.isIgnoredPaths({ paths: ["node_modules"] })).paths.length, 1)
    const inventory = await session.inventory({
      kind: "tracked_ignored",
      limit: 10,
    })
    assert.deepEqual(
      inventory.items.map((item) => item.path),
      ["node_modules/pkg/index.js"]
    )
    assert.equal(inventory.migration.ignored_rules_do_not_untrack, true)
    assert.equal(inventory.telemetry.inventory_cache_hit, false)
    const hotInventory = await session.inventory({
      kind: "tracked_ignored",
      limit: 10,
    })
    assert.equal(hotInventory.telemetry.inventory_cache_hit, true)
    assert.equal(hotInventory.telemetry.paths_examined, 0)

    await fs.writeFile(path.join(root, "note.txt"), "two\n")
    await fs.writeFile(path.join(root, "node_modules", "pkg", "index.js"), "two\n")
    const firstDiffPage = await session.diffPaths({
      paths: ["note.txt", "node_modules/pkg/index.js"],
      limit: 1,
    })
    assert.equal(firstDiffPage.paths.length, 1)
    assert.equal(firstDiffPage.has_more, true)
    assert.equal(firstDiffPage.telemetry.path_filter_fast_path, true)
    assert.equal(firstDiffPage.telemetry.full_tree_paths_hydrated, 0)
    const secondDiffPage = await session.diffPaths({
      paths: ["note.txt", "node_modules/pkg/index.js"],
      limit: 1,
      after: firstDiffPage.next_cursor,
    })
    assert.equal(secondDiffPage.paths.length, 1)
    assert.equal(secondDiffPage.has_more, false)

    const expectedHead = (await session.status()).current_head
    const staged = await session.stagePaths({
      paths: ["note.txt", "node_modules/pkg/index.js"],
      expectedHead,
    })
    assert.equal(staged.paths.length, 2)
    assert.equal(staged.materializes_worktree, false)
    const restored = await session.restorePaths({
      source: "HEAD",
      expectedHead,
      paths: ["note.txt", "node_modules/pkg/index.js"],
    })
    assert.equal(restored.paths.length, 2)
    assert.equal(restored.materializes_worktree, true)
    assert.equal(await fs.readFile(path.join(root, "note.txt"), "utf8"), "one\n")

    await assert.rejects(
      session.untrackPaths({ paths: ["node_modules"], expectedHead }),
      /directory/
    )
    await assert.rejects(
      session.untrackPaths({ paths: Array.from({ length: 1001 }, (_, index) => `p-${index}`) }),
      /exceeds 1000/
    )
    await assert.rejects(
      session.untrackPaths({ paths: ["node_modules/pkg/index.js"], expectedHead: "deadbeef" }),
      /HEAD changed/
    )
    const untrackAbort = new AbortController()
    const untrackRunning = Array.from({ length: 24 }, () => session.diff({ rows: true }))
    const queuedUntrack = session.untrackPaths({
      paths: ["node_modules/pkg/index.js"],
      expectedHead,
      signal: untrackAbort.signal,
    })
    untrackAbort.abort()
    await assert.rejects(
      queuedUntrack,
      (error) => error.name === "AbortError"
    )
    await Promise.all(untrackRunning)
    const untracked = await session.untrackPaths({
      paths: ["node_modules/pkg/index.js"],
      expectedHead,
    })
    assert.equal(untracked.paths.length, 1)
    assert.equal(untracked.paths[0].path, "node_modules/pkg/index.js")
    assert.equal(untracked.materializes_worktree, false)
    assert.equal(
      await fs.readFile(path.join(root, "node_modules", "pkg", "index.js"), "utf8"),
      "one\n"
    )
    await session.addAll()
    const ignoredAfterUntrack = await session.isIgnoredPath("node_modules/pkg/index.js")
    assert.equal(ignoredAfterUntrack.is_ignored, true)
    assert.equal(ignoredAfterUntrack.is_tracked, false)
    assert.deepEqual(
      (
        await session.inventory({
          kind: "tracked_ignored",
          limit: 10,
        })
      ).items,
      []
    )
    await session.close()
  })
})

test(
  "adds, commits, pushes, and clones a multi-file Space",
  nodeSqliteTest,
  async () => {
    await withTemporaryDirectory("graft-sdk-space-", async (temporaryRoot) => {
      const source = path.join(temporaryRoot, "source")
      const remote = path.join(temporaryRoot, "remote")
      const clone = path.join(temporaryRoot, "clone")
      await Promise.all([
        fs.mkdir(source),
        fs.mkdir(remote),
        fs.mkdir(clone),
      ])

      createDatabase(path.join(source, "project.eidos"), "projects", [
        ["Alpha"],
      ])
      createDatabase(path.join(source, "crm.eidos"), "contacts", [["Ada"]])
      await fs.writeFile(path.join(source, "notes.txt"), "whole Space\n")

      const sourceSession = await RepositorySession.open(source)
      await sourceSession.init()
      await sourceSession.addAll()
      const committed = await sourceSession.commit("multi-file baseline")
      assert.equal(typeof committed.current_head, "string")

      await sourceSession.configureRemote({
        name: "origin",
        url: `fs://${remote}`,
        upstreamBranch: "main",
      })
      const pushed = await sourceSession.push()
      assert.equal(pushed.operation, "push")

      const cloneSession = await RepositorySession.open(clone)
      const cloned = await cloneSession.cloneRepository({
        remoteUrl: `fs://${remote}`,
      })
      assert.equal(cloned.operation, "clone")
      assert.equal((await cloneSession.status()).dirty, false)

      assert.deepEqual(
        (await fs.readdir(clone))
          .filter((entry) => entry !== ".graft")
          .sort(),
        ["crm.eidos", "notes.txt", "project.eidos"]
      )
      assert.equal(
        readCount(path.join(clone, "project.eidos"), "projects"),
        1
      )
      assert.equal(readCount(path.join(clone, "crm.eidos"), "contacts"), 1)
      assert.equal(
        await fs.readFile(path.join(clone, "notes.txt"), "utf8"),
        "whole Space\n"
      )

      await fs.writeFile(
        path.join(source, "notes.txt"),
        "whole Space\nupdated\n"
      )
      await sourceSession.addAll()
      await sourceSession.commit("remote update")
      await sourceSession.push()

      const fetched = await cloneSession.fetch()
      assert.equal(fetched.operation, "fetch")
      assert.equal(
        await fs.readFile(path.join(clone, "notes.txt"), "utf8"),
        "whole Space\n"
      )
      const pulled = await cloneSession.pull()
      assert.equal(pulled.operation, "pull")
      assert.equal(
        await fs.readFile(path.join(clone, "notes.txt"), "utf8"),
        "whole Space\nupdated\n"
      )

      await Promise.all([sourceSession.close(), cloneSession.close()])
    })
  }
)

test("plans and resolves a durable Git-like text merge", async () => {
  await withTemporaryDirectory("graft-sdk-merge-", async (temporaryRoot) => {
    const source = path.join(temporaryRoot, "source")
    const remote = path.join(temporaryRoot, "remote")
    const clone = path.join(temporaryRoot, "clone")
    await Promise.all([fs.mkdir(source), fs.mkdir(remote), fs.mkdir(clone)])

    await fs.writeFile(path.join(source, "note.txt"), "base\n")
    const sourceSession = await RepositorySession.open(source)
    await sourceSession.init()
    await sourceSession.addAll()
    await sourceSession.commit("base")
    await sourceSession.configureRemote({
      name: "origin",
      url: `fs://${remote}`,
      upstreamBranch: "main",
    })
    await sourceSession.push()

    const cloneSession = await RepositorySession.open(clone)
    await cloneSession.cloneRepository({ remoteUrl: `fs://${remote}` })

    await fs.writeFile(path.join(source, "note.txt"), "hosted\n")
    await sourceSession.addAll()
    await sourceSession.commit("hosted edit")
    await sourceSession.push()

    await fs.writeFile(path.join(clone, "note.txt"), "local\n")
    await cloneSession.addAll()
    const local = await cloneSession.commit("local edit")
    await cloneSession.fetch()

    const plan = await cloneSession.planMerge({
      revision: "origin/main",
      expectedHead: local.commit.id,
    })
    assert.equal(plan.kind, "three_way")
    assert.deepEqual(plan.conflicted_paths, ["note.txt"])

    const applied = await cloneSession.applyMerge({
      revision: "origin/main",
      expectedHead: local.commit.id,
      planToken: plan.plan_token,
    })
    assert.equal(applied.merge.state, "merging")
    assert.equal(applied.merge.unmerged_count, 1)
    const stateToken = applied.merge.state_token

    const paths = await cloneSession.listMergePaths({
      expectedStateToken: stateToken,
    })
    assert.deepEqual(
      paths.items.map((item) => [item.path, item.state]),
      [["note.txt", "unmerged"]]
    )
    const conflicts = await cloneSession.listMergeConflicts({
      path: "note.txt",
      expectedStateToken: stateToken,
    })
    assert.equal(conflicts.items.length, 1)
    assert.equal(conflicts.items[0].kind, "file")

    const [base, ours, theirs] = await Promise.all(
      ["base", "ours", "theirs"].map((version) =>
        cloneSession.readMergeVersion({
          path: "note.txt",
          version,
          maxBytes: 1024,
          expectedStateToken: stateToken,
        })
      )
    )
    assert.equal(base.content.content, "base\n")
    assert.equal(ours.content.content, "local\n")
    assert.equal(theirs.content.content, "hosted\n")

    await cloneSession.close()
    await cloneSession.open()
    const reopened = await cloneSession.getMergeStatus()
    assert.equal(reopened.state, "merging")
    assert.equal(reopened.state_token, stateToken)

    await assert.rejects(
      cloneSession.writeAndStageTextResult({
        path: "note.txt",
        content: "stale\n",
        expectedStateToken: "stale",
      }),
      (error) => {
        assert.equal(error.code, "GRAFT_SDK_REPOSITORY_STALE")
        return true
      }
    )

    const resolved = await cloneSession.writeAndStageTextResult({
      path: "note.txt",
      content: "resolved\n",
      expectedStateToken: reopened.state_token,
    })
    assert.equal(resolved.merge.state, "merging")
    assert.equal(resolved.merge.unmerged_count, 0)
    const completed = await cloneSession.continueMerge({
      message: "merge hosted",
      expectedStateToken: resolved.merge.state_token,
    })
    assert.equal(completed.merge.state, "none")
    assert.equal(
      await fs.readFile(path.join(clone, "note.txt"), "utf8"),
      "resolved\n"
    )
    const commit = await cloneSession.commitDetails(
      completed.output.commit.id
    )
    assert.equal(commit.parents.length, 2)

    await Promise.all([sourceSession.close(), cloneSession.close()])
  })
})

test(
  "resolves a fetched SQLite row conflict through the public SDK",
  nodeSqliteTest,
  async () => {
    await withTemporaryDirectory("graft-sdk-row-merge-", async (root) => {
      const source = path.join(root, "source")
      const remote = path.join(root, "remote")
      const clone = path.join(root, "clone")
      await Promise.all([fs.mkdir(source), fs.mkdir(remote), fs.mkdir(clone)])
      const sourceDatabasePath = path.join(source, "space.eidos")
      const cloneDatabasePath = path.join(clone, "space.eidos")

      const database = new DatabaseSync(sourceDatabasePath)
      database.exec(`
        CREATE TABLE docs (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
        INSERT INTO docs VALUES (1, 'base'), (2, 'base');
      `)
      database.close()

      const sourceSession = await RepositorySession.open(source)
      await sourceSession.init()
      await sourceSession.addAll()
      await sourceSession.commit("base")
      await sourceSession.configureRemote({
        name: "origin",
        url: `fs://${remote}`,
        upstreamBranch: "main",
      })
      await sourceSession.push()

      const cloneSession = await RepositorySession.open(clone)
      await cloneSession.cloneRepository({ remoteUrl: `fs://${remote}` })

      const hosted = new DatabaseSync(sourceDatabasePath)
      hosted.exec("UPDATE docs SET value = 'hosted'")
      hosted.close()
      await sourceSession.addAll()
      await sourceSession.commit("hosted row")
      await sourceSession.push()

      const local = new DatabaseSync(cloneDatabasePath)
      local.exec("UPDATE docs SET value = 'local'")
      local.close()
      await cloneSession.addAll()
      const localCommit = await cloneSession.commit("local row")
      await cloneSession.fetch()

      const plan = await cloneSession.planMerge({
        revision: "origin/main",
        expectedHead: localCommit.commit.id,
      })
      const applied = await cloneSession.applyMerge({
        revision: "origin/main",
        expectedHead: localCommit.commit.id,
        planToken: plan.plan_token,
      })
      assert.equal(applied.merge.state, "merging")
      assert.deepEqual(applied.worktree_paths, [])
      const conflicts = await cloneSession.listMergeConflicts({
        path: "space.eidos",
        expectedStateToken: applied.merge.state_token,
      })
      assert.equal(conflicts.items.length, 2)
      assert.equal(conflicts.items[0].kind, "row")
      assert.equal(conflicts.items[0].rowid, 1)

      const mergeDiff = await cloneSession.diffMergeSqlite({
        path: "space.eidos",
        from: "base",
        to: "theirs",
        mode: "summary",
        expectedStateToken: applied.merge.state_token,
      })
      assert.equal(mergeDiff.state_token, applied.merge.state_token)
      assert.equal(mergeDiff.from.version, "base")
      assert.equal(mergeDiff.to.version, "theirs")
      assert.equal(mergeDiff.diff.files[0].summaries[0].name, "docs")
      assert.equal(mergeDiff.diff.files[0].summaries[0].updates, 2)
      await assert.rejects(
        cloneSession.diffMergeSqlite({
          path: "space.eidos",
          from: "base",
          to: "ours",
          mode: "summary",
          expectedStateToken: "stale",
        }),
        (error) => {
          assert.equal(error.code, "GRAFT_SDK_REPOSITORY_STALE")
          return true
        }
      )
      const cancelledDiffController = new AbortController()
      const diffBlockers = Array.from({ length: 8 }, () =>
        cloneSession.listMergeConflicts({
          path: "space.eidos",
          expectedStateToken: applied.merge.state_token,
        })
      )
      const cancelledDiff = cloneSession.diffMergeSqlite({
        path: "space.eidos",
        from: "base",
        to: "ours",
        mode: "summary",
        expectedStateToken: applied.merge.state_token,
        signal: cancelledDiffController.signal,
      })
      cancelledDiffController.abort()
      await assert.rejects(
        cancelledDiff,
        (error) => error.name === "AbortError"
      )
      await Promise.all(diffBlockers)

      const controller = new AbortController()
      const blockers = Array.from({ length: 8 }, () =>
        cloneSession.listMergeConflicts({
          path: "space.eidos",
          expectedStateToken: applied.merge.state_token,
        })
      )
      const cancelledTable = cloneSession.resolveMergeTable({
        path: "space.eidos",
        table: "docs",
        result: "ours",
        expectedStateToken: applied.merge.state_token,
        signal: controller.signal,
      })
      controller.abort()
      await assert.rejects(
        cancelledTable,
        (error) => error.name === "AbortError"
      )
      await Promise.all(blockers)
      assert.equal(
        (await cloneSession.getMergeStatus()).state_token,
        applied.merge.state_token
      )

      const firstResolved = await cloneSession.resolveMergeRow({
        path: "space.eidos",
        table: "docs",
        identity: 1,
        result: "theirs",
        expectedStateToken: applied.merge.state_token,
      })
      assert.equal(firstResolved.merge.state, "merging")
      assert.equal(firstResolved.merge.unmerged_count, 1)
      assert.deepEqual(firstResolved.worktree_paths, [])
      const intermediate = new DatabaseSync(cloneDatabasePath, {
        readOnly: true,
      })
      assert.equal(
        intermediate.prepare("SELECT value FROM docs WHERE id = 1").get()
          .value,
        "local"
      )
      intermediate.close()

      await cloneSession.close()
      await cloneSession.open()
      const reopened = await cloneSession.getMergeStatus()
      const resolvedConflicts = await cloneSession.listMergeConflicts({
        path: "space.eidos",
        expectedStateToken: reopened.state_token,
      })
      assert.equal(resolvedConflicts.items[0].status, "resolved")
      assert.equal(resolvedConflicts.items[0].resolution, "theirs")
      assert.equal(resolvedConflicts.items[1].status, "unresolved")

      const resolved = await cloneSession.resolveMergeRow({
        path: "space.eidos",
        table: "docs",
        identity: 2,
        result: "theirs",
        expectedStateToken: reopened.state_token,
      })
      assert.equal(resolved.merge.state, "merging")
      assert.equal(resolved.merge.unmerged_count, 0)
      assert.deepEqual(resolved.worktree_paths, ["space.eidos"])
      const materialized = new DatabaseSync(cloneDatabasePath, {
        readOnly: true,
      })
      assert.deepEqual(
        materialized
          .prepare("SELECT value FROM docs ORDER BY id")
          .all()
          .map((row) => row.value),
        ["hosted", "hosted"]
      )
      materialized.close()

      const unresolved = await cloneSession.unresolveMergePath({
        path: "space.eidos",
        expectedStateToken: resolved.merge.state_token,
      })
      assert.equal(unresolved.merge.unmerged_count, 1)
      assert.deepEqual(unresolved.worktree_paths, ["space.eidos"])
      const resetConflicts = await cloneSession.listMergeConflicts({
        path: "space.eidos",
        expectedStateToken: unresolved.merge.state_token,
      })
      assert.equal(resetConflicts.items[0].status, "unresolved")

      const tableResolved = await cloneSession.resolveMergeTable({
        path: "space.eidos",
        table: "docs",
        result: "theirs",
        expectedStateToken: unresolved.merge.state_token,
      })
      assert.equal(tableResolved.merge.unmerged_count, 0)
      assert.deepEqual(tableResolved.worktree_paths, ["space.eidos"])
      const completed = await cloneSession.continueMerge({
        message: "merge hosted row",
        expectedStateToken: tableResolved.merge.state_token,
      })
      assert.equal(completed.merge.state, "none")
      assert.deepEqual(completed.worktree_paths, ["space.eidos"])
      await assert.rejects(
        cloneSession.listMergeConflicts({
          path: "space.eidos",
          expectedStateToken: tableResolved.merge.state_token,
        }),
        /no merge in progress/
      )

      const merged = new DatabaseSync(cloneDatabasePath, { readOnly: true })
      assert.equal(
        merged.prepare("SELECT value FROM docs WHERE id = 1").get().value,
        "hosted"
      )
      merged.close()
      await Promise.all([sourceSession.close(), cloneSession.close()])
    })
  }
)

test("reports external writer lock and recovers after close", async () => {
  await withTemporaryDirectory("graft-sdk-lock-", async (root) => {
    const first = await RepositorySession.open(root)
    await first.init()

    const second = new RepositorySession(root)
    await assert.rejects(second.open(), (error) => {
      assert.equal(error.code, "GRAFT_SDK_REPOSITORY_BUSY")
      return true
    })

    await first.close()
    assert.equal(await second.open(), "open")
    assert.equal((await second.status()).dirty, false)
    await second.close()
  })
})

test("recovers the repository lock after a utility-process crash", async () => {
  await withTemporaryDirectory("graft-sdk-crash-", async (root) => {
    const packageRoot = path.resolve(__dirname, "..")
    const child = spawnSync(
      process.execPath,
      [
        "-e",
        `
          const { RepositorySession } = require(process.argv[1])
          RepositorySession.open(process.argv[2])
            .then((session) => session.init())
            .then(() => process.exit(0))
            .catch((error) => {
              console.error(error)
              process.exit(1)
            })
        `,
        packageRoot,
        root,
      ],
      { encoding: "utf8" }
    )
    assert.equal(child.status, 0, child.stderr)

    const recovered = await RepositorySession.open(root)
    assert.equal((await recovered.status()).dirty, false)
    await recovered.close()
  })
})

test("keeps HTTP credentials in memory and redacts command errors", async () => {
  await withTemporaryDirectory("graft-sdk-credential-", async (root) => {
    const secret = "node-sdk-secret"
    const session = await RepositorySession.open(root)
    await session.init()
    await session.configureRemote({
      name: "origin",
      url: "graft+http://127.0.0.1:1/org/repo",
      bearerToken: secret,
    })

    const config = await fs.readFile(
      path.join(root, ".graft", "config.toml"),
      "utf8"
    )
    assert.equal(config.includes(secret), false)
    assert.equal(config.includes("GRAFT_REMOTE_TOKEN"), false)
    await assert.rejects(session.push({ remote: "origin" }), (error) => {
      assert.equal(error.message.includes(secret), false)
      return true
    })
    await session.close()
  })
})

test("reports real HTTP response bytes through the JavaScript progress callback", async () => {
  await withTemporaryDirectory("graft-sdk-http-progress-", async (root) => {
    const body = Buffer.from("invalid-remote-head\n")
    const server = http.createServer((_request, response) => {
      response.writeHead(200, {
        "content-length": body.length,
        "graft-protocol": "1",
      })
      response.end(body)
    })
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve))
    const address = server.address()
    assert.ok(address && typeof address !== "string")
    const session = await RepositorySession.open(root)
    const progress = []

    try {
      await session.init()
      await session.configureRemote({
        name: "origin",
        url: `graft+http://127.0.0.1:${address.port}/org/repository`,
        upstreamBranch: "main",
      })
      await assert.rejects(
        session.fetch({ onProgress: (event) => progress.push(event) })
      )

      assert.ok(
        progress.some(
          (event) =>
            event.direction === "download" &&
            event.transferredBytes >= body.length &&
            event.totalBytes >= body.length
        ),
        JSON.stringify(progress)
      )
    } finally {
      await session.close()
      server.closeAllConnections()
      await new Promise((resolve, reject) => {
        server.close((error) => (error ? reject(error) : resolve()))
      })
    }
  })
})

test(
  "commit preserves an open SQLite worktree file identity",
  nodeSqliteTest,
  async () => {
    await withTemporaryDirectory("graft-sdk-commit-identity-", async (root) => {
      const databasePath = path.join(root, "records.eidos")
      createDatabase(databasePath, "records", [["before"]])
      const applicationDatabase = new DatabaseSync(databasePath)
      applicationDatabase.exec("PRAGMA journal_mode=WAL")
      const before = await fs.stat(databasePath)

      const session = await RepositorySession.open(root)
      await session.init()
      const afterInit = await fs.stat(databasePath)
      await session.stagePaths({ paths: ["records.eidos"] })
      const afterStage = await fs.stat(databasePath)
      const committed = await session.commit("Enable Space versioning")
      const afterCommit = await fs.stat(databasePath)

      assert.equal(afterInit.dev, before.dev)
      assert.equal(afterInit.ino, before.ino)
      assert.equal(afterStage.dev, before.dev)
      assert.equal(afterStage.ino, before.ino)
      assert.equal(afterCommit.dev, before.dev)
      assert.equal(afterCommit.ino, before.ino)
      assert.deepEqual(committed.materialized ?? [], [])

      applicationDatabase.exec(
        "INSERT INTO records (name) VALUES ('after')"
      )
      assert.equal(readCount(databasePath, "records"), 2)
      assert.equal((await session.status()).dirty, true)

      applicationDatabase.close()
      await session.close()
    })
  }
)

test(
  "keeps non-materializing calls safe with an app DB handle and restores after close",
  nodeSqliteTest,
  async () => {
    await withTemporaryDirectory("graft-sdk-handle-", async (root) => {
      const databasePath = path.join(root, "project.eidos")
      createDatabase(databasePath, "projects", [["Alpha"]])

      const session = await RepositorySession.open(root)
      await session.init()
      await session.addAll()
      await session.commit("baseline")

      const applicationDatabase = new DatabaseSync(databasePath)
      assert.equal((await session.status()).dirty, false)
      assert.deepEqual((await session.diff({ rows: true })).paths, [])
      applicationDatabase.exec(
        "INSERT INTO projects (name) VALUES ('Beta')"
      )
      assert.equal((await session.status()).dirty, true)
      const expectedHead = (await session.status()).current_head

      applicationDatabase.close()
      await session.restore({
        source: "HEAD",
        expectedHead,
        path: "project.eidos",
      })

      const reopenedDatabase = new DatabaseSync(databasePath, {
        readOnly: true,
      })
      reopenedDatabase.close()
      assert.equal(readCount(databasePath, "projects"), 1)
      assert.equal((await session.status()).dirty, false)
      await session.close()
    })
  }
)

test("AbortSignal cancels queued work and leaves the session usable", async () => {
  await withTemporaryDirectory("graft-sdk-abort-", async (root) => {
    const session = await RepositorySession.open(root)
    await session.init()
    await fs.writeFile(path.join(root, "note.txt"), "abort\n")
    await session.addAll()
    await session.commit("baseline")

    const running = Array.from({ length: 24 }, () =>
      session.diff({ rows: true })
    )
    const controller = new AbortController()
    const queued = session.status({ signal: controller.signal })
    controller.abort()
    await assert.rejects(queued, (error) => {
      assert.equal(error.name, "AbortError")
      return true
    })
    await Promise.all(running)
    assert.equal((await session.status()).dirty, false)
    await session.close()
  })
})

function createDatabase(databasePath, table, rows) {
  runSqliteHelper({ action: "create", databasePath, table, rows })
}

function readCount(databasePath, table) {
  return Number(runSqliteHelper({ action: "count", databasePath, table }))
}

function runSqliteHelper(command) {
  assert.match(command.table, /^[A-Za-z_][A-Za-z0-9_]*$/)
  // sqlite3_close_v2 may keep a Windows file handle alive while prepared
  // statements await GC. A utility-process exit gives materialization tests
  // the same deterministic handle release that Electron provides.
  const helper = String.raw`
    const { DatabaseSync } = require("node:sqlite")
    const command = JSON.parse(process.argv[1])
    const database = new DatabaseSync(command.databasePath, {
      readOnly: command.action === "count",
    })
    try {
      if (command.action === "create") {
        database.exec(
          "CREATE TABLE " + command.table +
          " (id INTEGER PRIMARY KEY, name TEXT NOT NULL)"
        )
        const insert = database.prepare(
          "INSERT INTO " + command.table + " (name) VALUES (?)"
        )
        for (const row of command.rows) insert.run(...row)
      } else {
        const count = database
          .prepare("SELECT COUNT(*) AS count FROM " + command.table)
          .get().count
        process.stdout.write(String(count))
      }
    } finally {
      database.close()
    }
  `
  const result = spawnSync(
    process.execPath,
    ["-e", helper, JSON.stringify(command)],
    { encoding: "utf8" }
  )
  assert.equal(
    result.status,
    0,
    `node:sqlite helper failed: ${result.stderr || result.stdout}`
  )
  return result.stdout.trim()
}

async function withTemporaryDirectory(prefix, operation) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), prefix))
  try {
    await operation(directory)
  } finally {
    await fs.rm(directory, { force: true, recursive: true })
  }
}

function restoreEnvironment(name, previous) {
  if (previous === undefined) delete process.env[name]
  else process.env[name] = previous
}
