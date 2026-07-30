"use strict"

const assert = require("node:assert/strict")
const { spawnSync } = require("node:child_process")
const fs = require("node:fs/promises")
const os = require("node:os")
const path = require("node:path")
const test = require("node:test")

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
  assert.equal(sdkVersion(), "0.3.0-rc.0")
  for (const operation of [
    "restore",
    "restorePaths",
    "pull",
    "cloneRepository",
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
    "untrackPaths",
    "commit",
    "diff",
    "diffPaths",
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
  ]) {
    assert.equal(operationMaterializesWorktree(operation), false)
  }
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
