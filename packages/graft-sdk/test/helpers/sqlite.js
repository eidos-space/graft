"use strict"

const assert = require("node:assert/strict")
const { spawnSync } = require("node:child_process")

let DatabaseSync
try {
  ;({ DatabaseSync } = require("node:sqlite"))
} catch (error) {
  if (error?.code !== "ERR_UNKNOWN_BUILTIN_MODULE") throw error
}

exports.nodeSqliteTest = DatabaseSync
  ? {}
  : { skip: "requires the node:sqlite integration available in Node.js 22+" }

// A subprocess releases all SQLite statement handles before Graft replaces a
// worktree file, including on Windows where sqlite3_close_v2 can retain them.
function run(file, sql, query) {
  const result = spawnSync(process.execPath, ["-e", `
    const { DatabaseSync } = require("node:sqlite")
    const database = new DatabaseSync(process.argv[1], { readOnly: ${query} })
    try {
      ${query ? "process.stdout.write(JSON.stringify(database.prepare(process.argv[2]).all()))" : "database.exec(process.argv[2])"}
    } finally { database.close() }
  `, file, sql], { encoding: "utf8" })
  assert.equal(result.status, 0, result.stderr || String(result.error))
  return query ? JSON.parse(result.stdout) : undefined
}

exports.execSqlite = (file, sql) => run(file, sql, false)
exports.querySqlite = (file, sql) => run(file, sql, true)
