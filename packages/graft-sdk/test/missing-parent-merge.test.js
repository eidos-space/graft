const assert = require("node:assert/strict")
const fs = require("node:fs/promises")
const os = require("node:os")
const path = require("node:path")
const test = require("node:test")
const { RepositorySession } = require("..")

for (const result of ["ours", "theirs"]) {
  test(`resolves ${result} after the local parent directory was deleted`, async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "graft-missing-parent-"))
    const source = path.join(root, "source")
    const remote = path.join(root, "remote")
    const clone = path.join(root, "clone")
    const file = "deleted/nested/note.txt"
    let sourceSession
    let cloneSession
    try {
      await Promise.all([source, remote, clone].map((directory) => fs.mkdir(directory)))
      await fs.mkdir(path.dirname(path.join(source, file)), { recursive: true })
      await fs.writeFile(path.join(source, file), "base\n")
      sourceSession = await RepositorySession.open(source)
      await sourceSession.init()
      await sourceSession.addAll()
      await sourceSession.commit("base")
      await sourceSession.configureRemote({ name: "origin", url: `fs://${remote}`, upstreamBranch: "main" })
      await sourceSession.push()
      cloneSession = await RepositorySession.open(clone)
      await cloneSession.cloneRepository({ remoteUrl: `fs://${remote}` })

      await fs.writeFile(path.join(source, file), "incoming\n")
      await sourceSession.addAll()
      await sourceSession.commit("incoming edit")
      await sourceSession.push()
      await fs.rm(path.join(clone, "deleted"), { recursive: true })
      await cloneSession.addAll()
      const local = await cloneSession.commit("local directory deletion")
      await cloneSession.fetch()
      const plan = await cloneSession.planMerge({ revision: "origin/main", expectedHead: local.commit.id })
      const applied = await cloneSession.applyMerge({ revision: "origin/main", expectedHead: local.commit.id, planToken: plan.plan_token })
      assert.equal(applied.merge.unmerged_count, 1)
      const resolved = await cloneSession.setMergePathResult({ path: file, result, expectedStateToken: applied.merge.state_token })
      assert.equal(resolved.merge.unmerged_count, 0)
      if (result === "ours") await assert.rejects(fs.stat(path.join(clone, "deleted")), { code: "ENOENT" })
      else assert.equal(await fs.readFile(path.join(clone, file), "utf8"), "incoming\n")
      await cloneSession.close()
      await cloneSession.open()
      const reopened = await cloneSession.getMergeStatus()
      assert.equal(reopened.unmerged_count, 0)
      assert.equal(reopened.state_token, resolved.merge.state_token)
      const completed = await cloneSession.continueMerge({ message: "resolve deletion versus edit", expectedStateToken: reopened.state_token })
      assert.equal(completed.merge.state, "none")
    } finally {
      await sourceSession?.close()
      await cloneSession?.close()
      await fs.rm(root, { recursive: true, force: true })
    }
  })
}
