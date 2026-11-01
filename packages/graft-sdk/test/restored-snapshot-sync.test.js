"use strict"

const assert = require("node:assert/strict")
const fs = require("node:fs/promises")
const os = require("node:os")
const path = require("node:path")
const test = require("node:test")
const { RepositorySession } = require("..")

let DatabaseSync
try {
  ;({ DatabaseSync } = require("node:sqlite"))
} catch (error) {
  if (error?.code !== "ERR_UNKNOWN_BUILTIN_MODULE") throw error
}

test("a restored SQLite checkpoint retains its identity across publication and clone", {
  skip: !DatabaseSync && "requires node:sqlite",
}, async () => {
  const fixture = await fs.mkdtemp(path.join(os.tmpdir(), "graft-restored-snapshot-"))
  const source = path.join(fixture, "source")
  const remote = path.join(fixture, "remote")
  const destination = path.join(fixture, "destination")
  await fs.mkdir(source)
  await fs.mkdir(remote)
  const session = await RepositorySession.open(source)
  const peer = new RepositorySession(destination)
  const databasePath = path.join(source, "notes.eidos")
  try {
    await session.init()
    const database = new DatabaseSync(databasePath)
    database.exec("CREATE TABLE notes(body TEXT); INSERT INTO notes VALUES ('original')")
    database.close()
    await session.addAll()
    await session.commit("original")
    const original = (await session.repositoryMetadata()).current_head
    const edited = new DatabaseSync(databasePath)
    edited.exec("UPDATE notes SET body='edited'; CREATE TABLE later(value TEXT)")
    edited.close()
    await session.addAll()
    await session.commit("edited")
    await session.restorePaths({
      paths: ["notes.eidos"], source: original,
      expectedHead: (await session.repositoryMetadata()).current_head,
    })
    await session.addAll()
    await session.commit("restore original")
    const expected = await session.history({ limit: 10 })
    assert.equal(expected.commits.length, 3)
    await session.close()
    await session.open()
    await session.configureRemote({ name: "origin", url: `fs://${remote}`, upstreamBranch: "main" })
    await session.push()
    await peer.open()
    await peer.cloneRepository({ remoteUrl: `fs://${remote}`, branch: "main" })
    assert.deepEqual((await peer.history({ limit: 10 })).commits, expected.commits)
    assert.equal((await peer.status()).dirty, false)
    const received = new DatabaseSync(path.join(destination, "notes.eidos"), { readOnly: true })
    try {
      assert.equal(received.prepare("SELECT body FROM notes").get().body, "original")
      assert.equal(received.prepare("PRAGMA integrity_check").get().integrity_check, "ok")
      assert.equal(received.prepare("SELECT count(*) AS count FROM sqlite_master WHERE name='later'").get().count, 0)
    } finally { received.close() }
  } finally {
    await peer.close()
    await session.close()
    await fs.rm(fixture, { recursive: true, force: true })
  }
})
