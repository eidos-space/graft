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
  assert.equal(sdkVersion(), "0.1.0")
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
    "addAll",
    "stagePaths",
    "commit",
    "diff",
    "diffPaths",
    "history",
    "historySummaries",
    "commitDetails",
    "isIgnoredPath",
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
    await session.commit("baseline")

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

    const ignored = await session.isIgnoredPath("node_modules/pkg/index.js")
    assert.equal(ignored.is_ignored, true)
    assert.equal(ignored.is_tracked, true)
    const inventory = await session.inventory({
      kind: "tracked_ignored",
      limit: 10,
    })
    assert.deepEqual(
      inventory.items.map((item) => item.path),
      ["node_modules/pkg/index.js"]
    )
    assert.equal(inventory.migration.ignored_rules_do_not_untrack, true)

    await fs.writeFile(path.join(root, "note.txt"), "two\n")
    await fs.writeFile(path.join(root, "node_modules", "pkg", "index.js"), "two\n")
    const firstDiffPage = await session.diffPaths({
      paths: ["note.txt", "node_modules/pkg/index.js"],
      limit: 1,
    })
    assert.equal(firstDiffPage.paths.length, 1)
    assert.equal(firstDiffPage.has_more, true)
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

test("AbortSignal cancels queued work without interrupting an in-flight command", async () => {
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
