import assert from "node:assert/strict"
import fs from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { performance } from "node:perf_hooks"
import { createRequire } from "node:module"

const require = createRequire(import.meta.url)
const { DatabaseSync } = require("node:sqlite")
const configuredSource = process.env.GRAFT_REAL_EIDOS_SOURCE
if (!configuredSource) {
  throw new Error("GRAFT_REAL_EIDOS_SOURCE must point to an Eidos fixture")
}
const source = path.resolve(configuredSource)
const sdkRoot = path.resolve(
  import.meta.dirname,
  process.env.GRAFT_SDK_MODULE ?? ".."
)
const sdkLabel =
  process.env.GRAFT_SDK_LABEL ??
  (process.env.GRAFT_SDK_MODULE ? "external-sdk" : "workspace-sdk")
const { RepositorySession } = require(sdkRoot)
const sdkVersion = require(path.join(sdkRoot, "package.json")).version
const root = await fs.mkdtemp(path.join(os.tmpdir(), "graft-real-eidos-"))

try {
  const target = path.join(root, "Untitled.eidos")
  const copy = await timed(() => fs.copyFile(source, target))
  const sourceBytes = (await fs.stat(target)).size
  const opened = await timed(() => RepositorySession.open(root))
  const session = opened.value
  try {
    const init = await timed(() => session.init())
    const stageInitial = await timed(() => session.addAll())
    const initialCommit = await timed(() => session.commit("real Eidos baseline"))
    const baselineCommit = commitId(initialCommit.value)
    mutateMeta(target)
    const dirtyStatus = await timed(() => session.statusIncremental())
    const summary = await timed(() =>
      session.diffSqlitePaths({ paths: ["Untitled.eidos"], mode: "summary" })
    )
    const rows = await timed(() =>
      session.diffSqlitePaths({
        paths: ["Untitled.eidos"],
        mode: "rows",
        table: "eidos__meta",
        rowLimit: 100,
      })
    )
    const stage = await timed(() =>
      session.stagePaths({ paths: ["Untitled.eidos"] })
    )
    const commit = await timed(() => session.commit("update metadata"))
    const updatedCommit = commitId(commit.value)
    const postCommitStatus = await timed(() => session.statusIncremental())
    mutateViewLayout(target)
    const layoutStatus = await timed(() => session.statusIncremental())
    const layoutSummary = await timed(() =>
      session.diffSqlitePaths({ paths: ["Untitled.eidos"], mode: "summary" })
    )
    const layoutRows = await timed(() =>
      session.diffSqlitePaths({
        paths: ["Untitled.eidos"],
        mode: "rows",
        table: "eidos__views",
        rowLimit: 100,
      })
    )
    const historicalSummary = await timed(() =>
      session.diffSqlitePaths({
        paths: ["Untitled.eidos"],
        from: baselineCommit,
        to: updatedCommit,
        mode: "summary",
      })
    )
    const history = await timed(() => session.historySummaries({ limit: 50 }))
    const report = {
      schema: "graft-sdk-real-eidos-benchmark-v1",
      generated_at: new Date().toISOString(),
      sdk: { module: sdkLabel, version: sdkVersion },
      source: { path: `fixture:${path.basename(source)}`, bytes: sourceBytes },
      environment: {
        platform: process.platform,
        arch: process.arch,
        node: process.version,
        cpu: os.cpus()[0]?.model ?? "unknown",
        memory_bytes: os.totalmem(),
      },
      fixture_copy: metric(copy),
      peak_rss_bytes: process.resourceUsage().maxRSS * 1024,
      operations: {
        session_open: metric(opened),
        init: metric(init),
        stage_initial: metric(stageInitial),
        commit_initial: metric(initialCommit),
        status_dirty: metric(dirtyStatus),
        working_summary: metric(summary),
        working_meta_rows: metric(rows),
        stage_meta_change: metric(stage),
        commit_meta_change: metric(commit),
        status_after_commit: metric(postCommitStatus),
        status_layout_dirty: metric(layoutStatus),
        working_layout_summary: metric(layoutSummary),
        working_layout_rows: metric(layoutRows),
        historical_summary: metric(historicalSummary),
        history_summaries_50: metric(history),
      },
    }
    const output = process.env.GRAFT_PERF_OUTPUT
    if (output) {
      await fs.mkdir(path.dirname(path.resolve(output)), { recursive: true })
      await fs.writeFile(path.resolve(output), `${JSON.stringify(report, null, 2)}\n`)
    } else {
      process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
    }
  } finally {
    await session.close().catch(() => undefined)
  }
} finally {
  await fs.rm(root, { recursive: true, force: true })
}

function mutateMeta(databasePath) {
  const database = new DatabaseSync(databasePath)
  try {
    database.exec(
      "UPDATE eidos__meta SET revision = revision + 1, updated_at = '2026-08-02T12:00:00.000Z' WHERE singleton = 1"
    )
  } finally {
    database.close()
  }
}

function mutateViewLayout(databasePath) {
  const database = new DatabaseSync(databasePath)
  try {
    database
      .prepare(
        "UPDATE eidos__views SET layout_json = layout_json || ?, updated_at = ? WHERE id = (SELECT min(id) FROM eidos__views)"
      )
      .run(" ".repeat(500), "2026-08-03T15:00:00.000Z")
  } finally {
    database.close()
  }
}

function commitId(result) {
  const id = result?.commit?.id
  assert.equal(typeof id, "string")
  return id
}

async function timed(operation) {
  const started = performance.now()
  const value = await operation()
  return { milliseconds: performance.now() - started, value }
}

function metric(measured) {
  const encoded = JSON.stringify(measured.value)
  return {
    milliseconds: Math.round(measured.milliseconds * 1_000) / 1_000,
    response_bytes: encoded === undefined ? 0 : Buffer.byteLength(encoded),
  }
}
