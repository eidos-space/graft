const assert = require("node:assert/strict")
const fs = require("node:fs/promises")
const os = require("node:os")
const path = require("node:path")
const { nodeSqliteTest, execSqlite, querySqlite } = require("./helpers/sqlite")
const test = require("node:test")
const { RepositorySession, operationMaterializesWorktree } = require("..")

test("exports exact immutable merge SQLite versions without changing the merge", nodeSqliteTest, async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "graft-merge-preview-"))
  const source = path.join(root, "source")
  const local = path.join(root, "local")
  const remote = path.join(root, "remote")
  let incoming
  let ours
  const edit = (directory, sql) => {
    execSqlite(path.join(directory, "projects.db"), sql)
  }
  try {
    await Promise.all([source, local, remote].map(directory => fs.mkdir(directory)))
    edit(source, "CREATE TABLE Projects (id INTEGER PRIMARY KEY, Owner TEXT); INSERT INTO Projects VALUES (1, 'Alex')")
    incoming = await RepositorySession.open(source)
    await incoming.init()
    await incoming.addAll()
    await incoming.commit("base")
    await incoming.configureRemote({name: "origin", url: `fs://${remote}`, upstreamBranch: "main"})
    await incoming.push()
    ours = await RepositorySession.open(local)
    await ours.cloneRepository({remoteUrl: `fs://${remote}`})
    edit(source, "ALTER TABLE Projects RENAME COLUMN Owner TO Assignee; UPDATE Projects SET Assignee = 'Avery'")
    await incoming.addAll()
    await incoming.commit("incoming schema")
    await incoming.push()
    edit(local, 'ALTER TABLE Projects RENAME COLUMN Owner TO "Project lead"')
    await ours.addAll()
    const head = await ours.commit("local schema")
    await ours.fetch()
    const plan = await ours.planMerge({revision: "origin/main", expectedHead: head.commit.id})
    const applied = await ours.applyMerge({revision: "origin/main", expectedHead: head.commit.id, planToken: plan.plan_token})
    const token = applied.merge.state_token
    assert.equal(applied.merge.unmerged_count, 1)
    for (const [version, column, value] of [["base", "Owner", "Alex"], ["ours", "Project lead", "Alex"], ["theirs", "Assignee", "Avery"]]) {
      const output = path.join(root, `${version}.db`)
      const result = await ours.exportMergeSqliteVersion({path: "projects.db", version, output, expectedStateToken: token})
      assert.equal(result.state_token, token)
      assert.equal(result.file_path, await fs.realpath(output))
      assert.deepEqual(querySqlite(output, "PRAGMA table_info(Projects)").map(item => item.name), ["id", column])
      assert.equal(querySqlite(output, "SELECT * FROM Projects")[0][column], value)
      assert.equal((await ours.getMergeStatus()).state_token, token)
    }
    for (const output of [path.join(local, "preview.db"), path.join(local, ".graft", "preview.db"), path.join(root, "ours.db")]) {
      await assert.rejects(ours.exportMergeSqliteVersion({path: "projects.db", version: "ours", output, expectedStateToken: token}), /outside|already exists/)
    }
    const absent = await ours.exportMergeSqliteVersion({path: "missing.db", version: "ours", output: path.join(root, "missing.db"), expectedStateToken: token})
    assert.equal(absent.file_path, null)
    await assert.rejects(fs.stat(path.join(root, "missing.db")), {code: "ENOENT"})
    const chosen = await ours.setMergePathResult({path: "projects.db", result: "theirs", expectedStateToken: token})
    await assert.rejects(ours.exportMergeSqliteVersion({path: "projects.db", version: "ours", output: path.join(root, "stale.db"), expectedStateToken: token}), {code: "GRAFT_SDK_REPOSITORY_STALE"})
    assert.equal(chosen.merge.unmerged_count, 0)
    assert.equal(operationMaterializesWorktree("exportMergeSqliteVersion"), false)
  } finally {
    await incoming?.close()
    await ours?.close()
    await fs.rm(root, {recursive: true, force: true})
  }
})
