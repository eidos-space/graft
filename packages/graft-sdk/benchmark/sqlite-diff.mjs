import assert from "node:assert/strict"
import fs from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { performance } from "node:perf_hooks"
import { createRequire } from "node:module"

const require = createRequire(import.meta.url)
const { DatabaseSync } = require("node:sqlite")
const { RepositorySession } = require("..")

const rows = positiveInteger(process.env.GRAFT_SQLITE_DIFF_ROWS, 10_000)
const payloadBytes = positiveInteger(
  process.env.GRAFT_SQLITE_DIFF_PAYLOAD_BYTES,
  128
)
const rowLimit = positiveInteger(process.env.GRAFT_SQLITE_DIFF_ROW_LIMIT, 100)
const runLegacy = process.env.GRAFT_SQLITE_DIFF_LEGACY === "1"
const root = await fs.mkdtemp(path.join(os.tmpdir(), "graft-sqlite-diff-"))

try {
  const databasePath = path.join(root, "space.eidos")
  const database = new DatabaseSync(databasePath)
  database.exec(
    "PRAGMA journal_mode=DELETE; CREATE TABLE records (id INTEGER PRIMARY KEY, value TEXT NOT NULL)"
  )
  database.close()

  const session = await RepositorySession.open(root)
  await session.init()
  await session.addAll()
  const baseline = await session.commit("empty records")

  insertRows(databasePath, rows, payloadBytes)
  const databaseBytes = (await fs.stat(databasePath)).size
  await session.addAll()
  const updated = await session.commit("bulk insert records")
  const comparison = {
    paths: ["space.eidos"],
    from: baseline.commit.id,
    to: updated.commit.id,
    limit: 1,
  }

  const summary = await measured(() =>
    session.diffSqlitePaths({ ...comparison, mode: "summary" })
  )
  const firstPage = await measured(() =>
    session.diffSqlitePaths({
      ...comparison,
      mode: "rows",
      table: "records",
      rowLimit,
    })
  )
  const firstFile = firstPage.value.paths[0].diff.files[0]
  assert.deepEqual(summary.value.paths[0].diff.files[0].summaries, [
    { name: "records", inserts: rows, deletes: 0, updates: 0 },
  ])
  assert.equal(firstFile.tables[0].changes.length, rowLimit)
  assert.equal(firstFile.has_more, rows > rowLimit)

  let legacy
  if (runLegacy) {
    legacy = await measured(() =>
      session.diffPaths({ ...comparison, rows: true })
    )
  }
  await session.close()

  process.stdout.write(
    `${JSON.stringify(
      {
        schema: "graft-sdk-sqlite-diff-benchmark-v1",
        fixture: { rows, payload_bytes: payloadBytes, database_bytes: databaseBytes },
        summary: report(summary),
        first_row_page: report(firstPage),
        legacy_full_rows: legacy ? report(legacy) : null,
      },
      null,
      2
    )}\n`
  )
} finally {
  await fs.rm(root, { recursive: true, force: true })
}

function insertRows(databasePath, count, valueBytes) {
  const database = new DatabaseSync(databasePath)
  const insert = database.prepare("INSERT INTO records (value) VALUES (?)")
  const prefix = "x".repeat(Math.max(1, valueBytes - 16))
  database.exec("BEGIN")
  for (let index = 0; index < count; index += 1) {
    insert.run(`${prefix}${index.toString(16).padStart(16, "0")}`)
  }
  database.exec("COMMIT")
  database.close()
}

async function measured(operation) {
  const rssBefore = process.memoryUsage().rss
  const started = performance.now()
  const value = await operation()
  const milliseconds = performance.now() - started
  const rssAfter = process.memoryUsage().rss
  return {
    value,
    milliseconds,
    responseBytes: Buffer.byteLength(JSON.stringify(value)),
    rssDeltaBytes: rssAfter - rssBefore,
  }
}

function report(sample) {
  return {
    milliseconds: sample.milliseconds,
    response_bytes: sample.responseBytes,
    rss_delta_bytes: sample.rssDeltaBytes,
    telemetry: sample.value.telemetry,
  }
}

function positiveInteger(value, fallback) {
  if (value === undefined) return fallback
  const parsed = Number.parseInt(value, 10)
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`expected a positive integer, received ${value}`)
  }
  return parsed
}
